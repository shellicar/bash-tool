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

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<PathBuf> = std::env::var("BASH_WALKER_STATE").ok().map(PathBuf::from);
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
            path = Some(PathBuf::from(p));
            args.drain(..2);
        }
    }
    let mut state = match &path {
        Some(p) => bash_walker::state::load_mode(p, mode),
        None => bash_walker::ShellState::default(),
    };

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
        let status = bash_walker::run_streaming(script, &mut state, out, err);
        if let Some(p) = &path {
            let _ = bash_walker::state::save_mode(p, &state, mode);
        }
        std::process::exit(status);
    }

    let (command, direct) = match args.first().map(String::as_str) {
        Some("-c") => match args.get(1) {
            Some(c) => (c.clone(), true),
            None => {
                eprintln!("bash-walker: -c requires a script argument");
                std::process::exit(2);
            }
        },
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

    let (output, returncode) = bash_walker::run(&command, &mut state);
    if let Some(p) = &path {
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
