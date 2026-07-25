//! Stub entry point — not yet implemented. Contract, fixed now so the
//! surrounding harness integration can be designed against it before the
//! walker logic exists:
//!
//!   - Reads one JSON object on stdin: `{"command": "<bash text>"}` — byte-
//!     identical to the plain bash tool's own schema (main.ts in tool-sea),
//!     since the whole point is Claude never sees a different tool.
//!   - Persistent state (cwd, exported env) lives in a state file next to
//!     the binary inside the container (path TBD), read at start of every
//!     invocation and written back after any state-mutating construct
//!     (`cd`, `export`, bare assignment, ...).
//!   - Writes one JSON object on stdout: `{"output": "...", "returncode": N}`
//!     — matching exec_docker.py's expected shape so the environment-side
//!     integration is a small, mechanical change, not a new pattern.

fn main() {
    eprintln!("bash-walker: not yet implemented");
    std::process::exit(1);
}
