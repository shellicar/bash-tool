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
    "echo", "printf", "exec", "umask",
];

const UNSUPPORTED: &[&str] = &[
    "declare", "typeset", "readonly", "alias", "unalias", "trap", "getopts", "ulimit",
    "jobs", "fg", "bg", "hash", "type", "help", "history", "disown", "suspend", "times",
    "builtin", "caller", "enable", "pushd", "popd", "dirs", "mapfile", "readarray",
];

pub fn is_builtin(name: &str) -> bool {
    NATIVE.contains(&name) || UNSUPPORTED.contains(&name)
}

pub fn run(ex: &mut Exec, ctx: &Ctx, name: &str, args: &[String]) -> Result<i32, Flow> {
    match name {
        "cd" => cd(ex, ctx, args),
        "pwd" => {
            ctx.write_out(&format!("{}\n", ex.state.cwd.display()));
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
            let src = std::fs::read_to_string(ex.state.resolve(path))
                .map_err(|e| Flow::Fatal(format!("source {path}: {}", crate::walk::errmsg(&e))))?;
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
        // Native because the recordings and real bash use the BUILTIN echo
        // and printf; the external binaries (BSD ones in particular)
        // diverge on flags, escapes, and error shapes.
        "echo" => echo(ctx, args),
        "printf" => printf(ex, ctx, args),
        "command" => command(ex, ctx, args),
        "umask" => umask(ex, ctx, args),
        // `exec` with only redirects rewires the shell itself for the rest
        // of the invocation (the redirects were already applied into this
        // ctx); with a command it replaces the shell: run it, then the
        // shell exits with its status.
        "exec" => {
            if args.is_empty() {
                let mut c = ctx.clone();
                c.derived = false;
                ex.shared.persistent_ctx = Some(c);
                Ok(0)
            } else {
                let st = ex.run_external_wait(args, ctx)?;
                Err(Flow::Exit(st))
            }
        }
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

fn echo(ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut newline = true;
    let mut escapes = false;
    let mut i = 0;
    // Only pure combinations of n/e/E are flags; anything else (including
    // `--`) prints as an ordinary argument, exactly like bash.
    while i < args.len() {
        let a = &args[i];
        if a.len() >= 2 && a.starts_with('-') && a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            for c in a[1..].chars() {
                match c {
                    'n' => newline = false,
                    'e' => escapes = true,
                    'E' => escapes = false,
                    _ => unreachable!(),
                }
            }
            i += 1;
        } else {
            break;
        }
    }
    let joined = args[i..].join(" ");
    let mut out = String::new();
    let mut suppress_newline = !newline;
    if escapes {
        let b: Vec<char> = joined.chars().collect();
        let mut k = 0;
        'outer: while k < b.len() {
            if b[k] == '\\' && k + 1 < b.len() {
                let (c, used) = match b[k + 1] {
                    'n' => ('\n', 2),
                    't' => ('\t', 2),
                    'r' => ('\r', 2),
                    'a' => ('\x07', 2),
                    'b' => ('\x08', 2),
                    'e' | 'E' => ('\x1b', 2),
                    'f' => ('\x0c', 2),
                    'v' => ('\x0b', 2),
                    '\\' => ('\\', 2),
                    'c' => {
                        // \c: stop all output, no trailing newline
                        suppress_newline = true;
                        break 'outer;
                    }
                    '0' => {
                        let oct: String = b[k + 2..]
                            .iter()
                            .copied()
                            .take(3)
                            .take_while(|c| c.is_digit(8))
                            .collect();
                        let v = u8::from_str_radix(&oct, 8).unwrap_or(0);
                        out.push(v as char);
                        k += 2 + oct.len();
                        continue;
                    }
                    'x' => {
                        let hex: String = b[k + 2..]
                            .iter()
                            .copied()
                            .take(2)
                            .take_while(|c| c.is_ascii_hexdigit())
                            .collect();
                        if hex.is_empty() {
                            out.push('\\');
                            out.push('x');
                            k += 2;
                            continue;
                        }
                        let v = u8::from_str_radix(&hex, 16).unwrap_or(0);
                        out.push(v as char);
                        k += 2 + hex.len();
                        continue;
                    }
                    other => {
                        out.push('\\');
                        out.push(other);
                        k += 2;
                        continue;
                    }
                };
                out.push(c);
                k += used;
            } else {
                out.push(b[k]);
                k += 1;
            }
        }
    } else {
        out = joined;
    }
    if !suppress_newline {
        out.push('\n');
    }
    ctx.write_out_pipeaware(&out)?;
    Ok(0)
}

fn printf(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut i = 0;
    let mut var_target: Option<String> = None;
    match args.first().map(String::as_str) {
        Some("-v") => {
            let Some(v) = args.get(1) else {
                ctx.write_err("bash-walker: printf: -v: option requires an argument\n");
                return Ok(2);
            };
            var_target = Some(v.clone());
            i = 2;
        }
        Some("--") => i = 1,
        Some(a) if a.starts_with('-') && a.len() > 1 => {
            // bash reports the offending option as `--` for `--...`, else -X
            let opt = if a.starts_with("--") { "--".to_string() } else { a[..2].to_string() };
            ctx.write_err(&format!(
                "bash-walker: printf: {opt}: invalid option\nprintf: usage: printf [-v var] format [arguments]\n"
            ));
            return Ok(2);
        }
        _ => {}
    }
    let Some(format) = args.get(i) else {
        ctx.write_err("bash-walker: printf: usage: printf [-v var] format [arguments]\n");
        return Ok(2);
    };
    let rest = &args[i + 1..];
    let mut out = String::new();
    let mut status = 0;
    let mut argi = 0;
    loop {
        let before = argi;
        let stop = render_format(format, rest, &mut argi, &mut out, &mut status, ctx);
        if stop || argi >= rest.len() || argi == before {
            break;
        }
    }
    match var_target {
        Some(v) => ex.state.set_var(&v, out),
        None => ctx.write_out_pipeaware(&out)?,
    }
    Ok(status)
}

/// One pass over the format string; returns true on `\c` (stop everything).
fn render_format(
    format: &str,
    args: &[String],
    argi: &mut usize,
    out: &mut String,
    status: &mut i32,
    ctx: &Ctx,
) -> bool {
    let chars: Vec<char> = format.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        match chars[k] {
            '\\' if k + 1 < chars.len() => {
                let (decoded, used, stop) = decode_escape(&chars[k..]);
                if stop {
                    return true;
                }
                out.push_str(&decoded);
                k += used;
            }
            '%' if k + 1 < chars.len() && chars[k + 1] == '%' => {
                out.push('%');
                k += 2;
            }
            '%' => {
                let spec_start = k;
                k += 1;
                let mut flags = String::new();
                while k < chars.len() && matches!(chars[k], '-' | '+' | ' ' | '#' | '0') {
                    flags.push(chars[k]);
                    k += 1;
                }
                let mut width = String::new();
                if k < chars.len() && chars[k] == '*' {
                    width = next_arg(args, argi).unwrap_or_default();
                    k += 1;
                } else {
                    while k < chars.len() && chars[k].is_ascii_digit() {
                        width.push(chars[k]);
                        k += 1;
                    }
                }
                let mut precision: Option<String> = None;
                if k < chars.len() && chars[k] == '.' {
                    k += 1;
                    if k < chars.len() && chars[k] == '*' {
                        precision = Some(next_arg(args, argi).unwrap_or_default());
                        k += 1;
                    } else {
                        let mut p = String::new();
                        while k < chars.len() && chars[k].is_ascii_digit() {
                            p.push(chars[k]);
                            k += 1;
                        }
                        precision = Some(p);
                    }
                }
                let Some(conv) = chars.get(k).copied() else {
                    // trailing bare % prints literally, like bash
                    out.push_str(&chars[spec_start..].iter().collect::<String>());
                    break;
                };
                k += 1;
                let width: Option<i64> = width.parse().ok();
                let prec: Option<usize> = precision.and_then(|p| p.parse().ok().or(Some(0)));
                render_conversion(conv, &flags, width, prec, args, argi, out, status, ctx);
            }
            c => {
                out.push(c);
                k += 1;
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn render_conversion(
    conv: char,
    flags: &str,
    width: Option<i64>,
    prec: Option<usize>,
    args: &[String],
    argi: &mut usize,
    out: &mut String,
    status: &mut i32,
    ctx: &Ctx,
) {
    let body = match conv {
        's' => {
            let a = next_arg(args, argi).unwrap_or_default();
            match prec {
                Some(p) => a.chars().take(p).collect(),
                None => a,
            }
        }
        'b' => {
            let a = next_arg(args, argi).unwrap_or_default();
            let chars: Vec<char> = a.chars().collect();
            let mut s = String::new();
            let mut j = 0;
            while j < chars.len() {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    let (decoded, used, stop) = decode_escape(&chars[j..]);
                    if stop {
                        break;
                    }
                    s.push_str(&decoded);
                    j += used;
                } else {
                    s.push(chars[j]);
                    j += 1;
                }
            }
            s
        }
        'q' => shell_quote(&next_arg(args, argi).unwrap_or_default()),
        'c' => next_arg(args, argi)
            .unwrap_or_default()
            .chars()
            .next()
            .map(String::from)
            .unwrap_or_default(),
        'd' | 'i' | 'u' | 'o' | 'x' | 'X' => {
            let a = next_arg(args, argi).unwrap_or_default();
            let v = parse_printf_int(&a).unwrap_or_else(|| {
                if !a.is_empty() {
                    ctx.write_err(&format!("bash-walker: printf: {a}: invalid number\n"));
                    *status = 1;
                }
                0
            });
            let mut s = match conv {
                'o' => format!("{v:o}"),
                'x' => format!("{v:x}"),
                'X' => format!("{v:X}"),
                _ => v.abs().to_string(),
            };
            if matches!(conv, 'd' | 'i' | 'u') {
                if v < 0 {
                    s = format!("-{s}");
                } else if flags.contains('+') {
                    s = format!("+{s}");
                } else if flags.contains(' ') {
                    s = format!(" {s}");
                }
            } else if flags.contains('#') && v != 0 {
                s = match conv {
                    'o' => format!("0{s}"),
                    'x' => format!("0x{s}"),
                    'X' => format!("0X{s}"),
                    _ => s,
                };
            }
            return pad_number(out, &s, flags, width);
        }
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' => {
            let a = next_arg(args, argi).unwrap_or_default();
            let v: f64 = a.trim().parse().unwrap_or_else(|_| {
                if !a.is_empty() {
                    ctx.write_err(&format!("bash-walker: printf: {a}: invalid number\n"));
                    *status = 1;
                }
                0.0
            });
            // NaN/Infinity: bash prints "nan"/"inf" (case follows the
            // conversion letter) and never zero-pads them — found live,
            // the walker printed "NaN" (Rust's Display) and zero-padded it
            // like an ordinary number under %015f.
            if v.is_nan() || v.is_infinite() {
                let word = if v.is_nan() {
                    "nan"
                } else if v.is_sign_negative() {
                    "-inf"
                } else {
                    "inf"
                };
                let word = if conv.is_uppercase() { word.to_uppercase() } else { word.to_string() };
                let flags_no_zero: String = flags.chars().filter(|&c| c != '0').collect();
                return pad_number(out, &word, &flags_no_zero, width);
            }
            let p = prec.unwrap_or(6);
            let s = match conv {
                'f' | 'F' => format!("{v:.p$}"),
                'e' | 'E' => {
                    let s = format!("{v:.p$e}");
                    let s = c_style_exponent(&s);
                    if conv == 'E' { s.to_uppercase() } else { s }
                }
                _ => {
                    // %g: shortest of %e/%f with trailing zeros trimmed
                    let s = format!("{v}");
                    if conv == 'G' { s.to_uppercase() } else { s }
                }
            };
            return pad_number(out, &s, flags, width);
        }
        other => {
            ctx.write_err(&format!("bash-walker: printf: `{other}': invalid format character\n"));
            *status = 1;
            return;
        }
    };
    // string-like padding
    let w = width.unwrap_or(0).unsigned_abs() as usize;
    let left = flags.contains('-') || width.is_some_and(|w| w < 0);
    let len = body.chars().count();
    if len >= w {
        out.push_str(&body);
    } else if left {
        out.push_str(&body);
        out.extend(std::iter::repeat_n(' ', w - len));
    } else {
        out.extend(std::iter::repeat_n(' ', w - len));
        out.push_str(&body);
    }
}

fn pad_number(out: &mut String, s: &str, flags: &str, width: Option<i64>) {
    let w = width.unwrap_or(0).unsigned_abs() as usize;
    let left = flags.contains('-') || width.is_some_and(|w| w < 0);
    let len = s.chars().count();
    if len >= w {
        out.push_str(s);
    } else if left {
        out.push_str(s);
        out.extend(std::iter::repeat_n(' ', w - len));
    } else if flags.contains('0') {
        // zero-padding goes between the sign and the digits
        let (sign, digits) = match s.strip_prefix(['-', '+', ' ']) {
            Some(d) => (&s[..1], d),
            None => ("", s),
        };
        out.push_str(sign);
        out.extend(std::iter::repeat_n('0', w - len));
        out.push_str(digits);
    } else {
        out.extend(std::iter::repeat_n(' ', w - len));
        out.push_str(s);
    }
}

fn next_arg(args: &[String], argi: &mut usize) -> Option<String> {
    let a = args.get(*argi).cloned();
    if a.is_some() {
        *argi += 1;
    }
    a
}

/// bash printf integer parsing: strtoll base 0 (0x hex, leading-0 octal),
/// plus the `'A` form meaning the character's code point.
fn parse_printf_int(a: &str) -> Option<i64> {
    let t = a.trim();
    if let Some(rest) = t.strip_prefix('\'').or_else(|| t.strip_prefix('"')) {
        return rest.chars().next().map(|c| c as i64);
    }
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()?
    } else if t.len() > 1 && t.starts_with('0') {
        i64::from_str_radix(&t[1..], 8).ok()?
    } else {
        t.parse().ok()?
    };
    Some(if neg { -v } else { v })
}

fn c_style_exponent(s: &str) -> String {
    // Rust: "1.5e2" / "1.5e-2"; C: "1.5e+02" / "1.5e-02"
    match s.split_once('e') {
        Some((m, exp)) => {
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            format!("{m}e{sign}{digits:0>2}")
        }
        None => s.to_string(),
    }
}

/// `%q`: bash's backslash-quoting; control characters force $'...' form.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().any(|c| (c as u32) < 0x20 || c == '\x7f') {
        let mut q = String::from("$'");
        for c in s.chars() {
            match c {
                '\n' => q.push_str("\\n"),
                '\t' => q.push_str("\\t"),
                '\r' => q.push_str("\\r"),
                '\'' => q.push_str("\\'"),
                '\\' => q.push_str("\\\\"),
                c if (c as u32) < 0x20 => q.push_str(&format!("\\{:03o}", c as u32)),
                c => q.push(c),
            }
        }
        q.push('\'');
        return q;
    }
    let mut q = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '@' | '+' | '%' | ',' | '^') || !c.is_ascii() {
            q.push(c);
        } else {
            q.push('\\');
            q.push(c);
        }
    }
    q
}

/// printf/echo escape at `chars[0] == '\\'`: (decoded, chars consumed,
/// stop-output). `\c` in printf's %b and echo -e stops everything.
fn decode_escape(chars: &[char]) -> (String, usize, bool) {
    match chars.get(1) {
        None => ("\\".to_string(), 1, false),
        Some('n') => ("\n".into(), 2, false),
        Some('t') => ("\t".into(), 2, false),
        Some('r') => ("\r".into(), 2, false),
        Some('a') => ("\x07".into(), 2, false),
        Some('b') => ("\x08".into(), 2, false),
        Some('e') | Some('E') => ("\x1b".into(), 2, false),
        Some('f') => ("\x0c".into(), 2, false),
        Some('v') => ("\x0b".into(), 2, false),
        Some('\\') => ("\\".into(), 2, false),
        Some('"') => ("\"".into(), 2, false),
        Some('\'') => ("'".into(), 2, false),
        Some('c') => (String::new(), 2, true),
        Some('x') => {
            let hex: String = chars[2..]
                .iter()
                .copied()
                .take(2)
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if hex.is_empty() {
                ("\\x".into(), 2, false)
            } else {
                let v = u8::from_str_radix(&hex, 16).unwrap_or(0);
                ((v as char).to_string(), 2 + hex.len(), false)
            }
        }
        Some(d) if d.is_digit(8) => {
            let oct: String = chars[1..]
                .iter()
                .copied()
                .take(3)
                .take_while(|c| c.is_digit(8))
                .collect();
            let v = u32::from_str_radix(&oct, 8).unwrap_or(0) & 0xff;
            (
                char::from_u32(v).unwrap_or('\0').to_string(),
                1 + oct.len(),
                false,
            )
        }
        Some(other) => (format!("\\{other}"), 2, false),
    }
}

fn parse_status(arg: Option<&String>) -> i32 {
    arg.and_then(|a| a.parse::<i64>().ok())
        .map(|n| (n.rem_euclid(256)) as i32)
        .unwrap_or(0)
}

/// A mutation of the shell's cwd field — validated against the filesystem,
/// but never chdir: the process cwd is not shell state.
fn cd(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
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
    let resolved = crate::state::normalize(&ex.state.resolve(&target));
    match std::fs::metadata(&resolved) {
        Ok(m) if m.is_dir() => {
            let prev = ex.state.cwd.clone();
            ex.state.export_var("OLDPWD", Some(prev.to_string_lossy().into_owned()));
            ex.state.export_var("PWD", Some(resolved.to_string_lossy().into_owned()));
            ex.state.cwd = resolved;
            Ok(0)
        }
        Ok(_) => {
            ctx.write_err(&format!("bash-walker: cd: {target}: Not a directory\n"));
            Ok(1)
        }
        Err(e) => {
            ctx.write_err(&format!("bash-walker: cd: {target}: {}\n", crate::walk::errmsg(&e)));
            Ok(1)
        }
    }
}

/// `umask [-S] [mode]` — query or set the file-creation mask. No arg
/// prints the current mask (bash's default `%04o` form, e.g. `0022`); `-S`
/// prints the symbolic form bash uses (`u=rwx,g=rx,o=rx`).
fn umask(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let symbolic = args.first().map(String::as_str) == Some("-S");
    let rest = if symbolic { &args[1..] } else { args };
    match rest.first() {
        None => {
            if symbolic {
                ctx.write_out(&format!("{}\n", symbolic_umask(ex.state.umask)));
            } else {
                ctx.write_out(&format!("{:04o}\n", ex.state.umask));
            }
            Ok(0)
        }
        Some(m) => match u32::from_str_radix(m, 8) {
            Ok(v) if v <= 0o777 => {
                ex.state.umask = v;
                Ok(0)
            }
            _ => {
                ctx.write_err(&format!("bash-walker: umask: {m}: octal number out of range\n"));
                Ok(1)
            }
        },
    }
}

fn symbolic_umask(mask: u32) -> String {
    let perm = |shift: u32| {
        let bits = 0o7 & !(mask >> shift);
        format!(
            "{}{}{}",
            if bits & 0b100 != 0 { "r" } else { "" },
            if bits & 0b010 != 0 { "w" } else { "" },
            if bits & 0b001 != 0 { "x" } else { "" },
        )
    };
    format!("u={},g={},o={}", perm(6), perm(3), perm(0))
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
            match path_lookup(ex, name) {
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

fn path_lookup(ex: &Exec, name: &str) -> Option<String> {
    if name.contains('/') {
        return std::fs::metadata(ex.state.resolve(name)).ok().map(|_| name.to_string());
    }
    let path = ex.state.get_var("PATH")?;
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
