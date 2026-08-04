//! The tree executor. External commands spawn as argv (never a shell
//! string); state-mutating constructs run as walker logic against the shared
//! `ShellState`. Output discipline: one shared append-mode capture file for
//! stdout+stderr of everything (no drain threads, no deadlock surface);
//! pipes exist only BETWEEN external children. Pipeline stages are
//! subshells, exactly like bash: each stage runs against a cloned state, so
//! `... | while read` not persisting variables is authentic, not a gap.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command as Proc, Stdio};
use std::sync::Arc;

use bash_parser::{Command, Connection, Connector, Redirect, RedirectOp, SimpleCommand, Word};

use crate::builtins;
use crate::cond;
use crate::expand;
use crate::state::ShellState;

/// Non-local control flow. `Fatal` aborts the whole invocation with a
/// message — used for real errors and for anything bash-walker does not
/// support (the message always names it; a silent wrong result is the one
/// forbidden outcome).
#[derive(Debug)]
pub enum Flow {
    Exit(i32),
    Return(i32),
    Break(u32),
    Continue(u32),
    Fatal(String),
    /// A failed redirect fails THAT command (status 1) and the script
    /// continues — bash's behaviour, caught at each command boundary.
    /// Turning this into `Fatal` was a real divergence the differential
    /// replay caught: bash printed the error and carried on.
    RedirectFailed(String),
}

#[derive(Clone)]
pub struct Ctx {
    pub stdin: Option<Arc<File>>,
    pub stdout: Arc<File>,
    pub stderr: Arc<File>,
    /// Streams for fds above 2 (`exec 3>&1`, `cmd 3>file`): resolved
    /// walker-side by later dup redirects, and dup2'd into spawned children
    /// so external programs see them too, as under bash.
    pub fds: std::collections::HashMap<u32, Arc<File>>,
    /// True on a ctx layered by redirects or pipeline plumbing. The shell's
    /// enduring context (what a redirect-only `exec` replaces) substitutes
    /// only at non-derived entry points, so per-command redirects still win.
    pub derived: bool,
}

impl Ctx {
    pub fn write_err(&self, msg: &str) {
        let _ = (&*self.stderr).write_all(msg.as_bytes());
    }
    pub fn write_out(&self, msg: &str) {
        let _ = (&*self.stdout).write_all(msg.as_bytes());
    }
    /// For builtins that can sit in a pipeline producing output (echo,
    /// printf): a broken pipe must kill the stage like SIGPIPE kills bash's
    /// subshell, or an upstream loop outlives its departed consumer.
    pub fn write_out_pipeaware(&self, msg: &str) -> Result<(), Flow> {
        match (&*self.stdout).write_all(msg.as_bytes()) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(Flow::Exit(141)),
            _ => Ok(()),
        }
    }
}

/// State the whole invocation shares regardless of subshell nesting.
pub struct Shared {
    pub bg: Vec<Child>,
    pub procsub_temps: Vec<PathBuf>,
    pub func_depth: u32,
    pub loop_depth: u32,
    /// How deep inside `$(...)` we are. Bash marks trace lines with one `+`
    /// per level, so the trace shows a substitution running before the
    /// command that asked for it.
    pub subst_depth: u32,
    /// Status of the most recent command substitution — an assignment-only
    /// command's status in bash (`x=$(false); echo $?` is 1).
    pub last_capture_status: Option<i32>,
    /// The shell's own context after a redirect-only `exec` — replaces the
    /// base ctx for the rest of the invocation. Subshells and pipeline
    /// stages save/restore it, so their `exec` does not leak out.
    pub persistent_ctx: Option<Ctx>,
    pub clock: Arc<dyn crate::clock::Clock + Send + Sync>,
    pub entropy: Arc<dyn crate::clock::Entropy + Send + Sync>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            bg: Vec::new(),
            procsub_temps: Vec::new(),
            func_depth: 0,
            loop_depth: 0,
            subst_depth: 0,
            last_capture_status: None,
            persistent_ctx: None,
            clock: Arc::new(crate::clock::RealClock::default()),
            entropy: Arc::new(crate::clock::RealEntropy),
        }
    }
}

pub struct Exec<'a> {
    pub state: &'a mut ShellState,
    pub shared: &'a mut Shared,
}

const DECL_BUILTINS: &[&str] = &["export", "local", "declare", "readonly", "typeset"];

impl<'a> Exec<'a> {
    /// An anonymous temp file: created then immediately unlinked, alive only
    /// through its handle — capture buffers and heredoc feeds need no
    /// cleanup pass. The counter is process-wide: multiple walkers can
    /// coexist in one process (parallel tests), so pid alone is not unique.
    pub fn anon_temp(&mut self) -> Result<File, Flow> {
        let n = unique_id();
        let path = std::env::temp_dir().join(format!(
            "bash-walker-{}-{}",
            std::process::id(),
            n
        ));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| Flow::Fatal(format!("temp file: {e}")))?;
        let _ = std::fs::remove_file(&path);
        Ok(f)
    }

    pub fn exec(&mut self, cmd: &Command, ctx: &Ctx, tested: bool) -> Result<i32, Flow> {
        let substituted;
        let ctx = if !ctx.derived && self.shared.persistent_ctx.is_some() {
            substituted = self.shared.persistent_ctx.clone().expect("checked is_some");
            &substituted
        } else {
            ctx
        };
        let status = self.exec_inner(cmd, ctx, tested)?;
        self.state.last_status = status;
        // `$PIPESTATUS`: a lone simple command is a one-stage pipeline.
        // `exec_connection`'s Pipe arm overwrites this with the real
        // per-stage array for an actual `|` chain.
        if matches!(cmd, Command::Simple(_)) {
            self.state.pipestatus = vec![status];
        }
        if status != 0
            && self.state.flags.errexit
            && !tested
            && errexit_eligible(cmd)
        {
            return Err(Flow::Exit(status));
        }
        Ok(status)
    }

    fn exec_inner(&mut self, cmd: &Command, ctx: &Ctx, tested: bool) -> Result<i32, Flow> {
        match cmd {
            Command::Simple(s) => self.exec_simple(s, ctx, tested),
            Command::Connection(c) => self.exec_connection(c, ctx, tested),
            Command::Invert(inner) => {
                let st = self.exec(inner, ctx, true)?;
                Ok(i32::from(st == 0))
            }
            Command::Time(inner) => {
                let clock = Arc::clone(&self.shared.clock);
                let wall_before = clock.now_monotonic();
                let cpu_before = clock.cpu_times();
                let st = self.exec(inner, ctx, tested)?;
                let wall = clock.now_monotonic().saturating_sub(wall_before);
                let cpu_after = clock.cpu_times();
                let user = cpu_after.user.saturating_sub(cpu_before.user);
                let sys = cpu_after.sys.saturating_sub(cpu_before.sys);
                ctx.write_err(&format!(
                    "\nreal\t{}\nuser\t{}\nsys\t{}\n",
                    fmt_interval(wall),
                    fmt_interval(user),
                    fmt_interval(sys)
                ));
                Ok(st)
            }
            Command::Background(inner) => self.exec_background(inner, ctx),
            Command::Redirected { command, redirects } => {
                let ctx2 = match self.apply_redirects(redirects, ctx) {
                    Err(Flow::RedirectFailed(msg)) => {
                        ctx.write_err(&format!("bash-walker: {msg}\n"));
                        return Ok(1);
                    }
                    other => other?,
                };
                self.exec(command, &ctx2, tested)
            }
            Command::Subshell(inner) => self.run_subshell(inner, ctx, tested),
            Command::Group(inner) => self.exec(inner, ctx, tested),
            Command::For(f) => {
                let words: Vec<Word> = f.words.clone();
                let values = if words.is_empty() {
                    self.state.positional.clone()
                } else {
                    expand::expand_fields(self, ctx, &words)?
                };
                self.run_loop_values(&f.var, &values, &f.body, ctx)
            }
            Command::ArithFor { expr, body } => self.exec_arith_for(expr, body, ctx),
            Command::If(i) => {
                for (cond_cmd, body) in &i.branches {
                    if self.exec(cond_cmd, ctx, true)? == 0 {
                        return self.exec(body, ctx, false);
                    }
                }
                match &i.else_branch {
                    Some(e) => self.exec(e, ctx, false),
                    None => Ok(0),
                }
            }
            Command::Case(c) => self.exec_case(c, ctx),
            Command::While { cond, body } => self.exec_while(cond, body, ctx, false),
            Command::Until { cond, body } => self.exec_while(cond, body, ctx, true),
            Command::Cond(expr) => {
                let ok = cond::eval(self, ctx, expr)?;
                Ok(i32::from(!ok))
            }
            Command::Arith { expr } => {
                let inner = expr.trim_start_matches("((").trim_end_matches("))");
                let text = expand::expand_textual(self, ctx, inner)?;
                let v = crate::arith::eval(&text, self.state)
                    .map_err(|e| Flow::Fatal(e.to_string()))?;
                Ok(i32::from(v == 0))
            }
            Command::FunctionDef { name, body } => {
                self.state.funcs.insert(name.clone(), (**body).clone());
                Ok(0)
            }
        }
    }

    fn exec_connection(&mut self, c: &Connection, ctx: &Ctx, tested: bool) -> Result<i32, Flow> {
        match c.connector {
            Connector::And => {
                let l = self.exec(&c.left, ctx, true)?;
                if l == 0 {
                    self.exec(&c.right, ctx, tested)
                } else {
                    Ok(l)
                }
            }
            Connector::Or => {
                let l = self.exec(&c.left, ctx, true)?;
                if l != 0 {
                    self.exec(&c.right, ctx, tested)
                } else {
                    Ok(l)
                }
            }
            Connector::Seq => {
                self.exec(&c.left, ctx, tested)?;
                self.exec(&c.right, ctx, tested)
            }
            Connector::Pipe => {
                let mut stages = Vec::new();
                flatten_pipeline(&Command::Connection(c.clone()), &mut stages);
                self.exec_pipeline(&stages, ctx, tested)
            }
        }
    }

    /// Every pipeline stage is a subshell (cloned state, saved cwd) — bash's
    /// own semantics. External simple commands spawn concurrently connected
    /// by real OS pipes; walker-internal stages run inline, buffering into
    /// an anonymous temp file when something downstream needs their output.
    fn exec_pipeline(&mut self, stages: &[Command], ctx: &Ctx, _tested: bool) -> Result<i32, Flow> {
        let saved_pctx = self.shared.persistent_ctx.clone();
        let r = self.exec_pipeline_inner(stages, ctx);
        self.shared.persistent_ctx = saved_pctx;
        r
    }

    fn exec_pipeline_inner(&mut self, stages: &[Command], ctx: &Ctx) -> Result<i32, Flow> {
        let n = stages.len();
        let mut statuses: Vec<Option<i32>> = vec![None; n];
        let mut waiting: Vec<Option<Child>> = Vec::with_capacity(n);
        let mut threads: Vec<Option<std::thread::JoinHandle<i32>>> = Vec::with_capacity(n);
        let mut next_stdin: Option<Arc<File>> = None;

        for (i, stage) in stages.iter().enumerate() {
            let last = i + 1 == n;
            let stage_stdin = if i == 0 { ctx.stdin.clone() } else { next_stdin.take() };

            // A stage is a subshell: cloned state, so its cd/vars stay its own.
            let mut stage_state = self.state.clone();

            // An external simple command spawns and stays concurrent. The
            // pipe is wired into the stage ctx BEFORE its redirects apply,
            // exactly like bash: `cmd 2>&1 | grep` must merge stderr into
            // the PIPE, not the surrounding output. (Applying redirects
            // first sent django's stderr around grep — the dominant class
            // of the 10% sequence-replay divergences.)
            if let Command::Simple(s) = stage {
                let mut sub = Exec { state: &mut stage_state, shared: &mut *self.shared };
                match sub.prepare_simple(s, ctx)? {
                    Prepared::External { fields, assigns, redirects } => {
                        // Every stage is traced, not just the first: half a
                        // pipeline is worse than none. Written to the outer
                        // ctx so the trace never enters the pipe.
                        for (k, v) in &assigns {
                            sub.xtrace_assign(ctx, k, v);
                        }
                        sub.xtrace(ctx, &fields);
                        let mut stage_ctx =
                            Ctx { stdin: stage_stdin, derived: true, ..ctx.clone() };
                        let mut reader: Option<File> = None;
                        if !last {
                            let (r, w) =
                                std::io::pipe().map_err(|e| Flow::Fatal(e.to_string()))?;
                            stage_ctx.stdout = Arc::new(File::from(std::os::fd::OwnedFd::from(w)));
                            reader = Some(File::from(std::os::fd::OwnedFd::from(r)));
                        }
                        let stage_ctx = match sub.apply_redirects(&redirects, &stage_ctx) {
                            Err(Flow::RedirectFailed(msg)) => {
                                stage_ctx.write_err(&format!("bash-walker: {msg}\n"));
                                statuses[i] = Some(1);
                                waiting.push(None);
                                if !last {
                                    let empty = sub.anon_temp()?;
                                    next_stdin = Some(Arc::new(empty));
                                }
                                threads.push(None);
                                continue;
                            }
                            other => other?,
                        };
                        match sub.spawn(&fields, &assigns, &stage_ctx)?.take() {
                            SpawnResult::Child(ch) => {
                                waiting.push(Some(ch));
                            }
                            SpawnResult::Failed(st) => {
                                statuses[i] = Some(st);
                                waiting.push(None);
                            }
                        }
                        if let Some(r) = reader {
                            next_stdin = Some(Arc::new(r));
                        }
                        threads.push(None);
                        continue;
                    }
                    Prepared::Internal => {} // falls through to the threaded path
                }
            }

            // Internal stage: its own interpreter on its own thread, wired
            // by a real pipe — concurrent with every other stage, exactly
            // like bash's forked subshell. Foreground pipelines join before
            // returning, so no stage outlives the process.
            let (out_file, reader): (File, Option<File>) = if last {
                (
                    ctx.stdout.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?,
                    None,
                )
            } else {
                let (r, w) = std::io::pipe().map_err(|e| Flow::Fatal(e.to_string()))?;
                (
                    File::from(std::os::fd::OwnedFd::from(w)),
                    Some(File::from(std::os::fd::OwnedFd::from(r))),
                )
            };
            let stage_ctx = Ctx {
                stdin: stage_stdin,
                stdout: Arc::new(out_file),
                stderr: Arc::clone(&ctx.stderr),
                fds: ctx.fds.clone(),
                derived: true,
            };
            let stage_cmd = stage.clone();
            let clock = Arc::clone(&self.shared.clock);
            let entropy = Arc::clone(&self.shared.entropy);
            // A stage inherits how deep in `$(...)` the pipeline itself is, or
            // a substitution inside a stage would trace one level too shallow.
            let subst_depth = self.shared.subst_depth;
            let handle = std::thread::spawn(move || -> i32 {
                let mut shared = Shared { clock, entropy, subst_depth, ..Shared::default() };
                let mut sub = Exec { state: &mut stage_state, shared: &mut shared };
                match sub.exec(&stage_cmd, &stage_ctx, true) {
                    Ok(st) => st,
                    Err(Flow::Exit(st)) | Err(Flow::Return(st)) => st,
                    Err(Flow::Break(_)) | Err(Flow::Continue(_)) => 0,
                    Err(Flow::Fatal(msg)) | Err(Flow::RedirectFailed(msg)) => {
                        stage_ctx.write_err(&format!("bash-walker: {msg}\n"));
                        1
                    }
                }
            });
            threads.push(Some(handle));
            waiting.push(None);
            if let Some(r) = reader {
                next_stdin = Some(Arc::new(r));
            }
        }

        for (i, slot) in waiting.iter_mut().enumerate() {
            if let Some(child) = slot {
                statuses[i] = Some(wait_reporting(child, ctx));
            }
        }
        for (i, slot) in threads.iter_mut().enumerate() {
            if let Some(handle) = slot.take() {
                statuses[i] = Some(handle.join().unwrap_or(1));
            }
        }

        self.state.pipestatus = statuses.iter().map(|s| s.unwrap_or(1)).collect();
        let final_status = if self.state.flags.pipefail {
            statuses
                .iter()
                .filter_map(|s| *s)
                .rfind(|s| *s != 0)
                .unwrap_or(0)
        } else {
            statuses.last().copied().flatten().unwrap_or(0)
        };
        Ok(final_status)
    }

    fn run_subshell(&mut self, inner: &Command, ctx: &Ctx, tested: bool) -> Result<i32, Flow> {
        let saved_pctx = self.shared.persistent_ctx.clone();
        let mut sub_state = self.state.clone();
        let mut sub = Exec { state: &mut sub_state, shared: &mut *self.shared };
        let r = sub.exec(inner, ctx, tested);
        let capture_status = sub_state.last_status;
        self.shared.persistent_ctx = saved_pctx;
        match r {
            Ok(st) => Ok(st),
            Err(Flow::Exit(st)) => Ok(st),
            // A stray break/continue/return cannot cross a subshell.
            Err(Flow::Break(_)) | Err(Flow::Continue(_)) => Ok(capture_status),
            Err(Flow::Return(st)) => Ok(st),
            Err(f @ (Flow::Fatal(_) | Flow::RedirectFailed(_))) => Err(f),
        }
    }

    fn exec_while(
        &mut self,
        cond_cmd: &Command,
        body: &Command,
        ctx: &Ctx,
        until: bool,
    ) -> Result<i32, Flow> {
        let mut status = 0;
        self.shared.loop_depth += 1;
        let result = loop {
            let c = match self.exec(cond_cmd, ctx, true) {
                Ok(c) => c,
                Err(f) => break Err(f),
            };
            let run_body = if until { c != 0 } else { c == 0 };
            if !run_body {
                break Ok(status);
            }
            match self.exec(body, ctx, false) {
                Ok(st) => status = st,
                Err(Flow::Break(1)) => break Ok(status),
                Err(Flow::Break(k)) => break Err(Flow::Break(k - 1)),
                Err(Flow::Continue(1)) => continue,
                Err(Flow::Continue(k)) => break Err(Flow::Continue(k - 1)),
                Err(f) => break Err(f),
            }
        };
        self.shared.loop_depth -= 1;
        result
    }

    fn run_loop_values(
        &mut self,
        var: &str,
        values: &[String],
        body: &Command,
        ctx: &Ctx,
    ) -> Result<i32, Flow> {
        let mut status = 0;
        self.shared.loop_depth += 1;
        let mut result = Ok(());
        // Bash re-traces the loop header on every iteration, with the word
        // list already expanded.
        let mut header = vec!["for".to_string(), var.to_string(), "in".to_string()];
        header.extend(values.iter().cloned());
        for v in values {
            self.xtrace(ctx, &header);
            self.state.set_var(var, v.clone());
            match self.exec(body, ctx, false) {
                Ok(st) => status = st,
                Err(Flow::Break(1)) => break,
                Err(Flow::Break(k)) => {
                    result = Err(Flow::Break(k - 1));
                    break;
                }
                Err(Flow::Continue(1)) => continue,
                Err(Flow::Continue(k)) => {
                    result = Err(Flow::Continue(k - 1));
                    break;
                }
                Err(f) => {
                    result = Err(f);
                    break;
                }
            }
        }
        self.shared.loop_depth -= 1;
        result.map(|()| status)
    }

    fn exec_arith_for(&mut self, expr: &str, body: &Command, ctx: &Ctx) -> Result<i32, Flow> {
        let inner = expr.trim_start_matches("((").trim_end_matches("))");
        let parts: Vec<&str> = inner.split(';').collect();
        if parts.len() != 3 {
            return Err(Flow::Fatal(format!("for (({inner})): expected init;cond;step")));
        }
        let eval_part = |ex: &mut Exec, part: &str, default: i64| -> Result<i64, Flow> {
            if part.trim().is_empty() {
                return Ok(default);
            }
            let text = expand::expand_textual(ex, ctx, part)?;
            crate::arith::eval(&text, ex.state).map_err(|e| Flow::Fatal(e.to_string()))
        };
        eval_part(self, parts[0], 0)?;
        let mut status = 0;
        self.shared.loop_depth += 1;
        let result = loop {
            match eval_part(self, parts[1], 1) {
                Ok(0) => break Ok(status),
                Ok(_) => {}
                Err(f) => break Err(f),
            }
            match self.exec(body, ctx, false) {
                Ok(st) => status = st,
                Err(Flow::Break(1)) => break Ok(status),
                Err(Flow::Break(k)) => break Err(Flow::Break(k - 1)),
                Err(Flow::Continue(1)) => {}
                Err(Flow::Continue(k)) => break Err(Flow::Continue(k - 1)),
                Err(f) => break Err(f),
            }
            if let Err(f) = eval_part(self, parts[2], 0) {
                break Err(f);
            }
        };
        self.shared.loop_depth -= 1;
        result
    }

    fn exec_case(&mut self, c: &bash_parser::CaseCommand, ctx: &Ctx) -> Result<i32, Flow> {
        let subject = expand::expand_single(self, ctx, &c.word)?;
        // Bash traces the header once, with the subject already expanded.
        self.xtrace(ctx, &["case".to_string(), subject.clone(), "in".to_string()]);
        let mut status = 0;
        let mut fell_through = false;
        for arm in &c.arms {
            let matched = fell_through
                || arm.patterns.iter().try_fold(false, |acc, p| {
                    if acc {
                        return Ok::<bool, Flow>(true);
                    }
                    let parts = expand::expand_parts(self, ctx, p)?;
                    let pat = expand::glob_pattern_from_parts(&parts);
                    Ok(glob::Pattern::new(&pat)
                        .map(|g| g.matches(&subject))
                        .unwrap_or(false))
                })?;
            if !matched {
                continue;
            }
            if let Some(body) = &arm.body {
                status = self.exec(body, ctx, false)?;
            }
            match arm.terminator {
                bash_parser::CaseTerminator::Stop => return Ok(status),
                bash_parser::CaseTerminator::Fallthrough => {
                    fell_through = true;
                }
                bash_parser::CaseTerminator::TestNext => {
                    fell_through = false;
                }
            }
        }
        Ok(status)
    }

    fn exec_background(&mut self, inner: &Command, ctx: &Ctx) -> Result<i32, Flow> {
        // Fast path: a pipeline of external simple commands spawns directly.
        // Anything else — compounds, builtins, functions — becomes a child
        // walker running the subtree: a real process, so the job outlives
        // this invocation exactly as bash's forked subshell would.
        let mut stages = Vec::new();
        flatten_pipeline(inner, &mut stages);
        let all_external_simples = stages.iter().all(|s| match s {
            Command::Simple(sc) => sc.program.as_ref().is_some_and(|p| {
                !self.state.funcs.contains_key(&p.text) && !builtins::is_builtin(&p.text)
            }),
            _ => false,
        });
        if !all_external_simples {
            return self.spawn_background_job(inner, ctx);
        }
        let mut simples = Vec::new();
        for s in &stages {
            match s {
                Command::Simple(sc) => simples.push(sc),
                _ => unreachable!("checked all stages are simple"),
            }
        }
        let mut bg_state = self.state.clone();
        let mut sub = Exec { state: &mut bg_state, shared: &mut *self.shared };
        let n = simples.len();
        let mut next_stdin: Option<Arc<File>> = None;
        let mut last_pid = None;
        for (i, s) in simples.iter().enumerate() {
            let last = i + 1 == n;
            let stage_stdin = if i == 0 { None } else { next_stdin.take() };
            match sub.prepare_simple(s, ctx)? {
                Prepared::External { fields, assigns, redirects } => {
                    let mut stage_ctx =
                        Ctx { stdin: stage_stdin, derived: true, ..ctx.clone() };
                    let mut reader: Option<File> = None;
                    if !last {
                        let (r, w) = std::io::pipe().map_err(|e| Flow::Fatal(e.to_string()))?;
                        stage_ctx.stdout = Arc::new(File::from(std::os::fd::OwnedFd::from(w)));
                        reader = Some(File::from(std::os::fd::OwnedFd::from(r)));
                    }
                    let stage_ctx = match sub.apply_redirects(&redirects, &stage_ctx) {
                        Err(Flow::RedirectFailed(msg)) => {
                            stage_ctx.write_err(&format!("bash-walker: {msg}\n"));
                            continue;
                        }
                        other => other?,
                    };
                    match sub.spawn(&fields, &assigns, &stage_ctx)?.take() {
                        SpawnResult::Child(ch) => {
                            last_pid = Some(ch.id());
                            sub.shared.bg.push(ch);
                        }
                        SpawnResult::Failed(_) => {}
                    }
                    if let Some(r) = reader {
                        next_stdin = Some(Arc::new(r));
                    }
                }
                Prepared::Internal => {
                    return Err(Flow::Fatal(
                        "backgrounding a builtin or function is not supported by bash-walker"
                            .into(),
                    ))
                }
            }
        }
        self.state.last_background_pid = last_pid;
        Ok(0)
    }

    /// Fork, in spawn form: a child walker executes the subtree with a copy
    /// of the shell state, detached. `$!` is its real pid, so `wait` and
    /// `kill` behave exactly as under bash.
    fn spawn_background_job(&mut self, inner: &Command, ctx: &Ctx) -> Result<i32, Flow> {
        let job = crate::BackgroundJob {
            command: inner.clone(),
            funcs: self.state.funcs.clone(),
            vars: self.state.vars.clone(),
            cwd: self.state.cwd.clone(),
            umask: self.state.umask,
        };
        let payload = serde_json::to_string(&job)
            .map_err(|e| Flow::Fatal(format!("background job: {e}")))?;
        let mut cmd = Proc::new(walker_exe());
        cmd.arg("--ast-stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(
                ctx.stdout.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?,
            ))
            .stderr(Stdio::from(
                ctx.stderr.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?,
            ));
        let mut ch = cmd
            .spawn()
            .map_err(|e| Flow::Fatal(format!("background job: {}", errmsg(&e))))?;
        if let Some(mut stdin) = ch.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes());
        }
        self.state.last_background_pid = Some(ch.id());
        self.shared.bg.push(ch);
        Ok(0)
    }

    /// Expansion and classification for one simple command, shared by the
    /// direct path and the pipeline path.
    fn prepare_simple(&mut self, s: &SimpleCommand, ctx: &Ctx) -> Result<Prepared, Flow> {
        // Classification only needs the program name; the full expansion
        // happens on whichever path runs it. A builtin or function is
        // internal; everything else external.
        let name = match &s.program {
            Some(w) => w.text.clone(),
            None => return Ok(Prepared::Internal),
        };
        if self.state.funcs.contains_key(&name) || builtins::is_builtin(&name) {
            return Ok(Prepared::Internal);
        }
        let mut words = vec![s.program.clone().unwrap()];
        words.extend(s.args.iter().cloned());
        let fields = expand::expand_fields(self, ctx, &words)?;
        // Expansion can change the verdict ($cmd resolving to a builtin) —
        // re-check on the expanded name.
        match fields.first() {
            None => Ok(Prepared::Internal),
            Some(n) if self.state.funcs.contains_key(n) || builtins::is_builtin(n) => {
                Ok(Prepared::Internal)
            }
            Some(_) => {
                let mut assigns = Vec::new();
                for (k, v) in &s.assignments {
                    assigns.push((k.clone(), expand::expand_single(self, ctx, v)?));
                }
                Ok(Prepared::External {
                    fields,
                    assigns,
                    redirects: s.redirects.clone(),
                })
            }
        }
    }

    fn exec_simple(&mut self, s: &SimpleCommand, ctx: &Ctx, _tested: bool) -> Result<i32, Flow> {
        self.shared.last_capture_status = None;

        // Declaration builtins get assignment-style argument expansion:
        // `export FOO=$x` must not word-split $x.
        let is_decl = s
            .program
            .as_ref()
            .is_some_and(|p| DECL_BUILTINS.contains(&p.text.as_str()));

        let mut assigns = Vec::new();
        for (k, v) in &s.assignments {
            assigns.push((k.clone(), expand::expand_single(self, ctx, v)?));
        }

        let fields = match &s.program {
            None => Vec::new(),
            Some(p) => {
                let mut words = vec![p.clone()];
                words.extend(s.args.iter().cloned());
                if is_decl {
                    let mut fs = Vec::with_capacity(words.len());
                    for w in &words {
                        fs.push(expand::expand_single(self, ctx, w)?);
                    }
                    fs
                } else {
                    expand::expand_fields(self, ctx, &words)?
                }
            }
        };

        let ctx2 = match self.apply_redirects(&s.redirects, ctx) {
            Err(Flow::RedirectFailed(msg)) => {
                ctx.write_err(&format!("bash-walker: {msg}\n"));
                return Ok(1);
            }
            other => other?,
        };

        if fields.is_empty() {
            for (k, v) in assigns {
                self.xtrace_assign(&ctx2, &k, &v);
                self.state.set_var(&k, v);
            }
            return Ok(self.shared.last_capture_status.unwrap_or(0));
        }

        // Bash traces a command's own assignments as separate lines ahead of
        // it, so `foo=bar cmd` is two lines, not one.
        for (k, v) in &assigns {
            self.xtrace_assign(&ctx2, k, v);
        }
        self.xtrace(&ctx2, &fields);

        let name = fields[0].clone();
        let args = &fields[1..];

        if let Some(body) = self.state.funcs.get(&name).cloned() {
            return self.call_function(&body, args, &assigns, &ctx2);
        }
        if builtins::is_builtin(&name) {
            return self.with_temp_assigns(&assigns, |ex| {
                builtins::run(ex, &ctx2, &name, args)
            });
        }
        match self.spawn(&fields, &assigns, &ctx2)?.take() {
            SpawnResult::Child(mut ch) => Ok(wait_reporting(&mut ch, &ctx2)),
            SpawnResult::Failed(st) => Ok(st),
        }
    }

    /// One `set -x` line: `+` per substitution level, then the words as bash
    /// prints them. The quoting is the point of the trace, since `echo "a b"`
    /// and `echo a b` are the same characters and different commands.
    fn xtrace(&self, ctx: &Ctx, words: &[String]) {
        if !self.state.flags.xtrace {
            return;
        }
        let line: Vec<String> = words.iter().map(|w| xtrace_quote(w)).collect();
        ctx.write_err(&format!("{}{}\n", self.xtrace_prefix(), line.join(" ")));
    }

    /// An assignment traces as `name=value` with only the value quoted.
    fn xtrace_assign(&self, ctx: &Ctx, name: &str, value: &str) {
        if !self.state.flags.xtrace {
            return;
        }
        ctx.write_err(&format!("{}{name}={}\n", self.xtrace_prefix(), xtrace_quote(value)));
    }

    /// `PS4`, with its first character repeated once per substitution level.
    /// That is bash's own rule, which is why the default `+ ` becomes `++ `
    /// inside a substitution rather than `+ + `. Setting PS4 to something
    /// unmistakable is how a reader tells the trace apart from output that
    /// happens to start with a plus, which any diff does.
    fn xtrace_prefix(&self) -> String {
        let ps4 = self.state.get_var("PS4").unwrap_or_else(|| "+ ".to_string());
        let lead = ps4.chars().next().unwrap_or('+');
        format!("{}{ps4}", String::from(lead).repeat(self.shared.subst_depth as usize))
    }

    /// Spawn one external command and wait — the `exec cmd` path, where the
    /// command's status becomes the shell's exit status.
    pub(crate) fn run_external_wait(&mut self, fields: &[String], ctx: &Ctx) -> Result<i32, Flow> {
        match self.spawn(fields, &[], ctx)?.take() {
            SpawnResult::Child(mut ch) => Ok(wait_reporting(&mut ch, ctx)),
            SpawnResult::Failed(st) => Ok(st),
        }
    }

    /// `FOO=1 builtin/function`: the assignments live for the call only.
    fn with_temp_assigns<T>(
        &mut self,
        assigns: &[(String, String)],
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        if assigns.is_empty() {
            return f(self);
        }
        let mut scope = std::collections::HashMap::new();
        for (k, v) in assigns {
            scope.insert(k.clone(), crate::state::Var { value: v.clone(), exported: true });
        }
        self.state.locals.push(scope);
        let r = f(self);
        self.state.locals.pop();
        r
    }

    fn call_function(
        &mut self,
        body: &Command,
        args: &[String],
        assigns: &[(String, String)],
        ctx: &Ctx,
    ) -> Result<i32, Flow> {
        let saved_positional = std::mem::replace(&mut self.state.positional, args.to_vec());
        self.state.locals.push(std::collections::HashMap::new());
        for (k, v) in assigns {
            self.state.declare_local(k, Some(v.clone()));
        }
        self.shared.func_depth += 1;
        let r = self.exec(body, ctx, false);
        self.shared.func_depth -= 1;
        self.state.locals.pop();
        self.state.positional = saved_positional;
        match r {
            Err(Flow::Return(n)) => Ok(n),
            other => other,
        }
    }

    /// Errors report to THIS ctx — the command's own (redirected) stderr,
    /// so `missing-cmd 2>/dev/null` stays silent, as under bash.
    fn spawn(
        &mut self,
        fields: &[String],
        assigns: &[(String, String)],
        ctx: &Ctx,
    ) -> Result<SpawnSlot, Flow> {
        let mut cmd = Proc::new(&fields[0]);
        cmd.args(&fields[1..]);
        // The child's world is built from shell state, not inherited: cwd
        // from the state's cwd, environment from the exported vars (the
        // process env is a stale birth snapshot, never consulted).
        cmd.current_dir(&self.state.cwd);
        cmd.env_clear();
        cmd.envs(self.state.child_env());
        for (k, v) in assigns {
            cmd.env(k, v);
        }
        // umask is process state in bash's own model, but ours never touches
        // the WALKER's process umask (racy under threaded pipeline stages,
        // same reasoning as cwd/env) — it's set post-fork, pre-exec, in the
        // single-threaded child, exactly like the fd-dup2 wiring above.
        let child_umask = self.state.umask;
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                libc::umask(child_umask as libc::mode_t);
                Ok(())
            });
        }
        cmd.stdin(match &ctx.stdin {
            Some(f) => Stdio::from(f.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?),
            None => Stdio::null(),
        });
        cmd.stdout(Stdio::from(
            ctx.stdout
                .try_clone()
                .map_err(|e| Flow::Fatal(e.to_string()))?,
        ));
        cmd.stderr(Stdio::from(
            ctx.stderr
                .try_clone()
                .map_err(|e| Flow::Fatal(e.to_string()))?,
        ));
        if !ctx.fds.is_empty() {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let mut dups: Vec<(File, i32)> = Vec::with_capacity(ctx.fds.len());
            for (n, f) in &ctx.fds {
                let h = f.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?;
                dups.push((h, *n as i32));
            }
            // SAFETY: dup2 is async-signal-safe; the handles live in the
            // closure, which the Command owns until spawn completes.
            unsafe {
                cmd.pre_exec(move || {
                    for (h, dst) in &dups {
                        if libc::dup2(h.as_raw_fd(), *dst) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
        }
        match cmd.spawn() {
            Ok(ch) => Ok(SpawnSlot(Some(SpawnResult::Child(ch)))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // bash's dialect: a path is "No such file or directory", a
                // PATH lookup miss is "command not found". Agents read these.
                if fields[0].contains('/') {
                    ctx.write_err(&format!(
                        "bash-walker: {}: No such file or directory\n",
                        fields[0]
                    ));
                } else {
                    ctx.write_err(&format!("bash-walker: {}: command not found\n", fields[0]));
                }
                Ok(SpawnSlot(Some(SpawnResult::Failed(127))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                ctx.write_err(&format!("bash-walker: {}: Permission denied\n", fields[0]));
                Ok(SpawnSlot(Some(SpawnResult::Failed(126))))
            }
            Err(e) => {
                ctx.write_err(&format!("bash-walker: {}: {e}\n", fields[0]));
                Ok(SpawnSlot(Some(SpawnResult::Failed(1))))
            }
        }
    }

    fn apply_redirects(&mut self, redirects: &[Redirect], ctx: &Ctx) -> Result<Ctx, Flow> {
        let mut c = ctx.clone();
        c.derived = true;
        for r in redirects {
            match r.op {
                RedirectOp::Out | RedirectOp::Append => {
                    let path = expand::expand_redirect_target(self, &c, &r.target)?;
                    let f = std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .append(r.op == RedirectOp::Append)
                        .truncate(r.op == RedirectOp::Out)
                        .mode(self.state.create_mode())
                        .open(self.state.resolve(&path))
                        .map_err(|e| Flow::RedirectFailed(format!("{path}: {}", errmsg(&e))))?;
                    match r.fd {
                        None | Some(1) => c.stdout = Arc::new(f),
                        Some(2) => c.stderr = Arc::new(f),
                        Some(n) => {
                            c.fds.insert(n, Arc::new(f));
                        }
                    }
                }
                RedirectOp::OutErr | RedirectOp::AppendOutErr => {
                    let path = expand::expand_redirect_target(self, &c, &r.target)?;
                    let f = std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .append(r.op == RedirectOp::AppendOutErr)
                        .truncate(r.op == RedirectOp::OutErr)
                        .mode(self.state.create_mode())
                        .open(self.state.resolve(&path))
                        .map_err(|e| Flow::RedirectFailed(format!("{path}: {}", errmsg(&e))))?;
                    let f = Arc::new(f);
                    c.stdout = Arc::clone(&f);
                    c.stderr = f;
                }
                RedirectOp::In => {
                    let path = expand::expand_redirect_target(self, &c, &r.target)?;
                    let f = File::open(self.state.resolve(&path))
                        .map_err(|e| Flow::RedirectFailed(format!("{path}: {}", errmsg(&e))))?;
                    c.stdin = Some(Arc::new(f));
                }
                RedirectOp::DupOut => {
                    let target = expand::expand_redirect_target(self, &c, &r.target)?;
                    let src = r.fd.unwrap_or(1);
                    match target.as_str() {
                        "-" => {
                            return Err(Flow::Fatal(
                                ">&-: closing fds is not supported by bash-walker".into(),
                            ))
                        }
                        t if t.parse::<u32>().is_ok() => {
                            let m: u32 = t.parse().expect("checked numeric");
                            let stream = match m {
                                1 => Arc::clone(&c.stdout),
                                2 => Arc::clone(&c.stderr),
                                n => match c.fds.get(&n) {
                                    Some(f) => Arc::clone(f),
                                    None => {
                                        return Err(Flow::RedirectFailed(format!(
                                            "{n}: Bad file descriptor"
                                        )))
                                    }
                                },
                            };
                            match src {
                                1 => c.stdout = stream,
                                2 => c.stderr = stream,
                                n => {
                                    c.fds.insert(n, stream);
                                }
                            }
                        }
                        // `>& file` (no fd, non-numeric): both streams to it.
                        path if r.fd.is_none() => {
                            let f = std::fs::OpenOptions::new()
                                .create(true)
                                .write(true)
                                .truncate(true)
                                .mode(self.state.create_mode())
                                .open(self.state.resolve(path))
                                .map_err(|e| Flow::Fatal(format!("{path}: {e}")))?;
                            let f = Arc::new(f);
                            c.stdout = Arc::clone(&f);
                            c.stderr = f;
                        }
                        other => {
                            return Err(Flow::Fatal(format!(
                                ">&{other}: unsupported fd duplication"
                            )))
                        }
                    }
                }
                RedirectOp::DupIn => {
                    let target = expand::expand_redirect_target(self, &c, &r.target)?;
                    if target != "0" {
                        return Err(Flow::Fatal(format!(
                            "<&{target}: fd duplication onto stdin is not supported by bash-walker"
                        )));
                    }
                }
                RedirectOp::Heredoc | RedirectOp::HeredocStrip => {
                    let body = r.heredoc_body.clone().unwrap_or_default();
                    let body = if r.target.quoted {
                        body
                    } else {
                        expand::expand_textual(self, &c, &body)?
                    };
                    c.stdin = Some(Arc::new(self.feed_file(body.as_bytes())?));
                }
                RedirectOp::HereString => {
                    let mut s = expand::expand_single(self, &c, &r.target)?;
                    s.push('\n');
                    c.stdin = Some(Arc::new(self.feed_file(s.as_bytes())?));
                }
            }
        }
        Ok(c)
    }

    /// An anonymous file pre-loaded with bytes, positioned at the start —
    /// heredoc and here-string stdin.
    fn feed_file(&mut self, bytes: &[u8]) -> Result<File, Flow> {
        let mut f = self.anon_temp()?;
        f.write_all(bytes).map_err(|e| Flow::Fatal(e.to_string()))?;
        f.seek(SeekFrom::Start(0)).map_err(|e| Flow::Fatal(e.to_string()))?;
        Ok(f)
    }
}

enum Prepared {
    External {
        fields: Vec<String>,
        assigns: Vec<(String, String)>,
        redirects: Vec<Redirect>,
    },
    Internal,
}

enum SpawnResult {
    Child(Child),
    Failed(i32),
}

struct SpawnSlot(Option<SpawnResult>);
impl SpawnSlot {
    fn take(&mut self) -> SpawnResult {
        self.0.take().expect("spawn result taken once")
    }
}

fn errexit_eligible(cmd: &Command) -> bool {
    !matches!(
        cmd,
        Command::Connection(Connection { connector: Connector::And | Connector::Or, .. })
            | Command::Invert(_)
            | Command::FunctionDef { .. }
    )
}

fn unique_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// bash's `time` interval shape: `1m2.345s`.
fn fmt_interval(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    format!("{}m{:.3}s", (secs / 60.0) as u64, secs % 60.0)
}

/// io::Error's Display appends " (os error N)"; bash's messages don't.
/// Agents read these strings, so speak bash's dialect.
pub fn errmsg(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(pos) => s[..pos].to_string(),
        None => s,
    }
}

fn s_signal(st: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    st.signal().unwrap_or(0)
}

/// Wait for a child; a signal death gets bash's epitaph on stderr
/// ("Aborted", "Segmentation fault", ...) — SIGPIPE dies silently, as in
/// bash — and the status is 128+signal either way.
fn wait_reporting(child: &mut Child, ctx: &Ctx) -> i32 {
    match child.wait() {
        Ok(st) => match st.code() {
            Some(c) => c,
            None => {
                let sig = s_signal(&st);
                if let Some(msg) = signal_epitaph(sig) {
                    ctx.write_err(&format!("{msg}\n"));
                }
                128 + sig
            }
        },
        Err(_) => 1,
    }
}

fn signal_epitaph(sig: i32) -> Option<&'static str> {
    match sig {
        4 => Some("Illegal instruction"),
        5 => Some("Trace/breakpoint trap"),
        6 => Some("Aborted"),
        7 => Some("Bus error"),
        8 => Some("Floating point exception"),
        9 => Some("Killed"),
        11 => Some("Segmentation fault"),
        15 => Some("Terminated"),
        _ => None,
    }
}

fn flatten_pipeline(cmd: &Command, out: &mut Vec<Command>) {
    match cmd {
        Command::Connection(Connection { connector: Connector::Pipe, left, right }) => {
            flatten_pipeline(left, out);
            out.push((**right).clone());
        }
        other => out.push(other.clone()),
    }
}

/// Bash's `set -x` quoting, taken from bash 5.3's own output rather than from
/// memory. Three forms, in order:
///
/// - a word holding a control character prints as `$'...'`, with the named
///   escapes bash uses and three-digit octal for the rest;
/// - a word needing quotes but holding no control character prints in single
///   quotes, with an embedded quote closed and reopened as `'\''`;
/// - anything else prints bare, including printable multibyte text: bash
///   leaves `café ✓ →` alone.
///
/// An empty word prints as `''`, which is how the trace shows a field that
/// exists and holds nothing.
fn xtrace_quote(word: &str) -> String {
    if word.is_empty() {
        return "''".to_string();
    }
    if word.chars().any(char::is_control) {
        return ansi_c_quote(word);
    }
    if word.chars().all(xtrace_bare) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Bash prints these unquoted. The ASCII punctuation set is exactly what bash
/// 5.3 leaves bare, established by sweeping every punctuation character
/// through its own trace; `^` is not in it and `#` and `~` are.
fn xtrace_bare(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric() || "#%+,-./:=@_~".contains(c)
    } else {
        !c.is_control()
    }
}

fn ansi_c_quote(word: &str) -> String {
    let mut out = String::from("$'");
    for c in word.chars() {
        match c {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            // Bash writes ESC as \E, not \e.
            '\u{1b}' => out.push_str("\\E"),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Parse and execute a source string in the current shell (top level, the
/// `eval`/`source` builtins, and substitution interiors).
pub fn run_source(ex: &mut Exec, ctx: &Ctx, src: &str, tested: bool) -> Result<i32, Flow> {
    // Empty input is a valid empty program — `bash -c ''`, `$()`, and
    // `` `` `` all succeed doing nothing. Found live: `` (`attrs`) `` docs
    // text made Claude write empty backtick pairs bash happily ran.
    if src.trim().is_empty() {
        return Ok(0);
    }
    let cmd = bash_parser::parse(src)
        .map_err(|e| Flow::Fatal(format!("syntax error: {e}")))?;
    ex.exec(&cmd, ctx, tested)
}

/// `$(...)`: run in a subshell, capture stdout, report the status back for
/// `$?` and assignment-only commands.
pub fn run_capture(ex: &mut Exec, ctx: &Ctx, src: &str) -> Result<String, Flow> {
    ex.shared.subst_depth += 1;
    let r = run_capture_inner(ex, ctx, src);
    ex.shared.subst_depth -= 1;
    r
}

fn run_capture_inner(ex: &mut Exec, ctx: &Ctx, src: &str) -> Result<String, Flow> {
    let capture = ex.anon_temp()?;
    let handle = capture.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?;
    let saved_pctx = ex.shared.persistent_ctx.clone();
    let mut sub_state = ex.state.clone();
    // A substitution does not inherit `-e` (bash: `set -e; echo $(false; echo
    // ok)` prints ok), but a `set -e` written inside it does apply to its own
    // commands. Marking the whole body as a tested context did both at once,
    // so `x=$(set -e; false; echo bad)` ran on and captured "bad" where bash
    // aborts with an empty capture and status 1.
    sub_state.flags.errexit = false;
    let status = {
        let mut sub = Exec { state: &mut sub_state, shared: &mut *ex.shared };
        let sub_ctx = Ctx {
            stdin: ctx.stdin.clone(),
            stdout: Arc::new(capture),
            stderr: Arc::clone(&ctx.stderr),
            fds: ctx.fds.clone(),
            derived: true,
        };
        match run_source(&mut sub, &sub_ctx, src, false) {
            Ok(st) => st,
            Err(Flow::Exit(st)) => st,
            Err(f) => return Err(f),
        }
    };
    ex.shared.persistent_ctx = saved_pctx;
    ex.state.last_status = status;
    ex.shared.last_capture_status = Some(status);
    let mut out = String::new();
    let mut h = handle;
    let _ = h.seek(SeekFrom::Start(0));
    let _ = h.read_to_string(&mut out);
    while out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

/// `<(...)`: a real FIFO fed by a second walker process running the inner
/// script — bash's own mechanism (fork + FIFO), done by re-exec because the
/// walker cannot safely fork itself. Concurrent like bash: the consumer
/// reads while the child writes, so early-exit consumers (`head`) and
/// infinite producers behave correctly. The child inherits the full shell
/// state (cwd + variables) via a throwaway state file.
pub fn run_procsub(ex: &mut Exec, ctx: &Ctx, src: &str) -> Result<String, Flow> {
    let tag = format!("{}-{}", std::process::id(), unique_id());
    let fifo = std::env::temp_dir().join(format!("bash-walker-procsub-{tag}"));
    let cpath = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes().to_vec())
        .map_err(|e| Flow::Fatal(format!("procsub path: {e}")))?;
    if unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) } != 0 {
        return Err(Flow::Fatal(format!(
            "procsub fifo: {}",
            errmsg(&std::io::Error::last_os_error())
        )));
    }
    ex.shared.procsub_temps.push(fifo.clone());

    let state_path = std::env::temp_dir().join(format!("bash-walker-procsub-state-{tag}"));
    crate::state::save(&state_path, ex.state)
        .map_err(|e| Flow::Fatal(format!("procsub state: {e}")))?;
    ex.shared.procsub_temps.push(state_path.clone());

    // The child opens the FIFO itself, write-only — blocking until the
    // consumer opens the read side, and dying on SIGPIPE when the consumer
    // leaves early. Opening it here (the only non-blocking option is
    // O_RDWR) would make this process a phantom reader and break both.
    let mut cmd = Proc::new(walker_exe());
    cmd.arg("--state")
        .arg(&state_path)
        .arg("--stdout-path")
        .arg(&fifo)
        .arg("-c")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            ctx.stderr
                .try_clone()
                .map_err(|e| Flow::Fatal(e.to_string()))?,
        ));
    match cmd.spawn() {
        Ok(ch) => ex.shared.bg.push(ch),
        Err(e) => return Err(Flow::Fatal(format!("procsub: {}", errmsg(&e)))),
    }
    Ok(fifo.to_string_lossy().into_owned())
}

/// The walker's own binary, for re-exec (process substitution, and later
/// compound backgrounding). Under `cargo test` the current exe is the test
/// harness, so $BASH_WALKER_SELF names the real binary explicitly.
fn walker_exe() -> PathBuf {
    std::env::var_os("BASH_WALKER_SELF")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("bash-walker"))
}
