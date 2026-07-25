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
//! State file: $BASH_WALKER_STATE, defaulting to $HOME/.bash-walker-state.json.
//! Read before the script runs, written back after — cwd and variables
//! survive between invocations.

use std::io::Read;
use std::path::PathBuf;

fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("BASH_WALKER_STATE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".bash-walker-state.json")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = state_path();
    let mut state = bash_walker::load(&path);

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
    if let Err(e) = bash_walker::save(&path, &state) {
        eprintln!("bash-walker: failed to save state: {e}");
    }

    if direct {
        print!("{output}");
        std::process::exit(returncode);
    }
    let result = serde_json::json!({ "output": output, "returncode": returncode });
    println!("{result}");
}
