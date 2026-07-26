//! bash-walker: executes bash-parser's AST through argv-per-command process
//! spawning plus native walker state (cwd, variables, functions, flags) —
//! never `sh -c`. See docs/ast-execution.md for why: the tree is
//! inspectable/permissionable before anything runs, and cwd/env persist
//! across invocations via the state file, which is the fix for the corpus's
//! dominant `cd`-first pattern.

pub mod arith;
pub mod builtins;
pub mod clock;
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
    run_with_clock(src, state, Arc::new(clock::RealClock::default()))
}

/// Same, with the `time` keyword's time sources injected — tests hand in a
/// scripted clock instead of trusting the real one.
pub fn run_with_clock(
    src: &str,
    state: &mut ShellState,
    clk: Arc<dyn clock::Clock + Send + Sync>,
) -> (String, i32) {
    run_with(src, state, clk, Arc::new(clock::RealEntropy))
}

/// Every non-deterministic source injected — the fully seamed entry point.
pub fn run_with(
    src: &str,
    state: &mut ShellState,
    clk: Arc<dyn clock::Clock + Send + Sync>,
    entropy: Arc<dyn clock::Entropy + Send + Sync>,
) -> (String, i32) {
    let mut shared = Shared { clock: clk, entropy, ..Shared::default() };
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
    let ctx = Ctx {
        stdin: None,
        stdout: Arc::clone(&out),
        stderr: out,
        fds: std::collections::HashMap::new(),
        derived: false,
    };

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

/// A backgrounded compound handed to a child walker: the subtree as the
/// exact AST the parent validated (never re-parsed), plus the full shell
/// state — bash's fork, done by spawn because a thread would die with the
/// parent process where a real background job must not.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct BackgroundJob {
    pub command: bash_parser::Command,
    pub funcs: std::collections::HashMap<String, bash_parser::Command>,
    pub vars: std::collections::HashMap<String, state::Var>,
    pub cwd: std::path::PathBuf,
}

/// Execute a background job payload, streaming to the given handles.
pub fn run_background_job(job: BackgroundJob, stdout: std::fs::File, stderr: std::fs::File) -> i32 {
    let mut state = ShellState::default();
    for (k, v) in job.vars {
        state.vars.insert(k, v);
    }
    state.cwd = job.cwd;
    state.funcs = job.funcs;
    let mut shared = Shared::default();
    let mut ex = Exec { state: &mut state, shared: &mut shared };
    let ctx = Ctx {
        stdin: None,
        stdout: Arc::new(stdout),
        stderr: Arc::new(stderr),
        fds: std::collections::HashMap::new(),
        derived: false,
    };
    let status = match ex.exec(&job.command, &ctx, false) {
        Ok(st) => st,
        Err(Flow::Exit(st)) => st,
        Err(Flow::Return(st)) => st,
        Err(Flow::Break(_)) | Err(Flow::Continue(_)) => 0,
        Err(Flow::Fatal(msg)) | Err(Flow::RedirectFailed(msg)) => {
            ctx.write_err(&format!("bash-walker: {msg}\n"));
            1
        }
    };
    for path in shared.procsub_temps.drain(..) {
        let _ = std::fs::remove_file(path);
    }
    status
}

/// Run with output flowing straight to the given handles instead of a
/// capture buffer — the process-substitution child, where the consumer
/// reads concurrently and buffering would defeat streaming (and SIGPIPE).
pub fn run_streaming(
    src: &str,
    state: &mut ShellState,
    stdout: std::fs::File,
    stderr: std::fs::File,
) -> i32 {
    let mut shared = Shared::default();
    let mut ex = Exec { state, shared: &mut shared };
    let ctx = Ctx {
        stdin: None,
        stdout: Arc::new(stdout),
        stderr: Arc::new(stderr),
        fds: std::collections::HashMap::new(),
        derived: false,
    };
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
    status
}
