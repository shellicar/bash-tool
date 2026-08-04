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
    pub umask: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone)]
pub struct ShellState {
    pub vars: HashMap<String, Var>,
    /// Innermost-last stack of `local` scopes; a function call pushes one.
    pub locals: Vec<HashMap<String, Var>>,
    pub funcs: HashMap<String, Command>,
    pub positional: Vec<String>,
    /// `$0`: the script's path when invoked with one, the name given after a
    /// `-c` script, and otherwise the shell's own name.
    pub script_name: String,
    pub flags: Flags,
    pub last_status: i32,
    pub last_background_pid: Option<u32>,
    /// `[[ =~ ]]`'s capture groups: whole match at 0, groups after.
    pub rematch: Vec<String>,
    /// `$PIPESTATUS`: the exit status of each stage of the most recently
    /// executed pipeline (a lone simple command counts as one stage).
    pub pipestatus: Vec<i32>,
    /// The shell's working directory — state threaded to every use site
    /// (spawns, redirects, globs, file tests), never the process cwd.
    /// `cd` is a mutation of this field; nothing ever calls chdir.
    pub cwd: PathBuf,
    /// File-creation mask — same "state, not process" rule as cwd: nothing
    /// ever calls the real `umask(2)` on this process (racy under threaded
    /// pipeline stages). Files the walker creates itself get their mode
    /// computed against this; spawned children get it set via `pre_exec`
    /// in their own, single-threaded post-fork moment.
    pub umask: u32,
}

impl Default for ShellState {
    /// The composition root: the ambient environment, process cwd, and
    /// process umask are read exactly once, here, into plain data. Every
    /// later read goes through the state.
    fn default() -> Self {
        let mut vars: HashMap<String, Var> = std::env::vars()
            .map(|(k, v)| (k, Var { value: v, exported: true }))
            .collect();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        // umask(2) has no read-only mode: it always sets and returns the
        // OLD mask, so reading it means set-then-immediately-restore. Only
        // safe here, at startup, before any other thread exists.
        let umask = unsafe {
            let prev = libc::umask(0o022);
            libc::umask(prev);
            prev as u32
        };
        vars.insert(
            "PWD".to_string(),
            Var { value: cwd.to_string_lossy().into_owned(), exported: true },
        );
        Self {
            vars,
            locals: Vec::new(),
            funcs: HashMap::new(),
            positional: Vec::new(),
            script_name: "bash".to_string(),
            flags: Flags::default(),
            last_status: 0,
            last_background_pid: None,
            rematch: Vec::new(),
            pipestatus: vec![0],
            cwd,
            umask,
        }
    }
}

impl ShellState {
    /// Variable lookup: innermost local scope first, then shell vars (which
    /// include the environment snapshot taken at birth).
    pub fn get_var(&self, name: &str) -> Option<String> {
        for scope in self.locals.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.value.clone());
            }
        }
        self.vars.get(name).map(|v| v.value.clone())
    }

    /// Assignment: an existing `local` in any active scope wins, otherwise
    /// the shell var (keeping its exported flag).
    pub fn set_var(&mut self, name: &str, value: String) {
        for scope in self.locals.iter_mut().rev() {
            if let Some(v) = scope.get_mut(name) {
                v.value = value;
                return;
            }
        }
        let exported = self.vars.get(name).is_some_and(|v| v.exported);
        self.vars.insert(name.to_string(), Var { value, exported });
    }

    pub fn export_var(&mut self, name: &str, value: Option<String>) {
        let value = value
            .or_else(|| self.get_var(name))
            .unwrap_or_default();
        // Marking a local exported must not reach the global of the same name.
        // Writing straight to `vars` meant a function exporting its own local
        // overwrote the caller's variable, and the overwrite outlived the call.
        for scope in self.locals.iter_mut().rev() {
            if let Some(v) = scope.get_mut(name) {
                v.value = value;
                v.exported = true;
                return;
            }
        }
        self.vars.insert(name.to_string(), Var { value, exported: true });
    }

    pub fn unset_var(&mut self, name: &str) {
        for scope in self.locals.iter_mut().rev() {
            if scope.remove(name).is_some() {
                return;
            }
        }
        self.vars.remove(name);
    }

    pub fn declare_local(&mut self, name: &str, value: Option<String>) {
        if let Some(scope) = self.locals.last_mut() {
            scope.insert(
                name.to_string(),
                Var { value: value.unwrap_or_default(), exported: false },
            );
        }
    }

    /// A relative path resolves against the shell's cwd; absolute passes
    /// through.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }

    /// The mode a newly-created regular file gets under this umask, for
    /// files the walker creates itself (redirects) rather than a spawned
    /// program.
    pub fn create_mode(&self) -> u32 {
        0o666 & !self.umask
    }

    /// The environment a child process receives: the exported vars, built
    /// fresh per spawn — nothing leaks in from the (stale) process env.
    pub fn child_env(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(_, v)| v.exported)
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    }
}

/// Logical path normalisation — bash's default `cd` semantics: `.` drops,
/// `..` pops textually (no symlink resolution, no filesystem access).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("/");
                }
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// What crosses invocations. `CwdOnly` matches Claude Code's semantics
/// ("The working directory persists between commands, but shell state does
/// not") — the experiment's persistence mode; `All` also carries variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persist {
    All,
    CwdOnly,
}

pub fn load(path: &Path) -> ShellState {
    load_mode(path, Persist::All)
}

pub fn load_mode(path: &Path, mode: Persist) -> ShellState {
    let mut persisted: PersistedState = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if mode == Persist::CwdOnly {
        persisted.vars.clear();
    }
    let mut state = ShellState::default();
    for (name, var) in persisted.vars {
        state.vars.insert(name, var);
    }
    if let Some(cwd) = persisted.cwd {
        state
            .vars
            .insert("PWD".to_string(), Var { value: cwd.to_string_lossy().into_owned(), exported: true });
        state.cwd = cwd;
    }
    if let Some(umask) = persisted.umask {
        state.umask = umask;
    }
    state
}

pub fn save(path: &Path, state: &ShellState) -> std::io::Result<()> {
    save_mode(path, state, Persist::All)
}

pub fn save_mode(path: &Path, state: &ShellState, mode: Persist) -> std::io::Result<()> {
    let persisted = PersistedState {
        cwd: Some(state.cwd.clone()),
        umask: Some(state.umask),
        vars: if mode == Persist::CwdOnly {
            Default::default()
        } else {
            state.vars.clone()
        },
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&persisted)?)
}
