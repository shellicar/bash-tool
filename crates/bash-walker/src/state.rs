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

/// What `declare` records about a variable beyond its value.
///
/// A variable can carry attributes with no value at all (`declare -i n`
/// leaves `n` unset but integer), and a `local` scope can carry its own,
/// so attributes are stored in the same map as the variables themselves,
/// under a key beginning with U+0001 — a byte no shell identifier can
/// contain. That is what keeps them scoped correctly for free: the walker
/// pushes and pops those maps around a function call, and anything living
/// beside the variables travels with them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
    pub integer: bool,
    pub readonly: bool,
    pub trace: bool,
    pub exported: bool,
    pub lower: bool,
    pub upper: bool,
}

impl Attrs {
    /// The letters in the order `declare -p` prints them, which is bash's
    /// own attribute table order and not alphabetical.
    pub fn letters(&self) -> String {
        let mut s = String::new();
        for (on, c) in [
            (self.integer, 'i'),
            (self.readonly, 'r'),
            (self.trace, 't'),
            (self.exported, 'x'),
            (self.lower, 'l'),
            (self.upper, 'u'),
        ] {
            if on {
                s.push(c);
            }
        }
        s
    }

    pub fn from_letters(s: &str) -> Self {
        let mut a = Self::default();
        for c in s.chars() {
            a.set(c, true);
        }
        a
    }

    /// One `declare` option letter. `-l` and `-u` are exclusive: bash drops
    /// the other rather than holding both.
    pub fn set(&mut self, letter: char, on: bool) -> bool {
        match letter {
            'i' => self.integer = on,
            'r' => self.readonly = on,
            't' => self.trace = on,
            'x' => self.exported = on,
            'l' => {
                self.lower = on;
                if on {
                    self.upper = false;
                }
            }
            'u' => {
                self.upper = on;
                if on {
                    self.lower = false;
                }
            }
            _ => return false,
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Why an assignment did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignError {
    Readonly,
    /// The variable is `-i` and the value is not an arithmetic expression;
    /// carries bash's own message for the expression.
    Arith(String),
}

fn attr_key(name: &str) -> String {
    format!("\u{1}{name}")
}

fn is_attr_key(key: &str) -> bool {
    key.starts_with('\u{1}')
}

/// How far into the current word `getopts` has read — what makes `-abc`
/// three options. It is stored beside `OPTIND` in whichever scope declares
/// that variable, which is what gives a function with its own `local
/// OPTIND` its own scan, restored when the call returns.
const GETOPTS_KEY: &str = "\u{1}getopts";

fn write_attrs(map: &mut HashMap<String, Var>, name: &str, key: &str, attrs: Attrs) {
    if let Some(v) = map.get_mut(name) {
        v.exported = attrs.exported;
    }
    map.insert(key.to_string(), Var { value: attrs.letters(), exported: false });
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub cwd: Option<PathBuf>,
    pub vars: HashMap<String, Var>,
    pub umask: Option<u32>,
}

/// bash 5.3's `shopt` options and their values in a non-interactive shell,
/// in the order `shopt` lists them.
pub const SHOPT_DEFAULTS: &[(&str, bool)] = &[
    ("array_expand_once", false),
    ("assoc_expand_once", false),
    ("autocd", false),
    ("bash_source_fullpath", false),
    ("cdable_vars", false),
    ("cdspell", false),
    ("checkhash", false),
    ("checkjobs", false),
    ("checkwinsize", true),
    ("cmdhist", true),
    ("compat31", false),
    ("compat32", false),
    ("compat40", false),
    ("compat41", false),
    ("compat42", false),
    ("compat43", false),
    ("compat44", false),
    ("complete_fullquote", true),
    ("direxpand", false),
    ("dirspell", false),
    ("dotglob", false),
    ("execfail", false),
    ("expand_aliases", false),
    ("extdebug", false),
    ("extglob", false),
    ("extquote", true),
    ("failglob", false),
    ("force_fignore", true),
    ("globasciiranges", true),
    ("globskipdots", true),
    ("globstar", false),
    ("gnu_errfmt", false),
    ("histappend", false),
    ("histreedit", false),
    ("histverify", false),
    ("hostcomplete", true),
    ("huponexit", false),
    ("inherit_errexit", false),
    ("interactive_comments", true),
    ("lastpipe", false),
    ("lithist", false),
    ("localvar_inherit", false),
    ("localvar_unset", false),
    ("login_shell", false),
    ("mailwarn", false),
    ("no_empty_cmd_completion", false),
    ("nocaseglob", false),
    ("nocasematch", false),
    ("noexpand_translation", false),
    ("nullglob", false),
    ("patsub_replacement", true),
    ("progcomp", true),
    ("progcomp_alias", false),
    ("promptvars", true),
    ("restricted_shell", false),
    ("shift_verbose", false),
    ("sourcepath", true),
    ("varredir_close", false),
    ("xpg_echo", false),
];

/// The options a non-interactive walker cannot exercise either way: line
/// editing, history, prompts, completion, mail and window size. Changing
/// one changes nothing about how a script runs, so it is recorded and
/// reported back rather than refused. Every other option is refused the
/// moment it is asked for a value the walker does not actually behave as.
pub const INERT_SHOPTS: &[&str] = &[
    "autocd",
    "cdspell",
    "checkjobs",
    "checkwinsize",
    "cmdhist",
    "complete_fullquote",
    "direxpand",
    "dirspell",
    // The alias table cannot exist here — `alias` itself is refused — so
    // whether aliases would be expanded can never be observed.
    "expand_aliases",
    "force_fignore",
    "histappend",
    "histreedit",
    "histverify",
    "hostcomplete",
    "huponexit",
    "lithist",
    "login_shell",
    "mailwarn",
    "no_empty_cmd_completion",
    "progcomp",
    "progcomp_alias",
    "promptvars",
];

/// `set -o` option names, in the order bash lists them.
pub const SET_OPTIONS: &[&str] = &[
    "allexport",
    "braceexpand",
    "emacs",
    "errexit",
    "errtrace",
    "functrace",
    "hashall",
    "histexpand",
    "history",
    "ignoreeof",
    "interactive-comments",
    "keyword",
    "monitor",
    "noclobber",
    "noexec",
    "noglob",
    "nolog",
    "notify",
    "nounset",
    "onecmd",
    "physical",
    "pipefail",
    "posix",
    "privileged",
    "verbose",
    "vi",
    "xtrace",
];

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
    /// Started as `-c`. Not a `set` option, but `$-` reports it alongside them.
    pub dash_c: bool,
    /// `set -m`: job control. Nothing here reads it — the walker has no job
    /// control to turn on — but scripts set it and ask for it back.
    pub monitor: bool,
    /// `set -p`: privileged mode, which only changes what a shell imports
    /// at startup.
    pub privileged: bool,
    /// `set -o history`. A non-interactive shell keeps no history, so this
    /// is a flag scripts carry rather than a behaviour.
    pub history: bool,
    /// `set -o emacs` / `set -o vi`: the line editor, which never runs here.
    pub emacs: bool,
    pub vi: bool,
    /// `set -o ignoreeof`, `set -o notify`, `set -o nolog`: interactive and
    /// history bookkeeping, inert in a script.
    pub ignoreeof: bool,
    pub notify: bool,
    pub nolog: bool,
}

impl Flags {
    /// One `set`-style option letter, as `set` and the command line both take
    /// them. `Err` carries the letter back so each caller can word its own
    /// refusal, since bash's wording differs between the two.
    pub fn set_letter(&mut self, letter: char, on: bool) -> Result<(), char> {
        match letter {
            'e' => self.errexit = on,
            'x' => self.xtrace = on,
            'u' => self.nounset = on,
            'm' => self.monitor = on,
            'p' => self.privileged = on,
            'b' => self.notify = on,
            other => return Err(other),
        }
        Ok(())
    }

    /// One `set -o` option by name. `Ok(())` means the walker's behaviour
    /// now matches what was asked for; `Err(())` means it cannot, and the
    /// caller refuses by name rather than letting the script believe it.
    pub fn set_option(&mut self, name: &str, on: bool) -> Result<(), ()> {
        match name {
            "errexit" => self.errexit = on,
            "xtrace" => self.xtrace = on,
            "nounset" => self.nounset = on,
            "pipefail" => self.pipefail = on,
            "monitor" => self.monitor = on,
            "privileged" => self.privileged = on,
            "history" => self.history = on,
            "emacs" => self.emacs = on,
            "vi" => self.vi = on,
            "ignoreeof" => self.ignoreeof = on,
            "notify" => self.notify = on,
            "nolog" => self.nolog = on,
            // The rest are real behaviour the walker has exactly one of, so
            // asking for the value it already has is fine and asking for the
            // other one is a refusal.
            other => {
                return match Self::fixed_option(other) {
                    Some(v) if v == on => Ok(()),
                    _ => Err(()),
                }
            }
        }
        Ok(())
    }

    /// The value of an option the walker does not implement a switch for.
    fn fixed_option(name: &str) -> Option<bool> {
        match name {
            "braceexpand" | "hashall" | "interactive-comments" => Some(true),
            "allexport" | "errtrace" | "functrace" | "histexpand" | "keyword" | "noclobber"
            | "noexec" | "noglob" | "onecmd" | "physical" | "posix" | "verbose" => Some(false),
            _ => None,
        }
    }

    /// A `set -o` option's current value, or None if there is no such
    /// option.
    pub fn option(&self, name: &str) -> Option<bool> {
        Some(match name {
            "errexit" => self.errexit,
            "xtrace" => self.xtrace,
            "nounset" => self.nounset,
            "pipefail" => self.pipefail,
            "monitor" => self.monitor,
            "privileged" => self.privileged,
            "history" => self.history,
            "emacs" => self.emacs,
            "vi" => self.vi,
            "ignoreeof" => self.ignoreeof,
            "notify" => self.notify,
            "nolog" => self.nolog,
            other => return Self::fixed_option(other),
        })
    }

    /// `$-`, in bash's own option order. `h` (hashall) and `B` (braceexpand)
    /// are always on: bash defaults them on, `set +h`/`set +B` are refusals
    /// here, so no reachable state has them off. `pipefail` has no letter.
    pub fn option_letters(&self) -> String {
        let mut s = String::new();
        for (on, c) in [
            (self.notify, 'b'),
            (self.errexit, 'e'),
            (true, 'h'),
            (self.monitor, 'm'),
            (self.privileged, 'p'),
            (self.nounset, 'u'),
            (self.xtrace, 'x'),
            (true, 'B'),
            (self.dash_c, 'c'),
        ] {
            if on {
                s.push(c);
            }
        }
        s
    }
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
    /// The `shopt` options whose value differs from bash's default. Only
    /// the inert options ever reach here: asking for a value the walker
    /// does not actually behave as is refused by the builtin.
    pub shopts: std::collections::BTreeSet<String>,
    pub last_status: i32,
    /// `hash`: remembered paths for command names, with the hit count bash
    /// prints. Only what `hash -p` puts there — nothing populates it by
    /// running a command, because the lookup is the operating system's.
    pub hashed: std::collections::BTreeMap<String, (String, u32)>,
    /// The directories under the current one, innermost first. `dirs`
    /// prints the current directory ahead of these; it is not stored
    /// twice.
    pub dirstack: Vec<PathBuf>,
    /// Trap actions by condition name, `EXIT` and `ERR` only — an empty
    /// action means the condition is ignored, and a condition with no entry
    /// takes its default. A trap on a real signal is refused by the
    /// builtin, because nothing here installs a handler to deliver it.
    /// Subshells clear this: bash resets a parent's traps in a child.
    pub traps: std::collections::BTreeMap<String, String>,
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
        // bash starts with `declare -i OPTIND="1"`, and scripts read it
        // before ever calling getopts.
        vars.insert("OPTIND".to_string(), Var { value: "1".to_string(), exported: false });
        vars.insert(
            attr_key("OPTIND"),
            Var { value: Attrs { integer: true, ..Attrs::default() }.letters(), exported: false },
        );
        Self {
            vars,
            locals: Vec::new(),
            funcs: HashMap::new(),
            positional: Vec::new(),
            script_name: "bash".to_string(),
            flags: Flags::default(),
            shopts: Default::default(),
            last_status: 0,
            hashed: Default::default(),
            dirstack: Vec::new(),
            traps: Default::default(),
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
    /// include the environment snapshot taken at birth). A scope that
    /// declares the name without a value (`local x`) shadows the outer one
    /// and reads as unset, exactly as under bash.
    pub fn get_var(&self, name: &str) -> Option<String> {
        let key = attr_key(name);
        for scope in self.locals.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.value.clone());
            }
            if scope.contains_key(&key) {
                return None;
            }
        }
        self.vars.get(name).map(|v| v.value.clone())
    }

    /// The attributes in force for `name`, taken from the scope that
    /// declares it. An exported value carries `-x` whether or not anything
    /// ever declared it, which is how the inherited environment prints.
    pub fn get_attrs(&self, name: &str) -> Attrs {
        let key = attr_key(name);
        for scope in self.locals.iter().rev() {
            let declared = scope.get(&key);
            let held = scope.get(name);
            if declared.is_none() && held.is_none() {
                continue;
            }
            let mut a = declared.map(|v| Attrs::from_letters(&v.value)).unwrap_or_default();
            if held.is_some_and(|v| v.exported) {
                a.exported = true;
            }
            return a;
        }
        let mut a = self
            .vars
            .get(&key)
            .map(|v| Attrs::from_letters(&v.value))
            .unwrap_or_default();
        if self.vars.get(name).is_some_and(|v| v.exported) {
            a.exported = true;
        }
        a
    }

    /// One `shopt` option's value, or None if there is no such option.
    pub fn shopt(&self, name: &str) -> Option<bool> {
        let default = SHOPT_DEFAULTS.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)?;
        Some(default ^ self.shopts.contains(name))
    }

    /// The scan position for the `OPTIND` currently in scope.
    pub fn getopts_charpos(&self) -> usize {
        let map = match self.optind_scope() {
            Some(i) => &self.locals[i],
            None => &self.vars,
        };
        map.get(GETOPTS_KEY).and_then(|v| v.value.parse().ok()).unwrap_or(0)
    }

    pub fn set_getopts_charpos(&mut self, charpos: usize) {
        let map = match self.optind_scope() {
            Some(i) => &mut self.locals[i],
            None => &mut self.vars,
        };
        map.insert(
            GETOPTS_KEY.to_string(),
            Var { value: charpos.to_string(), exported: false },
        );
    }

    fn optind_scope(&self) -> Option<usize> {
        let key = attr_key("OPTIND");
        (0..self.locals.len())
            .rev()
            .find(|&i| self.locals[i].contains_key("OPTIND") || self.locals[i].contains_key(&key))
    }

    pub fn set_shopt(&mut self, name: &str, on: bool) {
        let Some(default) = SHOPT_DEFAULTS.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
        else {
            return;
        };
        if default == on {
            self.shopts.remove(name);
        } else {
            self.shopts.insert(name.to_string());
        }
    }

    /// Whether anything here declares the name: a value, or attributes
    /// recorded before any value arrived (`declare -i n`).
    pub fn is_declared(&self, name: &str) -> bool {
        let key = attr_key(name);
        for scope in self.locals.iter().rev() {
            if scope.contains_key(name) || scope.contains_key(&key) {
                return true;
            }
        }
        self.vars.contains_key(name) || self.vars.contains_key(&key)
    }

    /// The attributes the current scope holds for `name`, if that scope is
    /// the one that declares it. A `local` starts from these rather than
    /// from whatever an outer scope says.
    pub fn current_scope_attrs(&self, name: &str) -> Option<Attrs> {
        let scope = self.locals.last()?;
        let key = attr_key(name);
        if !scope.contains_key(name) && !scope.contains_key(&key) {
            return None;
        }
        let mut a = scope.get(&key).map(|v| Attrs::from_letters(&v.value)).unwrap_or_default();
        if scope.get(name).is_some_and(|v| v.exported) {
            a.exported = true;
        }
        Some(a)
    }

    /// Records attributes against the scope that already declares the name,
    /// or globally. `local`-style declaration goes through
    /// `declare_local_attrs` instead, which always uses the current scope.
    pub fn set_attrs(&mut self, name: &str, attrs: Attrs) {
        let key = attr_key(name);
        for i in (0..self.locals.len()).rev() {
            if self.locals[i].contains_key(name) || self.locals[i].contains_key(&key) {
                write_attrs(&mut self.locals[i], name, &key, attrs);
                return;
            }
        }
        let mut globals = std::mem::take(&mut self.vars);
        write_attrs(&mut globals, name, &key, attrs);
        self.vars = globals;
    }

    /// `declare`/`local` inside a function: the attributes, and the name
    /// itself, belong to the current scope even when an outer one has the
    /// same name.
    pub fn declare_local_attrs(&mut self, name: &str, attrs: Attrs) {
        let key = attr_key(name);
        if let Some(scope) = self.locals.last_mut() {
            write_attrs(scope, name, &key, attrs);
        } else {
            self.set_attrs(name, attrs);
        }
    }

    /// Assignment, with the variable's attributes applied: `-i` evaluates
    /// the value as arithmetic, `-l`/`-u` change its case, and `-r` refuses
    /// the write outright.
    pub fn assign(&mut self, name: &str, value: String) -> Result<(), AssignError> {
        let attrs = self.get_attrs(name);
        if attrs.readonly {
            return Err(AssignError::Readonly);
        }
        let value = self.apply_attrs(attrs, value)?;
        self.store(name, value, attrs.exported);
        Ok(())
    }

    /// The infallible form every expansion site uses. A refused write is a
    /// no-op here; the builtins that owe the caller a message use `assign`.
    pub fn set_var(&mut self, name: &str, value: String) {
        let _ = self.assign(name, value);
    }

    fn apply_attrs(&mut self, attrs: Attrs, value: String) -> Result<String, AssignError> {
        if attrs.integer {
            if value.trim().is_empty() {
                return Ok("0".to_string());
            }
            let n = crate::arith::eval(&value, self).map_err(|e| AssignError::Arith(e.0))?;
            return Ok(n.to_string());
        }
        if attrs.lower {
            return Ok(value.to_lowercase());
        }
        if attrs.upper {
            return Ok(value.to_uppercase());
        }
        Ok(value)
    }

    /// Where a value lands: an existing `local` in any active scope wins,
    /// then a scope that declared the name without a value, then the shell
    /// vars (keeping whatever export flag was already there).
    fn store(&mut self, name: &str, value: String, exported: bool) {
        // Assigning OPTIND restarts the scan, which is how a script rewinds
        // getopts and how a function's own `local OPTIND=1` starts a fresh
        // one. getopts itself writes the position back afterwards.
        if name == "OPTIND" {
            self.set_getopts_charpos(0);
        }
        let key = attr_key(name);
        for scope in self.locals.iter_mut().rev() {
            if let Some(v) = scope.get_mut(name) {
                v.value = value;
                v.exported |= exported;
                return;
            }
            if scope.contains_key(&key) {
                scope.insert(name.to_string(), Var { value, exported });
                return;
            }
        }
        let was = self.vars.get(name).is_some_and(|v| v.exported);
        self.vars.insert(name.to_string(), Var { value, exported: was || exported });
    }

    pub fn export_var(&mut self, name: &str, value: Option<String>) {
        let mut attrs = self.get_attrs(name);
        attrs.exported = true;
        self.set_attrs(name, attrs);
        if let Some(v) = value {
            let _ = self.assign(name, v);
        }
    }

    pub fn unset_var(&mut self, name: &str) -> Result<(), AssignError> {
        if self.get_attrs(name).readonly {
            return Err(AssignError::Readonly);
        }
        let key = attr_key(name);
        for scope in self.locals.iter_mut().rev() {
            let had = scope.remove(name).is_some();
            let had_attrs = scope.remove(&key).is_some();
            if had || had_attrs {
                return Ok(());
            }
        }
        self.vars.remove(name);
        self.vars.remove(&key);
        Ok(())
    }

    /// A new variable in the current scope. Only the export attribute is
    /// inherited from the variable it shadows: bash's `local` starts clean
    /// otherwise, so a global `-i` does not make the local arithmetic.
    pub fn declare_local(&mut self, name: &str, value: Option<String>) {
        if self.locals.is_empty() {
            return;
        }
        let exported = self.get_attrs(name).exported;
        self.declare_local_attrs(name, Attrs { exported, ..Attrs::default() });
        if let Some(v) = value {
            let _ = self.assign(name, v);
        }
    }

    /// Every variable name visible here, innermost scope winning, in the
    /// sorted order `declare -p` prints.
    pub fn visible_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = self
            .vars
            .keys()
            .chain(self.locals.iter().flat_map(|s| s.keys()))
            .filter(|k| !is_attr_key(k))
            .cloned()
            .collect();
        for scope in &self.locals {
            for k in scope.keys().filter(|k| is_attr_key(k)) {
                names.insert(k[1..].to_string());
            }
        }
        for k in self.vars.keys().filter(|k| is_attr_key(k)) {
            names.insert(k[1..].to_string());
        }
        names.into_iter().collect()
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
            .filter(|(k, v)| v.exported && !is_attr_key(k))
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
