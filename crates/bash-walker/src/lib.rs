//! bash-walker: executes bash-parser's AST through argv-per-command process
//! spawning plus native walker state (cwd, variables, functions, flags) —
//! never `sh -c`. See docs/ast-execution.md for why: the tree is
//! inspectable/permissionable before anything runs, and cwd/env persist
//! across invocations via the state file, which is the fix for the corpus's
//! dominant `cd`-first pattern.

pub mod arith;
pub mod builtins;
pub mod cond;
pub mod expand;
pub mod state;
pub mod walk;

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

pub use state::{load, save, ShellState};
use walk::{Ctx, Exec, Flow, Shared};

/// Run one bash script against the state. Returns (combined stdout+stderr,
/// exit status) — the contract exec_docker.py expects.
pub fn run(src: &str, state: &mut ShellState) -> (String, i32) {
    let mut shared = Shared::default();
    let mut ex = Exec { state, shared: &mut shared };
    let capture = match ex.anon_temp() {
        Ok(f) => f,
        Err(_) => return ("bash-walker: cannot create output buffer".into(), 1),
    };
    let mut read_handle = match capture.try_clone() {
        Ok(h) => h,
        Err(e) => return (format!("bash-walker: {e}"), 1),
    };
    let out = Arc::new(capture);
    let ctx = Ctx { stdin: None, stdout: Arc::clone(&out), stderr: out };

    let status = match walk::run_source(&mut ex, &ctx, src, false) {
        Ok(st) => st,
        Err(Flow::Exit(st)) => st,
        Err(Flow::Return(st)) => st,
        Err(Flow::Break(_)) | Err(Flow::Continue(_)) => 0,
        Err(Flow::Fatal(msg)) | Err(Flow::RedirectFailed(msg)) => {
            ctx.write_err(&format!("bash-walker: {msg}\n"));
            if msg.starts_with("syntax error") {
                2
            } else {
                1
            }
        }
    };

    for path in shared.procsub_temps.drain(..) {
        let _ = std::fs::remove_file(path);
    }
    // Background children are left running on purpose — that is what `&`
    // means; the container owns their lifetime.

    let mut output = String::new();
    let _ = read_handle.seek(SeekFrom::Start(0));
    let _ = read_handle.read_to_string(&mut output);
    (output, status)
}
