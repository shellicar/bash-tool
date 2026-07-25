//! The builtins that must be walker-native because they mutate the calling
//! shell's own state (docs/ast-execution.md: two-thirds of real corpus
//! invocations contain at least one — `cd` alone is 65%). `test`/`[`,
//! `echo`, `printf` are deliberately NOT here: real external binaries with
//! identical behaviour exist, so they spawn like any other command.
//! Unimplemented builtins error by name, never silently no-op.

use std::io::Read;

use crate::walk::{Ctx, Exec, Flow};

const NATIVE: &[&str] = &[
    "cd", "pwd", "export", "unset", "local", "exit", "return", "break", "continue", "shift",
    "set", "read", "wait", "eval", "source", ".", ":", "true", "false", "command", "let",
];

const UNSUPPORTED: &[&str] = &[
    "declare", "typeset", "readonly", "alias", "unalias", "trap", "getopts", "exec", "ulimit",
    "jobs", "fg", "bg", "hash", "type", "help", "history", "disown", "suspend", "times",
    "builtin", "caller", "enable", "pushd", "popd", "dirs", "umask", "mapfile", "readarray",
];

pub fn is_builtin(name: &str) -> bool {
    NATIVE.contains(&name) || UNSUPPORTED.contains(&name)
}

pub fn run(ex: &mut Exec, ctx: &Ctx, name: &str, args: &[String]) -> Result<i32, Flow> {
    match name {
        "cd" => cd(ex, ctx, args),
        "pwd" => {
            let cwd = std::env::current_dir().map_err(|e| Flow::Fatal(e.to_string()))?;
            ctx.write_out(&format!("{}\n", cwd.display()));
            Ok(0)
        }
        "export" => {
            for a in args {
                match a.split_once('=') {
                    Some((k, v)) => ex.state.export_var(k, Some(v.to_string())),
                    None => ex.state.export_var(a, None),
                }
            }
            Ok(0)
        }
        "unset" => {
            let mut names = args.iter();
            for a in names.by_ref() {
                if a == "-f" {
                    continue;
                }
                if a == "-v" {
                    continue;
                }
                ex.state.funcs.remove(a);
                ex.state.unset_var(a);
            }
            Ok(0)
        }
        "local" => {
            if ex.shared.func_depth == 0 {
                ctx.write_err("bash-walker: local: can only be used in a function\n");
                return Ok(1);
            }
            for a in args {
                match a.split_once('=') {
                    Some((k, v)) => ex.state.declare_local(k, Some(v.to_string())),
                    None => ex.state.declare_local(a, None),
                }
            }
            Ok(0)
        }
        "exit" => Err(Flow::Exit(parse_status(args.first()))),
        "return" => {
            if ex.shared.func_depth == 0 {
                ctx.write_err("bash-walker: return: can only `return' from a function or sourced script\n");
                return Ok(1);
            }
            Err(Flow::Return(parse_status(args.first())))
        }
        "break" | "continue" => {
            if ex.shared.loop_depth == 0 {
                ctx.write_err(&format!("bash-walker: {name}: only meaningful in a loop\n"));
                return Ok(0);
            }
            let n: u32 = args
                .first()
                .and_then(|a| a.parse().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(1);
            if name == "break" {
                Err(Flow::Break(n))
            } else {
                Err(Flow::Continue(n))
            }
        }
        "shift" => {
            let n: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(1);
            if n > ex.state.positional.len() {
                return Ok(1);
            }
            ex.state.positional.drain(..n);
            Ok(0)
        }
        "set" => set(ex, ctx, args),
        "read" => read(ex, ctx, args),
        "wait" => {
            let mut status = 0;
            for mut child in ex.shared.bg.drain(..) {
                status = child
                    .wait()
                    .map(|s| s.code().unwrap_or(1))
                    .unwrap_or(1);
            }
            Ok(status)
        }
        "eval" => {
            let src = args.join(" ");
            if src.trim().is_empty() {
                return Ok(0);
            }
            crate::walk::run_source(ex, ctx, &src, false)
        }
        "source" | "." => {
            let Some(path) = args.first() else {
                ctx.write_err("bash-walker: source: filename argument required\n");
                return Ok(2);
            };
            let src = std::fs::read_to_string(path)
                .map_err(|e| Flow::Fatal(format!("source {path}: {e}")))?;
            let saved = if args.len() > 1 {
                Some(std::mem::replace(&mut ex.state.positional, args[1..].to_vec()))
            } else {
                None
            };
            let r = crate::walk::run_source(ex, ctx, &src, false);
            if let Some(p) = saved {
                ex.state.positional = p;
            }
            match r {
                Err(Flow::Return(n)) => Ok(n),
                other => other,
            }
        }
        ":" | "true" => Ok(0),
        "false" => Ok(1),
        "command" => command(ex, ctx, args),
        "let" => {
            let mut v = 0;
            for a in args {
                v = crate::arith::eval(a, ex.state).map_err(|e| Flow::Fatal(e.to_string()))?;
            }
            Ok(i32::from(v == 0))
        }
        other if UNSUPPORTED.contains(&other) => Err(Flow::Fatal(format!(
            "the '{other}' builtin is not supported by bash-walker"
        ))),
        other => Err(Flow::Fatal(format!("not a builtin: {other}"))),
    }
}

fn parse_status(arg: Option<&String>) -> i32 {
    arg.and_then(|a| a.parse::<i64>().ok())
        .map(|n| (n.rem_euclid(256)) as i32)
        .unwrap_or(0)
}

fn cd(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let prev = std::env::current_dir().ok();
    let target = match args.first().map(String::as_str) {
        None => match ex.state.get_var("HOME") {
            Some(h) => h,
            None => {
                ctx.write_err("bash-walker: cd: HOME not set\n");
                return Ok(1);
            }
        },
        Some("-") => match ex.state.get_var("OLDPWD") {
            Some(p) => {
                ctx.write_out(&format!("{p}\n"));
                p
            }
            None => {
                ctx.write_err("bash-walker: cd: OLDPWD not set\n");
                return Ok(1);
            }
        },
        Some(p) => p.to_string(),
    };
    match std::env::set_current_dir(&target) {
        Ok(()) => {
            if let Some(prev) = prev {
                ex.state.export_var("OLDPWD", Some(prev.to_string_lossy().into_owned()));
            }
            if let Ok(now) = std::env::current_dir() {
                ex.state.export_var("PWD", Some(now.to_string_lossy().into_owned()));
            }
            Ok(0)
        }
        Err(e) => {
            ctx.write_err(&format!("bash-walker: cd: {target}: {e}\n"));
            Ok(1)
        }
    }
}

fn set(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--" => {
                ex.state.positional = args[i + 1..].to_vec();
                return Ok(0);
            }
            "-o" | "+o" => {
                let on = a.starts_with('-');
                i += 1;
                match args.get(i).map(String::as_str) {
                    Some("pipefail") => ex.state.flags.pipefail = on,
                    Some("errexit") => ex.state.flags.errexit = on,
                    Some("nounset") => ex.state.flags.nounset = on,
                    Some("xtrace") => ex.state.flags.xtrace = on,
                    Some(other) => {
                        return Err(Flow::Fatal(format!(
                            "set -o {other}: not supported by bash-walker"
                        )))
                    }
                    None => {
                        ctx.write_err("bash-walker: set -o: option name required\n");
                        return Ok(2);
                    }
                }
            }
            flag if flag.starts_with('-') || flag.starts_with('+') => {
                let on = flag.starts_with('-');
                for c in flag[1..].chars() {
                    match c {
                        'e' => ex.state.flags.errexit = on,
                        'x' => ex.state.flags.xtrace = on,
                        'u' => ex.state.flags.nounset = on,
                        // -f (noglob), -C, ... are behaviour changes we don't
                        // implement; failing loud beats silently differing.
                        other => {
                            return Err(Flow::Fatal(format!(
                                "set -{other}: not supported by bash-walker"
                            )))
                        }
                    }
                }
            }
            _ => {
                ex.state.positional = args[i..].to_vec();
                return Ok(0);
            }
        }
        i += 1;
    }
    Ok(0)
}

/// One line from the context's stdin, read unbuffered (byte at a time) so
/// consecutive `read`s in a loop never swallow each other's input — the
/// same reason bash reads unseekable fds byte-wise.
fn read(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut vars: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "-r" => {} // no-backslash-processing is this implementation's only mode
            flag if flag.starts_with('-') => {
                return Err(Flow::Fatal(format!(
                    "read {flag}: flag not supported by bash-walker"
                )))
            }
            name => vars.push(name),
        }
    }
    if vars.is_empty() {
        vars.push("REPLY");
    }

    let Some(stdin) = &ctx.stdin else {
        return Ok(1); // non-interactive: no stdin means EOF
    };
    let mut line = Vec::new();
    let mut got_any = false;
    let mut buf = [0u8; 1];
    let mut f = &**stdin;
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {
                got_any = true;
                if buf[0] == b'\n' {
                    break;
                }
                line.push(buf[0]);
            }
            Err(_) => break,
        }
    }
    if !got_any {
        return Ok(1);
    }
    let line = String::from_utf8_lossy(&line).into_owned();

    let ifs = ex.state.get_var("IFS").unwrap_or_else(|| " \t\n".to_string());
    if vars.len() == 1 || ifs.is_empty() {
        let trimmed = if ifs.is_empty() {
            line.as_str()
        } else {
            line.trim_matches(|c: char| ifs.contains(c) && c.is_whitespace())
        };
        ex.state.set_var(vars[0], trimmed.to_string());
        for v in &vars[1..] {
            ex.state.set_var(v, String::new());
        }
        return Ok(0);
    }
    let seps: Vec<char> = ifs.chars().collect();
    let mut fields: Vec<&str> = line
        .split(|c: char| seps.contains(&c))
        .filter(|s| !s.is_empty())
        .collect();
    let last_join;
    if fields.len() > vars.len() {
        let head = fields[..vars.len() - 1].to_vec();
        last_join = fields[vars.len() - 1..].join(" ");
        fields = head;
        fields.push(&last_join);
    }
    for (i, v) in vars.iter().enumerate() {
        ex.state.set_var(v, fields.get(i).copied().unwrap_or("").to_string());
    }
    Ok(0)
}

fn command(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    match args.first().map(String::as_str) {
        Some("-v") => {
            let Some(name) = args.get(1) else {
                return Ok(1);
            };
            if ex.state.funcs.contains_key(name) || is_builtin(name) {
                ctx.write_out(&format!("{name}\n"));
                return Ok(0);
            }
            match path_lookup(name) {
                Some(p) => {
                    ctx.write_out(&format!("{p}\n"));
                    Ok(0)
                }
                None => Ok(1),
            }
        }
        Some(_) => Err(Flow::Fatal(
            "command (other than -v) is not supported by bash-walker".into(),
        )),
        None => Ok(0),
    }
}

fn path_lookup(name: &str) -> Option<String> {
    if name.contains('/') {
        return std::fs::metadata(name).ok().map(|_| name.to_string());
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let cand = std::path::Path::new(dir).join(name);
        if let Ok(md) = cand.metadata() {
            use std::os::unix::fs::PermissionsExt;
            if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}
