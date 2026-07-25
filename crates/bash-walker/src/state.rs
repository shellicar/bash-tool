//! Shell state: what a real bash session carries between commands, made
//! explicit so it can survive between separate walker invocations (each tool
//! call is a fresh process via `docker exec` — nothing survives in memory).
//!
//! Persisted: cwd and all variables (exported flag kept). This is the fix
//! for the corpus's dominant finding — 65% of real invocations start with
//! `cd` because the harness resets cwd every call. Persisting all variables
//! (not only exported ones) matches the persistent-session model Claude
//! already knows from Claude Code's shell.
//!
//! Not persisted: functions (session-scoped: defined and called within one
//! invocation, which is how the corpus uses them), shell flags (`set -e`
//! does not leak across tool calls, same as one bash process per call), and
//! positional parameters.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bash_parser::Command;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Var {
    pub value: String,
    pub exported: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub cwd: Option<PathBuf>,
    pub vars: HashMap<String, Var>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    /// `set -e`: stop on an untested failure.
    pub errexit: bool,
    /// `set -x`: trace commands to the output before running them.
    pub xtrace: bool,
    /// `set -u`: expanding an unset variable is an error.
    pub nounset: bool,
    /// `set -o pipefail`: a pipeline fails if any stage fails.
    pub pipefail: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ShellState {
    pub vars: HashMap<String, Var>,
    /// Innermost-last stack of `local` scopes; a function call pushes one.
    pub locals: Vec<HashMap<String, Var>>,
    pub funcs: HashMap<String, Command>,
    pub positional: Vec<String>,
    pub flags: Flags,
    pub last_status: i32,
    pub last_background_pid: Option<u32>,
    /// `[[ =~ ]]`'s capture groups: whole match at 0, groups after.
    pub rematch: Vec<String>,
}

impl ShellState {
    /// Variable lookup: innermost local scope first, then shell vars, then
    /// the process environment (the container's own env — HOME, PATH, ...).
    pub fn get_var(&self, name: &str) -> Option<String> {
        for scope in self.locals.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.value.clone());
            }
        }
        if let Some(v) = self.vars.get(name) {
            return Some(v.value.clone());
        }
        std::env::var(name).ok()
    }

    /// Assignment: an existing `local` in any active scope wins, otherwise
    /// the shell var (keeping its exported flag). An exported var's new
    /// value is pushed into the process env immediately so child PATH
    /// lookups and inheritance see it without per-spawn merging.
    pub fn set_var(&mut self, name: &str, value: String) {
        for scope in self.locals.iter_mut().rev() {
            if let Some(v) = scope.get_mut(name) {
                v.value = value;
                return;
            }
        }
        let exported = self.vars.get(name).is_some_and(|v| v.exported)
            || std::env::var_os(name).is_some();
        if exported {
            // SAFETY: the walker is single-threaded.
            unsafe { std::env::set_var(name, &value) };
        }
        self.vars.insert(name.to_string(), Var { value, exported });
    }

    pub fn export_var(&mut self, name: &str, value: Option<String>) {
        let value = value
            .or_else(|| self.get_var(name))
            .unwrap_or_default();
        // SAFETY: the walker is single-threaded.
        unsafe { std::env::set_var(name, &value) };
        self.vars.insert(name.to_string(), Var { value, exported: true });
    }

    pub fn unset_var(&mut self, name: &str) {
        for scope in self.locals.iter_mut().rev() {
            if scope.remove(name).is_some() {
                return;
            }
        }
        self.vars.remove(name);
        // SAFETY: the walker is single-threaded.
        unsafe { std::env::remove_var(name) };
    }

    pub fn declare_local(&mut self, name: &str, value: Option<String>) {
        if let Some(scope) = self.locals.last_mut() {
            scope.insert(
                name.to_string(),
                Var { value: value.unwrap_or_default(), exported: false },
            );
        }
    }
}

pub fn load(path: &Path) -> ShellState {
    let persisted: PersistedState = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if let Some(cwd) = &persisted.cwd {
        // Restore the working directory for real: the process cwd is the
        // single source of truth during a run (globs, relative redirects,
        // and spawned children all read it), state.cwd only carries it
        // across invocations.
        let _ = std::env::set_current_dir(cwd);
        // SAFETY: called at startup, before any threads exist.
        unsafe { std::env::set_var("PWD", cwd) };
    }
    for (name, var) in &persisted.vars {
        if var.exported {
            // SAFETY: called at startup, before any threads exist.
            unsafe { std::env::set_var(name, &var.value) };
        }
    }
    ShellState { vars: persisted.vars, ..Default::default() }
}

pub fn save(path: &Path, state: &ShellState) -> std::io::Result<()> {
    let persisted = PersistedState {
        cwd: std::env::current_dir().ok(),
        vars: state.vars.clone(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&persisted)?)
}
