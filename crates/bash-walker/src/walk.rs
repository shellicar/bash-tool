//! The tree executor. External commands spawn as argv (never a shell
//! string); state-mutating constructs run as walker logic against the shared
//! `ShellState`. Output discipline: one shared append-mode capture file for
//! stdout+stderr of everything (no drain threads, no deadlock surface);
//! pipes exist only BETWEEN external children. Pipeline stages are
//! subshells, exactly like bash: each stage runs against a cloned state, so
//! `... | while read` not persisting variables is authentic, not a gap.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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
}

impl Ctx {
    pub fn write_err(&self, msg: &str) {
        let _ = (&*self.stderr).write_all(msg.as_bytes());
    }
    pub fn write_out(&self, msg: &str) {
        let _ = (&*self.stdout).write_all(msg.as_bytes());
    }
}

/// State the whole invocation shares regardless of subshell nesting.
#[derive(Default)]
pub struct Shared {
    pub bg: Vec<Child>,
    pub procsub_temps: Vec<PathBuf>,
    pub temp_counter: u64,
    pub func_depth: u32,
    pub loop_depth: u32,
    /// Status of the most recent command substitution — an assignment-only
    /// command's status in bash (`x=$(false); echo $?` is 1).
    pub last_capture_status: Option<i32>,
}

pub struct Exec<'a> {
    pub state: &'a mut ShellState,
    pub shared: &'a mut Shared,
}

const DECL_BUILTINS: &[&str] = &["export", "local", "declare", "readonly", "typeset"];

impl<'a> Exec<'a> {
    /// An anonymous temp file: created then immediately unlinked, alive only
    /// through its handle — capture buffers and heredoc feeds need no
    /// cleanup pass.
    pub fn anon_temp(&mut self) -> Result<File, Flow> {
        self.shared.temp_counter += 1;
        let path = std::env::temp_dir().join(format!(
            "bash-walker-{}-{}",
            std::process::id(),
            self.shared.temp_counter
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
        let status = self.exec_inner(cmd, ctx, tested)?;
        self.state.last_status = status;
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
                let start = std::time::Instant::now();
                let st = self.exec(inner, ctx, tested)?;
                let secs = start.elapsed().as_secs_f64();
                // user/sys would need wait4 rusage plumbing; real is measured.
                ctx.write_err(&format!(
                    "\nreal\t{}m{:.3}s\nuser\t0m0.000s\nsys\t0m0.000s\n",
                    (secs / 60.0) as u64,
                    secs % 60.0
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
            Connector::SeqAsync => {
                self.exec_background(&c.left, ctx)?;
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
        let n = stages.len();
        let mut statuses: Vec<Option<i32>> = vec![None; n];
        let mut waiting: Vec<Option<Child>> = Vec::with_capacity(n);
        let mut next_stdin: Option<Arc<File>> = None;

        for (i, stage) in stages.iter().enumerate() {
            let last = i + 1 == n;
            let stage_stdin = if i == 0 { ctx.stdin.clone() } else { next_stdin.take() };

            let saved_cwd = std::env::current_dir().ok();
            let mut stage_state = self.state.clone();
            let mut sub = Exec { state: &mut stage_state, shared: &mut *self.shared };

            // An external simple command spawns and stays concurrent.
            if let Command::Simple(s) = stage {
                match sub.prepare_simple(s, ctx)? {
                    Prepared::External { fields, assigns, redirects } => {
                        let stage_ctx = Ctx { stdin: stage_stdin, ..ctx.clone() };
                        let stage_ctx = match sub.apply_redirects(&redirects, &stage_ctx) {
                            Err(Flow::RedirectFailed(msg)) => {
                                ctx.write_err(&format!("bash-walker: {msg}\n"));
                                statuses[i] = Some(1);
                                waiting.push(None);
                                if !last {
                                    let empty = sub.anon_temp()?;
                                    next_stdin = Some(Arc::new(empty));
                                }
                                if let Some(c) = saved_cwd {
                                    let _ = std::env::set_current_dir(c);
                                }
                                continue;
                            }
                            other => other?,
                        };
                        let stdout_to = if last {
                            SpawnOut::Ctx
                        } else {
                            SpawnOut::Pipe
                        };
                        let mut child =
                            sub.spawn(&fields, &assigns, &stage_ctx, stdout_to, ctx)?;
                        match child.take() {
                            SpawnResult::Child(mut ch) => {
                                if !last {
                                    let out = ch.stdout.take().expect("stdout was piped");
                                    next_stdin =
                                        Some(Arc::new(File::from(std::os::fd::OwnedFd::from(out))));
                                }
                                waiting.push(Some(ch));
                            }
                            SpawnResult::Failed(st) => {
                                statuses[i] = Some(st);
                                waiting.push(None);
                                if !last {
                                    // downstream reads EOF
                                    let empty = sub.anon_temp()?;
                                    next_stdin = Some(Arc::new(empty));
                                }
                            }
                        }
                        if let Some(c) = saved_cwd {
                            let _ = std::env::set_current_dir(c);
                        }
                        continue;
                    }
                    Prepared::Internal => {} // falls through to the inline path
                }
            }

            // Internal stage: run inline against the cloned state.
            let (out_arc, capture): (Arc<File>, Option<File>) = if last {
                (Arc::clone(&ctx.stdout), None)
            } else {
                let f = sub.anon_temp()?;
                let h = f.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?;
                (Arc::new(f), Some(h))
            };
            let stage_ctx = Ctx {
                stdin: stage_stdin,
                stdout: out_arc,
                stderr: Arc::clone(&ctx.stderr),
            };
            let st = match sub.exec(stage, &stage_ctx, true) {
                Ok(st) => st,
                Err(Flow::Exit(st)) => st,
                Err(other) => return Err(other),
            };
            statuses[i] = Some(st);
            waiting.push(None);
            if let Some(mut h) = capture {
                let _ = h.seek(SeekFrom::Start(0));
                next_stdin = Some(Arc::new(h));
            }
            if let Some(c) = saved_cwd {
                let _ = std::env::set_current_dir(c);
            }
        }

        for (i, slot) in waiting.iter_mut().enumerate() {
            if let Some(child) = slot {
                let st = child
                    .wait()
                    .map(|s| s.code().unwrap_or(128 + s_signal(&s)))
                    .unwrap_or(1);
                statuses[i] = Some(st);
            }
        }

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
        let saved_cwd = std::env::current_dir().ok();
        let mut sub_state = self.state.clone();
        let mut sub = Exec { state: &mut sub_state, shared: &mut *self.shared };
        let r = sub.exec(inner, ctx, tested);
        let capture_status = sub_state.last_status;
        if let Some(c) = saved_cwd {
            let _ = std::env::set_current_dir(c);
        }
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
        for v in values {
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
        let mut stages = Vec::new();
        flatten_pipeline(inner, &mut stages);
        let mut simples = Vec::new();
        for s in &stages {
            match s {
                Command::Simple(sc) => simples.push(sc),
                _ => {
                    return Err(Flow::Fatal(
                        "backgrounding a compound command is not supported by bash-walker".into(),
                    ))
                }
            }
        }
        let saved_cwd = std::env::current_dir().ok();
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
                    let stage_ctx = Ctx { stdin: stage_stdin, ..ctx.clone() };
                    let stage_ctx = match sub.apply_redirects(&redirects, &stage_ctx) {
                        Err(Flow::RedirectFailed(msg)) => {
                            ctx.write_err(&format!("bash-walker: {msg}\n"));
                            continue;
                        }
                        other => other?,
                    };
                    let out = if last { SpawnOut::Ctx } else { SpawnOut::Pipe };
                    match sub.spawn(&fields, &assigns, &stage_ctx, out, ctx)?.take() {
                        SpawnResult::Child(mut ch) => {
                            if !last {
                                let o = ch.stdout.take().expect("stdout was piped");
                                next_stdin =
                                    Some(Arc::new(File::from(std::os::fd::OwnedFd::from(o))));
                            }
                            last_pid = Some(ch.id());
                            sub.shared.bg.push(ch);
                        }
                        SpawnResult::Failed(_) => {}
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
        if let Some(c) = saved_cwd {
            let _ = std::env::set_current_dir(c);
        }
        self.state.last_background_pid = last_pid;
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
                self.state.set_var(&k, v);
            }
            return Ok(self.shared.last_capture_status.unwrap_or(0));
        }

        if self.state.flags.xtrace {
            ctx2.write_err(&format!("+ {}\n", fields.join(" ")));
        }

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
        match self.spawn(&fields, &assigns, &ctx2, SpawnOut::Ctx, ctx)?.take() {
            SpawnResult::Child(mut ch) => Ok(ch
                .wait()
                .map(|st| st.code().unwrap_or(128 + s_signal(&st)))
                .unwrap_or(1)),
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

    fn spawn(
        &mut self,
        fields: &[String],
        assigns: &[(String, String)],
        ctx: &Ctx,
        out: SpawnOut,
        report_ctx: &Ctx,
    ) -> Result<SpawnSlot, Flow> {
        let mut cmd = Proc::new(&fields[0]);
        cmd.args(&fields[1..]);
        for (k, v) in assigns {
            cmd.env(k, v);
        }
        cmd.stdin(match &ctx.stdin {
            Some(f) => Stdio::from(f.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?),
            None => Stdio::null(),
        });
        cmd.stdout(match out {
            SpawnOut::Ctx => Stdio::from(
                ctx.stdout
                    .try_clone()
                    .map_err(|e| Flow::Fatal(e.to_string()))?,
            ),
            SpawnOut::Pipe => Stdio::piped(),
        });
        cmd.stderr(Stdio::from(
            ctx.stderr
                .try_clone()
                .map_err(|e| Flow::Fatal(e.to_string()))?,
        ));
        match cmd.spawn() {
            Ok(ch) => Ok(SpawnSlot(Some(SpawnResult::Child(ch)))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report_ctx.write_err(&format!("bash-walker: {}: command not found\n", fields[0]));
                Ok(SpawnSlot(Some(SpawnResult::Failed(127))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                report_ctx.write_err(&format!("bash-walker: {}: permission denied\n", fields[0]));
                Ok(SpawnSlot(Some(SpawnResult::Failed(126))))
            }
            Err(e) => {
                report_ctx.write_err(&format!("bash-walker: {}: {e}\n", fields[0]));
                Ok(SpawnSlot(Some(SpawnResult::Failed(1))))
            }
        }
    }

    fn apply_redirects(&mut self, redirects: &[Redirect], ctx: &Ctx) -> Result<Ctx, Flow> {
        let mut c = ctx.clone();
        for r in redirects {
            match r.op {
                RedirectOp::Out | RedirectOp::Append => {
                    let path = expand::expand_redirect_target(self, &c, &r.target)?;
                    let f = std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .append(r.op == RedirectOp::Append)
                        .truncate(r.op == RedirectOp::Out)
                        .open(&path)
                        .map_err(|e| Flow::RedirectFailed(format!("{path}: {}", errmsg(&e))))?;
                    match r.fd {
                        None | Some(1) => c.stdout = Arc::new(f),
                        Some(2) => c.stderr = Arc::new(f),
                        Some(n) => {
                            return Err(Flow::Fatal(format!(
                                "{n}>: only fds 1 and 2 are supported by bash-walker"
                            )))
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
                        .open(&path)
                        .map_err(|e| Flow::RedirectFailed(format!("{path}: {}", errmsg(&e))))?;
                    let f = Arc::new(f);
                    c.stdout = Arc::clone(&f);
                    c.stderr = f;
                }
                RedirectOp::In => {
                    let path = expand::expand_redirect_target(self, &c, &r.target)?;
                    let f = File::open(&path)
                        .map_err(|e| Flow::RedirectFailed(format!("{path}: {}", errmsg(&e))))?;
                    c.stdin = Some(Arc::new(f));
                }
                RedirectOp::DupOut => {
                    let target = expand::expand_redirect_target(self, &c, &r.target)?;
                    let src = r.fd.unwrap_or(1);
                    match target.as_str() {
                        "1" => match src {
                            1 => {}
                            2 => c.stderr = Arc::clone(&c.stdout),
                            n => {
                                return Err(Flow::Fatal(format!(
                                    "{n}>&1: only fds 1 and 2 are supported by bash-walker"
                                )))
                            }
                        },
                        "2" => match src {
                            2 => {}
                            1 => c.stdout = Arc::clone(&c.stderr),
                            n => {
                                return Err(Flow::Fatal(format!(
                                    "{n}>&2: only fds 1 and 2 are supported by bash-walker"
                                )))
                            }
                        },
                        "-" => {
                            return Err(Flow::Fatal(
                                ">&-: closing fds is not supported by bash-walker".into(),
                            ))
                        }
                        // `>& file` (no fd, non-numeric): both streams to it.
                        path if r.fd.is_none() => {
                            let f = std::fs::OpenOptions::new()
                                .create(true)
                                .write(true)
                                .truncate(true)
                                .open(path)
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

enum SpawnOut {
    Ctx,
    Pipe,
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

fn flatten_pipeline(cmd: &Command, out: &mut Vec<Command>) {
    match cmd {
        Command::Connection(Connection { connector: Connector::Pipe, left, right }) => {
            flatten_pipeline(left, out);
            out.push((**right).clone());
        }
        other => out.push(other.clone()),
    }
}

/// Parse and execute a source string in the current shell (top level, and
/// the `eval`/`source` builtins).
pub fn run_source(ex: &mut Exec, ctx: &Ctx, src: &str, tested: bool) -> Result<i32, Flow> {
    let cmd = bash_parser::parse(src)
        .map_err(|e| Flow::Fatal(format!("syntax error: {e}")))?;
    ex.exec(&cmd, ctx, tested)
}

/// `$(...)`: run in a subshell, capture stdout, report the status back for
/// `$?` and assignment-only commands.
pub fn run_capture(ex: &mut Exec, ctx: &Ctx, src: &str) -> Result<String, Flow> {
    let capture = ex.anon_temp()?;
    let handle = capture.try_clone().map_err(|e| Flow::Fatal(e.to_string()))?;
    let saved_cwd = std::env::current_dir().ok();
    let mut sub_state = ex.state.clone();
    let status = {
        let mut sub = Exec { state: &mut sub_state, shared: &mut *ex.shared };
        let sub_ctx = Ctx {
            stdin: ctx.stdin.clone(),
            stdout: Arc::new(capture),
            stderr: Arc::clone(&ctx.stderr),
        };
        match run_source(&mut sub, &sub_ctx, src, true) {
            Ok(st) => st,
            Err(Flow::Exit(st)) => st,
            Err(f) => return Err(f),
        }
    };
    if let Some(c) = saved_cwd {
        let _ = std::env::set_current_dir(c);
    }
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

/// `<(...)`: run to completion into a NAMED temp file and substitute its
/// path. Output-equivalent to bash's FIFO for finite streams; the file is
/// cleaned up when the invocation ends.
pub fn run_procsub(ex: &mut Exec, ctx: &Ctx, src: &str) -> Result<String, Flow> {
    ex.shared.temp_counter += 1;
    let path = std::env::temp_dir().join(format!(
        "bash-walker-procsub-{}-{}",
        std::process::id(),
        ex.shared.temp_counter
    ));
    let f = File::create(&path).map_err(|e| Flow::Fatal(e.to_string()))?;
    ex.shared.procsub_temps.push(path.clone());
    let saved_cwd = std::env::current_dir().ok();
    let mut sub_state = ex.state.clone();
    {
        let mut sub = Exec { state: &mut sub_state, shared: &mut *ex.shared };
        let sub_ctx = Ctx {
            stdin: None,
            stdout: Arc::new(f),
            stderr: Arc::clone(&ctx.stderr),
        };
        match run_source(&mut sub, &sub_ctx, src, true) {
            Ok(_) | Err(Flow::Exit(_)) => {}
            Err(f) => return Err(f),
        }
    }
    if let Some(c) = saved_cwd {
        let _ = std::env::set_current_dir(c);
    }
    Ok(path.to_string_lossy().into_owned())
}
