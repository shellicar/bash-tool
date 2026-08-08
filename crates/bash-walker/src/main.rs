//! Entry point, two modes:
//!
//!   - default: one JSON object on stdin `{"command": "<bash text>"}`,
//!     one JSON object on stdout `{"output": "...", "returncode": N}` —
//!     byte-compatible with the plain bash tool's schema and
//!     exec_docker.py's expected result shape, so harness integration is
//!     mechanical.
//!   - `-c '<script>'`: direct mode for humans and tests; raw output on
//!     stdout, status as the exit code.
//!
//! State persistence is a per-arm A/B switch: `--state <path>` (or
//! $BASH_WALKER_STATE) persists cwd+variables across invocations; absent,
//! every invocation is fresh, byte-identical to the baseline's
//! bash-per-call. Default off because persisted cwd under PARALLEL tool
//! calls is a write race (Claude Code exhibits last-finisher-wins) and a
//! permission-gating engine needs a command's paths interpretable without
//! invisible prior state — but whether persistence measurably helps is an
//! open experiment (prompt-told × persistence, 2×2), so both modes are
//! first-class.

use std::io::Read;
use std::path::PathBuf;

/// io::Error's Display carries a " (os error N)" suffix bash never prints.
fn errmsg(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// The first two of the three phases, composed here rather than inside either
/// crate: parse, then inspect. Only a tree that came back with no findings is
/// handed to the walker, so a refused construct anywhere in the script means
/// none of the script runs.
///
/// `Ok(None)` is the empty program, which is a valid one that does nothing.
/// The `Err` carries what to print and the status to leave with.
fn approve(src: &str, origin: &str) -> Result<Option<bash_parser::Command>, (String, i32)> {
    if src.trim().is_empty() {
        return Ok(None);
    }
    let tree = match bash_parser::parse(src) {
        Ok(t) => t,
        Err(e) => return Err((format!("bash-walker: syntax error: {e}\n"), 2)),
    };
    let findings = bash_inspect::inspect(&tree);
    if findings.is_empty() {
        Ok(Some(tree))
    } else {
        Err((bash_inspect::render(&findings, origin), 2))
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut path_state: Option<PathBuf> = std::env::var("BASH_WALKER_STATE").ok().map(PathBuf::from);
    let mut mode = bash_walker::state::Persist::All;
    if let Some(first) = args.first() {
        if first == "--state" || first == "--state-cwd" {
            if first == "--state-cwd" {
                mode = bash_walker::state::Persist::CwdOnly;
            }
            let Some(p) = args.get(1) else {
                eprintln!("bash-walker: {first} requires a file path argument");
                std::process::exit(2);
            };
            path_state = Some(PathBuf::from(p));
            args.drain(..2);
        }
    }
    let mut state = match &path_state {
        Some(p) => bash_walker::state::load_mode(p, mode),
        None => bash_walker::ShellState::default(),
    };

    // Structural question, no execution: how many adjacent trace lines may a
    // comparator accept out of order for this script. It lives here because
    // the machine running a replay has this binary and no Rust toolchain.
    if args.first().is_some_and(|a| a == "--pipeline-width") {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            eprintln!("bash-walker: failed to read script from stdin");
            std::process::exit(2);
        }
        let width = bash_parser::parse(&src)
            .map(|ast| bash_parser::widest_pipeline(&ast))
            .unwrap_or(1);
        println!("{width}");
        return;
    }

    // Background-job child mode: the parent hands the exact AST subtree and
    // shell state on stdin; output streams to the inherited fds. This
    // process IS the background job — a real pid, orphan-safe, like bash's
    // fork.
    if args.first().is_some_and(|a| a == "--ast-stdin") {
        let mut input = String::new();
        if std::io::stdin().read_to_string(&mut input).is_err() {
            eprintln!("bash-walker: failed to read job from stdin");
            std::process::exit(2);
        }
        let job: bash_walker::BackgroundJob = match serde_json::from_str(&input) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("bash-walker: bad job payload: {e}");
                std::process::exit(2);
            }
        };
        let (out, err) = {
            use std::os::fd::FromRawFd;
            // SAFETY: dup yields fresh fds we own.
            unsafe {
                (
                    std::fs::File::from_raw_fd(libc::dup(1)),
                    std::fs::File::from_raw_fd(libc::dup(2)),
                )
            }
        };
        std::process::exit(bash_walker::run_background_job(job, out, err));
    }

    // Process-substitution child mode: open the FIFO ourselves (write-only,
    // blocking until the consumer opens the read side — bash's own dance)
    // and stream output straight through, so early-exit consumers and
    // SIGPIPE behave exactly as under bash.
    if args.first().is_some_and(|a| a == "--stdout-path") {
        let (Some(p), Some(c_flag), Some(script)) = (args.get(1), args.get(2), args.get(3))
        else {
            eprintln!("bash-walker: --stdout-path requires <path> -c <script>");
            std::process::exit(2);
        };
        if c_flag != "-c" {
            eprintln!("bash-walker: --stdout-path requires <path> -c <script>");
            std::process::exit(2);
        }
        let out = match std::fs::OpenOptions::new().write(true).open(p) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("bash-walker: {p}: {e}");
                std::process::exit(1);
            }
        };
        let err = {
            use std::os::fd::FromRawFd;
            // SAFETY: dup(2) yields a fresh fd we own.
            unsafe { std::fs::File::from_raw_fd(libc::dup(2)) }
        };
        let status = match approve(script, "-c") {
            Ok(None) => 0,
            Ok(Some(tree)) => bash_walker::run_tree_streaming(&tree, &mut state, out, err),
            Err((msg, status)) => {
                use std::io::Write;
                let _ = (&err).write_all(msg.as_bytes());
                status
            }
        };
        if let Some(p) = &path_state {
            let _ = bash_walker::state::save_mode(p, &state, mode);
        }
        std::process::exit(status);
    }

    // Invocation forms, matching bash: `-c script [name [args...]]`, a script
    // path with its own arguments, or the JSON-on-stdin form the harness uses.
    // The script-path form exists because that is how a shell is invoked, and
    // bash's own test suite drives `$THIS_SH ./name.tests` that way.
    // Option letters bundle as bash's do, so `-ce` is `-c` plus `set -e`.
    // Bash's own test suite invokes `$THIS_SH -ce 'script'` throughout.
    let mut dash_c = false;
    while let Some(first) = args.first() {
        let Some(letters) = first.strip_prefix('-') else { break };
        if letters.is_empty() || !letters.chars().all(|c| c.is_ascii_alphabetic()) {
            break;
        }
        for c in letters.chars() {
            if c == 'c' {
                dash_c = true;
            } else if let Err(bad) = state.flags.set_letter(c, true) {
                eprintln!("bash-walker: -{bad}: not supported by bash-walker");
                std::process::exit(2);
            }
        }
        args.remove(0);
        if dash_c {
            state.flags.dash_c = true;
            break;
        }
    }

    let (command, direct) = match args.first().map(String::as_str) {
        _ if dash_c => match args.first() {
            Some(c) => {
                // `bash -c script name arg...` sets $0 to name and $1 onward
                // to the rest, so a script can report its own name.
                let c = c.clone();
                if let Some(name) = args.get(1) {
                    state.script_name = name.clone();
                    state.positional = args[2..].to_vec();
                }
                (c, true)
            }
            None => {
                eprintln!("bash-walker: -c requires a script argument");
                std::process::exit(2);
            }
        },
        // A script run by path is a shell, not a tool call: output goes to
        // stdout and diagnostics to stderr, as bash does. The `-c` and JSON
        // modes keep their combined-on-stdout contract, which the replay
        // harness reads.
        Some(path) if !path.starts_with('-') => {
            let src = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("bash-walker: {path}: {}", errmsg(&e));
                    std::process::exit(127);
                }
            };
            state.script_name = path.to_string();
            state.positional = args[1..].to_vec();
            let (out, err) = {
                use std::os::fd::FromRawFd;
                // SAFETY: dup yields fresh fds we own.
                unsafe {
                    (
                        std::fs::File::from_raw_fd(libc::dup(1)),
                        std::fs::File::from_raw_fd(libc::dup(2)),
                    )
                }
            };
            let status = match approve(&src, path) {
                Ok(None) => 0,
                Ok(Some(tree)) => bash_walker::run_tree_streaming(&tree, &mut state, out, err),
                Err((msg, status)) => {
                    use std::io::Write;
                    let _ = (&err).write_all(msg.as_bytes());
                    status
                }
            };
            if let Some(p) = &path_state {
                if let Err(e) = bash_walker::state::save_mode(p, &state, mode) {
                    eprintln!("bash-walker: failed to save state: {e}");
                }
            }
            std::process::exit(status);
        }
        _ => {
            let mut input = String::new();
            if std::io::stdin().read_to_string(&mut input).is_err() {
                eprintln!("bash-walker: failed to read stdin");
                std::process::exit(2);
            }
            let parsed: serde_json::Value = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("bash-walker: stdin is not valid JSON: {e}");
                    std::process::exit(2);
                }
            };
            match parsed.get("command").and_then(|c| c.as_str()) {
                Some(c) => (c.to_string(), false),
                None => {
                    eprintln!("bash-walker: expected {{\"command\": \"...\"}}");
                    std::process::exit(2);
                }
            }
        }
    };

    let (output, returncode) = match approve(&command, "-c") {
        Ok(None) => (String::new(), 0),
        Ok(Some(tree)) => bash_walker::run_tree(&tree, &mut state),
        Err(refusal) => refusal,
    };
    if let Some(p) = &path_state {
        if let Err(e) = bash_walker::state::save_mode(p, &state, mode) {
            eprintln!("bash-walker: failed to save state: {e}");
        }
    }

    if direct {
        print!("{output}");
        std::process::exit(returncode);
    }
    let result = serde_json::json!({ "output": output, "returncode": returncode });
    println!("{result}");
}
