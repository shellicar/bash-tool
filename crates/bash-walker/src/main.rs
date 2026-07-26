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
    if args.first().is_some_and(|a| a == "--state") {
        let Some(p) = args.get(1) else {
            eprintln!("bash-walker: --state requires a file path argument");
            std::process::exit(2);
        };
        path = Some(PathBuf::from(p));
        args.drain(..2);
    }
    let mut state = match &path {
        Some(p) => bash_walker::load(p),
        None => bash_walker::ShellState::default(),
    };

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
        if let Err(e) = bash_walker::save(p, &state) {
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
