//! The builtins that must be walker-native because they mutate the calling
//! shell's own state (docs/ast-execution.md: two-thirds of real corpus
//! invocations contain at least one — `cd` alone is 65%). `test`/`[`,
//! `echo`, `printf` are deliberately NOT here: real external binaries with
//! identical behaviour exist, so they spawn like any other command.
//! Unimplemented builtins error by name, never silently no-op.

use std::io::Read;

use crate::state::{AssignError, Attrs};
use crate::walk::{Ctx, Exec, Flow};

const NATIVE: &[&str] = &[
    "cd", "pwd", "export", "unset", "local", "exit", "return", "break", "continue", "shift",
    "set", "read", "wait", "eval", "source", ".", ":", "true", "false", "command", "let",
    "echo", "printf", "exec", "umask", "builtin", "declare", "typeset", "readonly",
    "shopt", "getopts", "type", "hash", "alias", "unalias", "dirs", "pushd", "popd",
    "compgen", "ulimit", "trap",
];

const UNSUPPORTED: &[&str] = &[
    "jobs", "fg", "bg", "help", "history", "disown", "suspend", "times",
    "caller", "enable", "mapfile", "readarray", "complete", "compopt",
];

pub fn is_builtin(name: &str) -> bool {
    NATIVE.contains(&name) || UNSUPPORTED.contains(&name)
}

/// `tested` is the caller's errexit context, which `eval` and `source` carry
/// into the code they run: `if eval false` is a condition all the way down.
pub fn run(
    ex: &mut Exec,
    ctx: &Ctx,
    name: &str,
    args: &[String],
    tested: bool,
) -> Result<i32, Flow> {
    match name {
        "cd" => cd(ex, ctx, args),
        "pwd" => {
            ctx.write_out(&format!("{}\n", ex.state.cwd.display()));
            Ok(0)
        }
        "export" | "declare" | "typeset" | "readonly" => declare(ex, ctx, name, args),
        "unset" => unset(ex, ctx, args),
        "local" => {
            if ex.shared.func_depth == 0 {
                ctx.write_err("bash-walker: local: can only be used in a function\n");
                return Ok(1);
            }
            declare(ex, ctx, "local", args)
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
        "shopt" => shopt(ex, ctx, args),
        "getopts" => getopts(ex, ctx, args),
        "type" => type_builtin(ex, ctx, "type", args),
        "hash" => hash(ex, ctx, args),
        "alias" | "unalias" => alias(ex, ctx, name, args),
        "dirs" | "pushd" | "popd" => dirs(ex, ctx, name, args),
        "compgen" => compgen(ex, ctx, args),
        "ulimit" => ulimit(ex, ctx, args),
        "trap" => trap(ex, ctx, args),
        "read" => read(ex, ctx, args),
        "wait" => {
            // Waiting for every job succeeds; it is `wait PID` that reports the
            // job's own status. Returning the last child's status made
            // `cmd & wait` fail whenever the job did, so `set -e` scripts and
            // `if ... wait` branches took the other path.
            for mut child in ex.shared.bg.drain(..) {
                let _ = child.wait();
            }
            Ok(0)
        }
        "eval" => {
            let src = args.join(" ");
            if src.trim().is_empty() {
                return Ok(0);
            }
            crate::walk::run_source(ex, ctx, &src, tested)
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
            let r = crate::walk::run_source(ex, ctx, &src, tested);
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
        // Runs the builtin even where a function of the same name shadows it,
        // which is what dispatching here rather than through exec_simple gives.
        "builtin" => match args.split_first() {
            None => Ok(0),
            Some((first, rest)) if is_builtin(first) => run(ex, ctx, first, rest, tested),
            Some((first, _)) => {
                ctx.write_err(&format!("bash-walker: builtin: {first}: not a shell builtin\n"));
                Ok(1)
            }
        },
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
            let digits = match conv {
                'o' => format!("{v:o}"),
                'x' => format!("{v:x}"),
                'X' => format!("{v:X}"),
                _ => v.abs().to_string(),
            };
            // For an integer a precision is the minimum number of digits, so
            // it zero-pads the number itself rather than the field, sits
            // inside the sign, and prints zero as nothing at all.
            let mut s = match prec {
                Some(0) if v == 0 => String::new(),
                Some(p) if digits.len() < p => format!("{}{digits}", "0".repeat(p - digits.len())),
                _ => digits,
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
                    'o' if s.starts_with('0') => s,
                    'o' => format!("0{s}"),
                    'x' => format!("0x{s}"),
                    'X' => format!("0X{s}"),
                    _ => s,
                };
            }
            // A precision cancels the `0` flag: the zeros are already in the
            // number, and the field pads with spaces.
            let field_flags = if prec.is_some() { flags.replace('0', "") } else { flags.to_string() };
            return pad_number(out, &s, &field_flags, width);
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

/// `declare`, and the four builtins bash implements with the same code:
/// `typeset`, `readonly`, `export` and `local`. They differ in the
/// attributes they imply and in whether a name lands in the current scope
/// or the global one.
fn declare(ex: &mut Exec, ctx: &Ctx, verb: &str, args: &[String]) -> Result<i32, Flow> {
    let mut on = Attrs::default();
    let mut off = Attrs::default();
    let mut print = false;
    let mut global = false;
    let mut functions = false;
    let mut names_only = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        let plus = a.starts_with('+');
        if !(plus || a.starts_with('-')) || a.len() < 2 {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'p' => print = true,
                'g' => global = true,
                'f' => functions = true,
                'F' => {
                    functions = true;
                    names_only = true;
                }
                'n' if verb == "export" => off.exported = true,
                'a' | 'A' => {
                    return Err(Flow::Fatal(format!(
                        "{verb} -{c}: arrays are not supported by bash-walker"
                    )))
                }
                'n' => {
                    return Err(Flow::Fatal(format!(
                        "{verb} -n: namerefs are not supported by bash-walker"
                    )))
                }
                other => {
                    let target = if plus { &mut off } else { &mut on };
                    if !target.set(other, true) {
                        ctx.write_err(&format!(
                            "bash-walker: {verb}: -{other}: invalid option\n{verb}: usage: {verb} [-fFgilprtux] [name[=value] ...]\n"
                        ));
                        return Ok(2);
                    }
                }
            }
        }
        i += 1;
    }
    match verb {
        "readonly" => on.readonly = true,
        "export" if !off.exported => on.exported = true,
        _ => {}
    }
    let names = &args[i..];

    if names.is_empty() {
        return list_declarations(ex, ctx, verb, on, print, functions, names_only);
    }

    // `declare` inside a function declares locally unless told otherwise,
    // which is what makes `declare x` in a function shadow the global.
    let scope_is_local = verb == "local" || (ex.shared.func_depth > 0 && !global);
    let mut status = 0;
    for arg in names {
        let (name, value) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        if functions {
            status |= print_function(ex, ctx, verb, name, names_only);
            continue;
        }
        if !is_identifier(name) {
            ctx.write_err(&format!(
                "bash-walker: {verb}: `{arg}': not a valid identifier\n"
            ));
            status = 1;
            continue;
        }
        if print && on.is_empty() && off.is_empty() && value.is_none() {
            match declaration_line(ex, name) {
                Some(line) => ctx.write_out(&line),
                None => {
                    ctx.write_err(&format!("bash-walker: {verb}: {name}: not found\n"));
                    status = 1;
                }
            }
            continue;
        }
        // A new local starts from nothing but the export attribute: a
        // global `-i` must not make the local arithmetic. Readonly is still
        // read from the variable being shadowed, which is what refuses
        // `local x` over a readonly global.
        let effective = ex.state.get_attrs(name);
        let current = if scope_is_local {
            ex.state
                .current_scope_attrs(name)
                .unwrap_or(Attrs { exported: effective.exported, ..Attrs::default() })
        } else {
            effective
        };
        if effective.readonly && (value.is_some() || !off.is_empty()) {
            ctx.write_err(&format!("bash-walker: {verb}: {name}: readonly variable\n"));
            status = 1;
            continue;
        }
        let mut attrs = current;
        for c in on.letters().chars() {
            attrs.set(c, true);
        }
        for c in off.letters().chars() {
            attrs.set(c, false);
        }
        // The value goes in before `-r` takes effect: `readonly x=1` sets
        // the variable it is making readonly, it does not refuse itself.
        let landing = Attrs { readonly: false, ..attrs };
        if scope_is_local {
            ex.state.declare_local_attrs(name, landing);
        } else {
            ex.state.set_attrs(name, landing);
        }
        let mut assigned = Ok(());
        if let Some(v) = value {
            assigned = ex.state.assign(name, v);
        }
        if attrs.readonly {
            if scope_is_local {
                ex.state.declare_local_attrs(name, attrs);
            } else {
                ex.state.set_attrs(name, attrs);
            }
        }
        {
            match assigned {
                Ok(()) => {}
                Err(AssignError::Readonly) => {
                    ctx.write_err(&format!("bash-walker: {verb}: {name}: readonly variable\n"));
                    status = 1;
                }
                Err(AssignError::Arith(msg)) => {
                    ctx.write_err(&format!("bash-walker: {verb}: {msg}\n"));
                    status = 1;
                }
            }
        }
    }
    Ok(status)
}

/// The no-name forms: every variable carrying the attributes asked for, in
/// the order and shape bash prints them.
fn list_declarations(
    ex: &mut Exec,
    ctx: &Ctx,
    verb: &str,
    filter: Attrs,
    print: bool,
    functions: bool,
    names_only: bool,
) -> Result<i32, Flow> {
    if functions {
        if !names_only {
            return Err(Flow::Fatal(
                "declare -f: printing a function definition is not supported by bash-walker".into(),
            ));
        }
        let mut names: Vec<&String> = ex.state.funcs.keys().collect();
        names.sort();
        let mut out = String::new();
        for n in names {
            out.push_str(&format!("{n}\n"));
        }
        ctx.write_out(&out);
        return Ok(0);
    }
    // Bare `declare` prints name=value and the function definitions after
    // it; every other form prints the `declare -X name="value"` shape.
    let bare = !print && filter.is_empty();
    if bare && !ex.state.funcs.is_empty() {
        return Err(Flow::Fatal(format!(
            "{verb}: printing a function definition is not supported by bash-walker"
        )));
    }
    let mut out = String::new();
    for name in ex.state.visible_names() {
        let attrs = ex.state.get_attrs(&name);
        if !attrs_include(attrs, filter) {
            continue;
        }
        let value = ex.state.get_var(&name);
        if bare {
            if let Some(v) = value {
                out.push_str(&format!("{name}={v}\n"));
            }
        } else {
            out.push_str(&format_declaration(&name, attrs, value.as_deref()));
        }
    }
    ctx.write_out(&out);
    Ok(0)
}

fn print_function(ex: &Exec, ctx: &Ctx, verb: &str, name: &str, names_only: bool) -> i32 {
    if !ex.state.funcs.contains_key(name) {
        ctx.write_err(&format!("bash-walker: {verb}: {name}: not found\n"));
        return 1;
    }
    if names_only {
        ctx.write_out(&format!("{name}\n"));
        return 0;
    }
    ctx.write_err(
        "bash-walker: declare -f: printing a function definition is not supported by bash-walker\n",
    );
    1
}

/// One `declare -p` line, or None when nothing declares the name.
fn declaration_line(ex: &Exec, name: &str) -> Option<String> {
    let attrs = ex.state.get_attrs(name);
    let value = ex.state.get_var(name);
    if value.is_none() && attrs.is_empty() && !ex.state.is_declared(name) {
        return None;
    }
    Some(format_declaration(name, attrs, value.as_deref()))
}

fn format_declaration(name: &str, attrs: Attrs, value: Option<&str>) -> String {
    let letters = attrs.letters();
    let flags = if letters.is_empty() { "--".to_string() } else { format!("-{letters}") };
    match value {
        Some(v) => format!("declare {flags} {name}={}\n", quote_value(v)),
        None => format!("declare {flags} {name}\n"),
    }
}

/// bash quotes a value so it reads back as itself: double quotes normally,
/// and `$'...'` as soon as a control character is in there.
fn quote_value(v: &str) -> String {
    if v.chars().any(|c| (c as u32) < 0x20 || c == '\x7f') {
        let mut q = String::from("$'");
        for c in v.chars() {
            match c {
                '\n' => q.push_str("\\n"),
                '\t' => q.push_str("\\t"),
                '\r' => q.push_str("\\r"),
                '\'' => q.push_str("\\'"),
                '\\' => q.push_str("\\\\"),
                c if (c as u32) < 0x20 || c == '\x7f' => {
                    q.push_str(&format!("\\{:03o}", c as u32))
                }
                c => q.push(c),
            }
        }
        q.push('\'');
        return q;
    }
    let mut q = String::from("\"");
    for c in v.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

fn attrs_include(a: Attrs, filter: Attrs) -> bool {
    filter.letters().chars().all(|c| a.letters().contains(c))
}

fn is_identifier(name: &str) -> bool {
    let mut cs = name.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `unset [-fv] name...`. With neither flag bash unsets the variable, and
/// only falls back to the function when no variable of that name exists.
fn unset(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut vars_only = false;
    let mut funcs_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" => vars_only = true,
            "-f" => funcs_only = true,
            "--" => {
                i += 1;
                break;
            }
            a if a.starts_with('-') && a.len() > 1 => {
                ctx.write_err(&format!(
                    "bash-walker: unset: {a}: invalid option\nunset: usage: unset [-f] [-v] [name ...]\n"
                ));
                return Ok(2);
            }
            _ => break,
        }
        i += 1;
    }
    let mut status = 0;
    for name in &args[i..] {
        if funcs_only {
            ex.state.funcs.remove(name);
            continue;
        }
        if !vars_only && !ex.state.is_declared(name) && ex.state.funcs.contains_key(name) {
            ex.state.funcs.remove(name);
            continue;
        }
        if ex.state.unset_var(name).is_err() {
            ctx.write_err(&format!(
                "bash-walker: unset: {name}: cannot unset: readonly variable\n"
            ));
            status = 1;
        }
    }
    Ok(status)
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
                    // A bare `-o`/`+o` lists every option, `-o` as a table
                    // and `+o` as the `set` commands that would restore it.
                    None => {
                        ctx.write_out(&option_listing(ex, on));
                        return Ok(0);
                    }
                    Some(name) => match ex.state.flags.set_option(name, on) {
                        Ok(()) => {}
                        Err(()) if ex.state.flags.option(name).is_none() => {
                            ctx.write_err(&format!(
                                "bash-walker: set: {name}: invalid option name\n"
                            ));
                            return Ok(2);
                        }
                        Err(()) => {
                            let sign = if on { '-' } else { '+' };
                            return Err(Flow::Fatal(format!(
                                "set {sign}o {name}: not supported by bash-walker"
                            )));
                        }
                    },
                }
            }
            flag if flag.starts_with('-') || flag.starts_with('+') => {
                let on = flag.starts_with('-');
                for c in flag[1..].chars() {
                    // -f (noglob), -C, ... are behaviour changes we don't
                    // implement; failing loud beats silently differing.
                    if let Err(other) = ex.state.flags.set_letter(c, on) {
                        return Err(Flow::Fatal(format!(
                            "set -{other}: not supported by bash-walker"
                        )));
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

/// The words bash's parser treats as syntax, which `type` reports as
/// keywords rather than commands.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until",
    "do", "done", "in", "function", "time", "{", "}", "!", "[[", "]]", "coproc",
];

/// What a name resolves to, innermost first — the order `type -a` prints.
enum Resolved {
    Keyword,
    Function,
    Builtin,
    Hashed(String),
    File(String),
}

fn resolve(ex: &Exec, name: &str, all: bool) -> Vec<Resolved> {
    let mut found = Vec::new();
    if KEYWORDS.contains(&name) {
        found.push(Resolved::Keyword);
        if !all {
            return found;
        }
    }
    if ex.state.funcs.contains_key(name) {
        found.push(Resolved::Function);
        if !all {
            return found;
        }
    }
    if is_builtin(name) {
        found.push(Resolved::Builtin);
        if !all {
            return found;
        }
    }
    if let Some((path, _)) = ex.state.hashed.get(name) {
        found.push(Resolved::Hashed(path.clone()));
        if !all {
            return found;
        }
    }
    for path in executables_named(ex, name, all) {
        found.push(Resolved::File(path));
        if !all {
            break;
        }
    }
    found
}

/// Every executable of that name on PATH, in PATH order. A name with a
/// slash in it is a path already and stands or falls on its own.
fn executables_named(ex: &Exec, name: &str, all: bool) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    if name.contains('/') {
        if let Ok(md) = std::fs::metadata(ex.state.resolve(name)) {
            if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                out.push(name.to_string());
            }
        }
        return out;
    }
    let Some(path) = ex.state.get_var("PATH") else {
        return out;
    };
    for dir in path.split(':') {
        let cand = std::path::Path::new(dir).join(name);
        if let Ok(md) = cand.metadata() {
            if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                out.push(cand.to_string_lossy().into_owned());
                if !all {
                    return out;
                }
            }
        }
    }
    out
}

/// `type [-afptP] name ...`, and the same work behind `command -v`/`-V`.
fn type_builtin(ex: &mut Exec, ctx: &Ctx, verb: &str, args: &[String]) -> Result<i32, Flow> {
    let mut all = false;
    let mut kind_only = false;
    let mut path_only = false;
    let mut force_path = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'a' => all = true,
                't' => kind_only = true,
                'p' => path_only = true,
                'P' => {
                    force_path = true;
                    path_only = true;
                }
                'f' => {}
                other => {
                    ctx.write_err(&format!(
                        "bash-walker: {verb}: -{other}: invalid option\n{verb}: usage: type [-afptP] name [name ...]\n"
                    ));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    let mut status = 0;
    for name in &args[i..] {
        let found = if force_path {
            executables_named(ex, name, all).into_iter().map(Resolved::File).collect()
        } else {
            resolve(ex, name, all)
        };
        if found.is_empty() {
            if !path_only && !kind_only {
                ctx.write_err(&format!("bash-walker: {verb}: {name}: not found\n"));
            }
            status = 1;
            continue;
        }
        for what in &found {
            match (kind_only, path_only, what) {
                (true, _, Resolved::Keyword) => ctx.write_out("keyword\n"),
                (true, _, Resolved::Function) => ctx.write_out("function\n"),
                (true, _, Resolved::Builtin) => ctx.write_out("builtin\n"),
                (true, _, Resolved::Hashed(_) | Resolved::File(_)) => ctx.write_out("file\n"),
                (_, true, Resolved::Hashed(p) | Resolved::File(p)) => {
                    ctx.write_out(&format!("{p}\n"))
                }
                (_, true, _) => {}
                (_, _, Resolved::Keyword) => {
                    ctx.write_out(&format!("{name} is a shell keyword\n"))
                }
                (_, _, Resolved::Builtin) => {
                    ctx.write_out(&format!("{name} is a shell builtin\n"))
                }
                (_, _, Resolved::Hashed(p)) => {
                    ctx.write_out(&format!("{name} is hashed ({p})\n"))
                }
                (_, _, Resolved::File(p)) => ctx.write_out(&format!("{name} is {p}\n")),
                (_, _, Resolved::Function) => {
                    ctx.write_out(&format!("{name} is a function\n"));
                    return Err(Flow::Fatal(format!(
                        "{verb} {name}: printing a function definition is not supported by bash-walker"
                    )));
                }
            }
        }
    }
    Ok(status)
}

/// The conditions the walker can actually raise. A real signal is not one
/// of them: no handler is installed here, so a trap on `INT` would be a
/// promise the walker could not keep.
const TRAP_CONDITIONS: &[&str] = &["EXIT", "ERR"];

/// The canonical name for a trap condition, or None when the walker cannot
/// raise it. `0` is `EXIT`, and a `SIG` prefix is optional.
fn trap_condition(spec: &str) -> Option<String> {
    let upper = spec.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    if bare == "0" {
        return Some("EXIT".to_string());
    }
    TRAP_CONDITIONS.contains(&bare).then(|| bare.to_string())
}

/// `trap [-Plp] [[action] condition ...]`.
fn trap(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    const USAGE: &str = "trap: usage: trap [-Plp] [[action] signal_spec ...]\n";
    let mut print = false;
    let mut action_only = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'p' => print = true,
                'P' => action_only = true,
                'l' => {
                    return Err(Flow::Fatal(
                        "trap -l: the signal list is not supported by bash-walker".into(),
                    ))
                }
                other => {
                    ctx.write_err(&format!(
                        "bash-walker: trap: -{other}: invalid option\n{USAGE}"
                    ));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    if print && action_only {
        ctx.write_err("bash-walker: trap: cannot specify both -p and -P\n");
        return Ok(2);
    }
    let rest = &args[i..];

    // Listing: everything, or just the conditions named.
    if rest.is_empty() || print || action_only {
        let mut out = String::new();
        let named: Vec<String> = rest.to_vec();
        for cond in TRAP_CONDITIONS {
            let Some(action) = ex.state.traps.get(*cond) else {
                continue;
            };
            if !named.is_empty()
                && !named.iter().any(|n| trap_condition(n).as_deref() == Some(cond))
            {
                continue;
            }
            if action_only {
                out.push_str(&format!("{action}\n"));
            } else {
                out.push_str(&format!("trap -- '{action}' {cond}\n"));
            }
        }
        ctx.write_out(&out);
        return Ok(0);
    }

    // `trap CONDITION` on its own resets that condition, which is why the
    // first word is only an action when more words follow it.
    let (action, conditions): (Option<&str>, &[String]) = if rest.len() == 1 {
        match trap_condition(&rest[0]) {
            Some(_) => (None, rest),
            None => {
                ctx.write_err(USAGE);
                return Ok(2);
            }
        }
    } else {
        (Some(rest[0].as_str()), &rest[1..])
    };

    let mut status = 0;
    for spec in conditions {
        let Some(cond) = trap_condition(spec) else {
            if signal_name(spec) {
                return Err(Flow::Fatal(format!(
                    "trap on {spec}: signals are not supported by bash-walker, which installs no handler to deliver one"
                )));
            }
            ctx.write_err(&format!(
                "bash-walker: trap: {spec}: invalid signal specification\n"
            ));
            status = 1;
            continue;
        };
        match action {
            // `trap - COND` and a bare `trap COND` restore the default.
            None | Some("-") => {
                ex.state.traps.remove(&cond);
            }
            Some(a) => {
                ex.state.traps.insert(cond, a.to_string());
            }
        }
    }
    Ok(status)
}

/// Whether a spec names a real signal (or `DEBUG`/`RETURN`), as opposed to
/// being a typo. Both are refused, but for different reasons and with
/// different words.
fn signal_name(spec: &str) -> bool {
    let upper = spec.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    if let Ok(n) = bare.parse::<u32>() {
        return (1..=64).contains(&n);
    }
    matches!(
        bare,
        "HUP" | "INT" | "QUIT" | "ILL" | "TRAP" | "ABRT" | "EMT" | "FPE" | "KILL" | "BUS"
            | "SEGV" | "SYS" | "PIPE" | "ALRM" | "TERM" | "URG" | "STOP" | "TSTP" | "CONT"
            | "CHLD" | "CLD" | "TTIN" | "TTOU" | "IO" | "XCPU" | "XFSZ" | "VTALRM" | "PROF"
            | "WINCH" | "INFO" | "USR1" | "USR2" | "POLL" | "PWR" | "STKFLT" | "UNUSED"
            | "DEBUG" | "RETURN"
    )
}

/// `ulimit` — the reading side. Each letter names a resource and the
/// divisor bash reports it in.
const LIMITS: &[(char, libc::c_int, u64)] = &[
    ('c', libc::RLIMIT_CORE as libc::c_int, 512),
    ('d', libc::RLIMIT_DATA as libc::c_int, 1024),
    ('f', libc::RLIMIT_FSIZE as libc::c_int, 512),
    ('l', libc::RLIMIT_MEMLOCK as libc::c_int, 1024),
    ('m', libc::RLIMIT_RSS as libc::c_int, 1024),
    ('n', libc::RLIMIT_NOFILE as libc::c_int, 1),
    ('s', libc::RLIMIT_STACK as libc::c_int, 1024),
    ('t', libc::RLIMIT_CPU as libc::c_int, 1),
    ('u', libc::RLIMIT_NPROC as libc::c_int, 1),
    ('v', libc::RLIMIT_AS as libc::c_int, 1024),
];

/// `ulimit [-HS] [-cdflmnstuv]` — reports a limit. Changing one is refused:
/// the walker runs every stage of a script in one process, so a `setrlimit`
/// here would reach further than the subshell that asked for it and could
/// starve the walker itself of the descriptors it is holding.
fn ulimit(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let _ = ex;
    let mut hard = false;
    let mut which: Option<char> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'H' => hard = true,
                'S' => hard = false,
                'a' => {
                    return Err(Flow::Fatal(
                        "ulimit -a: the listing is not supported by bash-walker".into(),
                    ))
                }
                c if LIMITS.iter().any(|(l, _, _)| *l == c) => which = Some(c),
                other => {
                    ctx.write_err(&format!(
                        "bash-walker: ulimit: -{other}: invalid option\nulimit: usage: ulimit [-SHabcdefiklmnpqrstuvxPRT] [limit]\n"
                    ));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    if args.len() > i {
        return Err(Flow::Fatal(
            "ulimit: setting a limit is not supported by bash-walker".into(),
        ));
    }
    // No resource letter means the file-size limit, bash's default.
    let letter = which.unwrap_or('f');
    let (_, resource, divisor) = LIMITS.iter().find(|(l, _, _)| *l == letter).unwrap();
    // SAFETY: getrlimit only reads, into a value we own.
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(*resource as _, &mut rl) } != 0 {
        ctx.write_err("bash-walker: ulimit: cannot read limit\n");
        return Ok(1);
    }
    let value = if hard { rl.rlim_max } else { rl.rlim_cur };
    if value == libc::RLIM_INFINITY {
        ctx.write_out("unlimited\n");
    } else {
        ctx.write_out(&format!("{}\n", value as u64 / divisor));
    }
    Ok(0)
}

/// `compgen [-abekv] [-A action] [-W wordlist] [word]` — the words a
/// completion would offer, which is a question about shell state and so is
/// answerable here even though completion itself never runs.
fn compgen(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut actions: Vec<String> = Vec::new();
    let mut wordlist: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        let mut chars = a[1..].chars();
        while let Some(c) = chars.next() {
            match c {
                'a' => actions.push("alias".into()),
                'b' => actions.push("builtin".into()),
                'e' => actions.push("export".into()),
                'k' => actions.push("keyword".into()),
                'v' => actions.push("variable".into()),
                'A' | 'W' => {
                    let inline: String = chars.by_ref().collect();
                    let value = if inline.is_empty() {
                        i += 1;
                        match args.get(i) {
                            Some(v) => v.clone(),
                            None => {
                                ctx.write_err(&format!(
                                    "bash-walker: compgen: -{c}: option requires an argument\n"
                                ));
                                return Ok(2);
                            }
                        }
                    } else {
                        inline
                    };
                    if c == 'A' {
                        actions.push(value);
                    } else {
                        wordlist = Some(value);
                    }
                }
                other => {
                    return Err(Flow::Fatal(format!(
                        "compgen -{other}: not supported by bash-walker"
                    )))
                }
            }
        }
        i += 1;
    }
    let prefix = args.get(i).cloned().unwrap_or_default();

    let mut words: Vec<String> = Vec::new();
    if let Some(list) = wordlist {
        words.extend(list.split_whitespace().map(str::to_string));
    }
    for action in &actions {
        match action.as_str() {
            "function" => {
                let mut names: Vec<String> = ex.state.funcs.keys().cloned().collect();
                names.sort();
                words.extend(names);
            }
            "builtin" => {
                let mut names: Vec<String> =
                    NATIVE.iter().chain(UNSUPPORTED.iter()).map(|s| (*s).to_string()).collect();
                names.sort();
                words.extend(names);
            }
            "keyword" => words.extend(KEYWORDS.iter().map(|s| (*s).to_string())),
            "shopt" => {
                words.extend(crate::state::SHOPT_DEFAULTS.iter().map(|(n, _)| (*n).to_string()))
            }
            "setopt" => words.extend(crate::state::SET_OPTIONS.iter().map(|s| (*s).to_string())),
            "variable" => words.extend(ex.state.visible_names()),
            "export" => words.extend(
                ex.state.visible_names().into_iter().filter(|n| ex.state.get_attrs(n).exported),
            ),
            // The alias table is always empty; see `alias`.
            "alias" => {}
            other => {
                return Err(Flow::Fatal(format!(
                    "compgen -A {other}: not supported by bash-walker"
                )))
            }
        }
    }
    let matched: Vec<String> = words.into_iter().filter(|w| w.starts_with(&prefix)).collect();
    if matched.is_empty() {
        return Ok(1);
    }
    let mut out = String::new();
    for w in matched {
        out.push_str(&w);
        out.push('\n');
    }
    ctx.write_out(&out);
    Ok(0)
}

/// `hash [-lr] [-p path] [-dt] [name ...]`. Nothing fills this table by
/// running a command — the walker looks a program up through the operating
/// system every time — so what is in here is what `hash -p` put there.
fn hash(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut path: Option<String> = None;
    let mut list = false;
    let mut delete = false;
    let mut show_path = false;
    let mut cleared = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'r' => {
                    ex.state.hashed.clear();
                    cleared = true;
                }
                'l' => list = true,
                'd' => delete = true,
                't' => show_path = true,
                'p' => {
                    i += 1;
                    match args.get(i) {
                        Some(p) => path = Some(p.clone()),
                        None => {
                            ctx.write_err("bash-walker: hash: -p: option requires an argument\n");
                            return Ok(2);
                        }
                    }
                }
                other => {
                    ctx.write_err(&format!(
                        "bash-walker: hash: -{other}: invalid option\nhash: usage: hash [-lr] [-p pathname] [-dt] [name ...]\n"
                    ));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    let names = &args[i..];
    if names.is_empty() {
        // `hash -r` empties the table and says nothing.
        if cleared {
            return Ok(0);
        }
        if ex.state.hashed.is_empty() {
            if !list {
                ctx.write_out("hash: hash table empty\n");
            }
            return Ok(0);
        }
        let mut out = String::new();
        if !list {
            out.push_str("hits\tcommand\n");
        }
        for (name, (p, hits)) in &ex.state.hashed {
            if list {
                out.push_str(&format!("builtin hash -p {p} {name}\n"));
            } else {
                out.push_str(&format!("{hits:4}\t{p}\n"));
            }
        }
        ctx.write_out(&out);
        return Ok(0);
    }
    let mut status = 0;
    for name in names {
        if let Some(p) = &path {
            ex.state.hashed.insert(name.clone(), (p.clone(), 0));
            continue;
        }
        if delete {
            if ex.state.hashed.remove(name).is_none() {
                ctx.write_err(&format!("bash-walker: hash: {name}: not found\n"));
                status = 1;
            }
            continue;
        }
        if show_path {
            match ex.state.hashed.get(name) {
                Some((p, _)) => {
                    if names.len() > 1 {
                        ctx.write_out(&format!("{name}\t{p}\n"));
                    } else {
                        ctx.write_out(&format!("{p}\n"));
                    }
                }
                None => {
                    ctx.write_err(&format!("bash-walker: hash: {name}: not found\n"));
                    status = 1;
                }
            }
            continue;
        }
        match executables_named(ex, name, false).into_iter().next() {
            Some(p) => {
                ex.state.hashed.insert(name.clone(), (p, 0));
            }
            None => {
                ctx.write_err(&format!("bash-walker: hash: {name}: not found\n"));
                status = 1;
            }
        }
    }
    Ok(status)
}

/// `alias`/`unalias`. The alias table is always empty here: defining one is
/// refused by name, because nothing in the walker would ever expand it. The
/// query and listing forms are exactly right against that empty table.
fn alias(ex: &mut Exec, ctx: &Ctx, verb: &str, args: &[String]) -> Result<i32, Flow> {
    let _ = ex;
    let mut i = 0;
    let mut all = false;
    while i < args.len() {
        match args[i].as_str() {
            "-p" if verb == "alias" => {}
            "-a" if verb == "unalias" => all = true,
            "--" => {
                i += 1;
                break;
            }
            a if a.starts_with('-') && a.len() > 1 => {
                ctx.write_err(&format!(
                    "bash-walker: {verb}: {a}: invalid option\n{verb}: usage: {verb} [-p] [name[=value] ... ]\n"
                ));
                return Ok(2);
            }
            _ => break,
        }
        i += 1;
    }
    if all {
        return Ok(0);
    }
    let mut status = 0;
    for a in &args[i..] {
        if verb == "alias" && a.contains('=') {
            return Err(Flow::Fatal(
                "alias: defining an alias is not supported by bash-walker".into(),
            ));
        }
        ctx.write_err(&format!("bash-walker: {verb}: {a}: not found\n"));
        status = 1;
    }
    Ok(status)
}

/// `dirs`, `pushd` and `popd` over one stack whose top is always the
/// current directory.
fn dirs(ex: &mut Exec, ctx: &Ctx, verb: &str, args: &[String]) -> Result<i32, Flow> {
    let mut verbose = false;
    let mut long = false;
    let mut clear = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        // `+N`/`-N` are stack positions, not options.
        if !a.starts_with('-') || a.len() < 2 || a[1..].chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'v' => verbose = true,
                'l' => long = true,
                'c' => clear = true,
                'p' => {}
                other => {
                    ctx.write_err(&format!(
                        "bash-walker: {verb}: -{other}: invalid option\n{verb}: usage: {verb} [-clpv] [+N] [-N]\n"
                    ));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    let rest = &args[i..];
    match verb {
        "dirs" => {
            if clear {
                ex.state.dirstack.clear();
                return Ok(0);
            }
            ctx.write_out(&dirstack_listing(ex, verbose, long));
            Ok(0)
        }
        "pushd" => {
            let Some(target) = rest.first() else {
                // No argument swaps the top two entries.
                if ex.state.dirstack.is_empty() {
                    ctx.write_err("bash-walker: pushd: no other directory\n");
                    return Ok(1);
                }
                let next = ex.state.dirstack.remove(0);
                let here = ex.state.cwd.clone();
                ex.state.dirstack.insert(0, here);
                let st = cd(ex, ctx, &[next.to_string_lossy().into_owned()])?;
                if st != 0 {
                    return Ok(st);
                }
                ctx.write_out(&dirstack_listing(ex, false, false));
                return Ok(0);
            };
            let here = ex.state.cwd.clone();
            let st = cd(ex, ctx, &[target.clone()])?;
            if st != 0 {
                return Ok(st);
            }
            ex.state.dirstack.insert(0, here);
            ctx.write_out(&dirstack_listing(ex, false, false));
            Ok(0)
        }
        _ => {
            if ex.state.dirstack.is_empty() {
                ctx.write_err("bash-walker: popd: directory stack empty\n");
                return Ok(1);
            }
            let next = ex.state.dirstack.remove(0);
            let st = cd(ex, ctx, &[next.to_string_lossy().into_owned()])?;
            if st != 0 {
                return Ok(st);
            }
            ctx.write_out(&dirstack_listing(ex, false, false));
            Ok(0)
        }
    }
}

fn dirstack_listing(ex: &Exec, verbose: bool, long: bool) -> String {
    let home = ex.state.get_var("HOME").unwrap_or_default();
    let shorten = |p: &str| -> String {
        if long || home.is_empty() {
            return p.to_string();
        }
        if p == home {
            "~".to_string()
        } else if let Some(rest) = p.strip_prefix(&format!("{home}/")) {
            format!("~/{rest}")
        } else {
            p.to_string()
        }
    };
    let mut all = vec![ex.state.cwd.to_string_lossy().into_owned()];
    all.extend(ex.state.dirstack.iter().map(|p| p.to_string_lossy().into_owned()));
    if verbose {
        let mut out = String::new();
        for (i, p) in all.iter().enumerate() {
            out.push_str(&format!("{i:2}  {}\n", shorten(p)));
        }
        return out;
    }
    format!("{}\n", all.iter().map(|p| shorten(p)).collect::<Vec<_>>().join(" "))
}

/// `getopts optstring name [arg ...]` — one option per call, with the
/// scan's position carried in `OPTIND` and in the walker's own record of
/// how far into the current word it has read.
fn getopts(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    const USAGE: &str = "getopts: usage: getopts optstring name [arg ...]\n";
    if let Some(a) = args.first() {
        if a.starts_with('-') && a.len() > 1 {
            ctx.write_err(&format!("bash-walker: getopts: {a}: invalid option\n{USAGE}"));
            return Ok(2);
        }
    }
    let (Some(optstring), Some(name)) = (args.first(), args.get(1)) else {
        ctx.write_err(USAGE);
        return Ok(2);
    };
    let words: Vec<String> = if args.len() > 2 {
        args[2..].to_vec()
    } else {
        ex.state.positional.clone()
    };

    let silent = optstring.starts_with(':');
    // OPTERR=0 silences the messages without changing what getopts reports
    // back through the variable, which is what a leading colon does.
    let quiet = silent || ex.state.get_var("OPTERR").as_deref() == Some("0");
    let spec: Vec<char> = optstring.chars().collect();

    let stated: i64 = ex.state.get_var("OPTIND").and_then(|v| v.trim().parse().ok()).unwrap_or(1);
    let mut charpos = ex.state.getopts_charpos();
    let mut optind = if stated < 1 { 1usize } else { stated as usize };

    // What to report back, once the scan has moved: the value for the
    // caller's variable, what OPTARG becomes, and getopts' own status.
    // Every path ends at the same place, because bash advances the scan
    // before it discovers it cannot store the answer.
    let mut optarg: Option<String> = None;
    let mut status = 0;
    let mut value = "?".to_string();

    let word = loop {
        match words.get(optind - 1) {
            None => {
                charpos = 0;
                status = 1;
                break None;
            }
            Some(word) if charpos > 0 => break Some(word.clone()),
            Some(word) if word == "--" => {
                optind += 1;
                charpos = 0;
                status = 1;
                break None;
            }
            Some(word) if !word.starts_with('-') || word == "-" => {
                charpos = 0;
                status = 1;
                break None;
            }
            Some(word) => {
                charpos = 1;
                break Some(word.clone());
            }
        }
    };

    if let Some(word) = word {
        let chars: Vec<char> = word.chars().collect();
        let c = chars[charpos];
        charpos += 1;
        // The word is used up: the next call starts at the next argument.
        if charpos >= chars.len() {
            optind += 1;
            charpos = 0;
        }
        let known = c != ':' && spec.contains(&c);
        let takes_arg = known
            && spec.iter().position(|&s| s == c).is_some_and(|i| spec.get(i + 1) == Some(&':'));
        if !known {
            if !quiet {
                ctx.write_err(&format!("{}: illegal option -- {c}\n", ex.state.script_name));
            }
            if silent {
                optarg = Some(c.to_string());
            }
        } else if !takes_arg {
            value = c.to_string();
        } else {
            // `-b bval` and `-bbval` are the same option with the same
            // argument.
            let arg = if charpos > 0 {
                let rest: String = chars[charpos..].iter().collect();
                optind += 1;
                charpos = 0;
                Some(rest)
            } else {
                words.get(optind - 1).cloned().inspect(|_| optind += 1)
            };
            match arg {
                Some(a) => {
                    value = c.to_string();
                    optarg = Some(a);
                }
                None => {
                    if !quiet {
                        ctx.write_err(&format!(
                            "{}: option requires an argument -- {c}\n",
                            ex.state.script_name
                        ));
                    }
                    if silent {
                        value = ":".to_string();
                        optarg = Some(c.to_string());
                    }
                }
            }
        }
    }

    // Writing OPTIND is itself what rewinds the scan, so the position goes
    // back afterwards, never before.
    ex.state.set_var("OPTIND", optind.to_string());
    ex.state.set_getopts_charpos(charpos);

    if !is_identifier(name) {
        ctx.write_err(&format!("bash-walker: getopts: `{name}': not a valid identifier\n"));
        return Ok(1);
    }
    if ex.state.assign(name, value).is_err() {
        ctx.write_err(&format!("bash-walker: {name}: readonly variable\n"));
        return Ok(2);
    }
    match optarg {
        Some(a) => {
            if ex.state.assign("OPTARG", a).is_err() {
                ctx.write_err("bash-walker: OPTARG: readonly variable\n");
                return Ok(2);
            }
        }
        // Clearing OPTARG is silent even when it cannot happen: bash
        // reports a readonly OPTARG when it has something to store, and
        // says nothing when it has not.
        None => {
            let _ = ex.state.unset_var("OPTARG");
        }
    }
    Ok(status)
}

/// `set -o` prints a table of every option; `set +o` prints the commands
/// that would put a fresh shell back into this state.
fn option_listing(ex: &Exec, table: bool) -> String {
    let mut out = String::new();
    for name in crate::state::SET_OPTIONS {
        let on = ex.state.flags.option(name).unwrap_or(false);
        if table {
            out.push_str(&format!("{name:<15}\t{}\n", if on { "on" } else { "off" }));
        } else {
            out.push_str(&format!("set {}o {name}\n", if on { '-' } else { '+' }));
        }
    }
    out
}

/// `shopt [-pqsu] [-o] [optname ...]`. The options the walker cannot
/// actually switch are refused by name when a script asks for the value
/// the walker does not have; asking for the one it does have is a no-op,
/// and the inert ones (history, completion, prompts) are simply recorded.
fn shopt(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    let mut print = false;
    let mut quiet = false;
    let mut set_them = false;
    let mut unset_them = false;
    let mut set_options = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'p' => print = true,
                'q' => quiet = true,
                's' => set_them = true,
                'u' => unset_them = true,
                'o' => set_options = true,
                other => {
                    ctx.write_err(&format!(
                        "bash-walker: shopt: -{other}: invalid option\nshopt: usage: shopt [-pqsu] [-o] [optname ...]\n"
                    ));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    let names = &args[i..];
    let changing = set_them != unset_them;

    if names.is_empty() {
        if !quiet {
            ctx.write_out(&shopt_listing(ex, set_options, print, set_them, unset_them));
        }
        return Ok(0);
    }

    let mut status = 0;
    for name in names {
        let value = if set_options {
            ex.state.flags.option(name)
        } else {
            ex.state.shopt(name)
        };
        let Some(current) = value else {
            let what = if set_options { "invalid option name" } else { "invalid shell option name" };
            ctx.write_err(&format!("bash-walker: shopt: {name}: {what}\n"));
            status = 1;
            continue;
        };
        if !changing {
            if !quiet {
                ctx.write_out(&shopt_line(name, current, set_options, print));
            }
            if !current {
                status = 1;
            }
            continue;
        }
        let want = set_them;
        if current == want {
            continue;
        }
        if set_options {
            if ex.state.flags.set_option(name, want).is_err() {
                let sign = if want { '-' } else { '+' };
                return Err(Flow::Fatal(format!(
                    "shopt {}o {name} (set {sign}o {name}): not supported by bash-walker",
                    if want { "-s -" } else { "-u -" }
                )));
            }
            continue;
        }
        if !crate::state::INERT_SHOPTS.contains(&name.as_str()) {
            let flag = if want { "-s" } else { "-u" };
            return Err(Flow::Fatal(format!(
                "shopt {flag} {name}: not supported by bash-walker"
            )));
        }
        ex.state.set_shopt(name, want);
    }
    Ok(status)
}

fn shopt_line(name: &str, on: bool, set_options: bool, print: bool) -> String {
    match (set_options, print) {
        (false, false) => format!("{name:<20}\t{}\n", if on { "on" } else { "off" }),
        (false, true) => format!("shopt -{} {name}\n", if on { 's' } else { 'u' }),
        (true, false) => format!("{name:<15}\t{}\n", if on { "on" } else { "off" }),
        (true, true) => format!("set {}o {name}\n", if on { '-' } else { '+' }),
    }
}

fn shopt_listing(
    ex: &Exec,
    set_options: bool,
    print: bool,
    only_set: bool,
    only_unset: bool,
) -> String {
    let names: Vec<(String, bool)> = if set_options {
        crate::state::SET_OPTIONS
            .iter()
            .map(|n| ((*n).to_string(), ex.state.flags.option(n).unwrap_or(false)))
            .collect()
    } else {
        crate::state::SHOPT_DEFAULTS
            .iter()
            .map(|(n, _)| ((*n).to_string(), ex.state.shopt(n).unwrap_or(false)))
            .collect()
    };
    let mut out = String::new();
    for (name, on) in names {
        if (only_set && !on) || (only_unset && on) {
            continue;
        }
        out.push_str(&shopt_line(&name, on, set_options, print));
    }
    out
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
    let mut saw_newline = false;
    let mut buf = [0u8; 1];
    let mut f = &**stdin;
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {
                got_any = true;
                if buf[0] == b'\n' {
                    saw_newline = true;
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
    // A final line with no newline is assigned and then reported as failure,
    // which is what ends a `while read` loop. Returning 0 ran one iteration
    // bash never runs.
    let unterminated = !saw_newline;
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
        return Ok(i32::from(unterminated));
    }
    // Only runs of IFS WHITESPACE collapse. A non-whitespace delimiter
    // delimits each time it appears, so `x::z` under `IFS=:` is three fields
    // with an empty middle, not two. Dropping empties shifted every variable
    // after the gap.
    let seps: Vec<char> = ifs.chars().collect();
    let ws: Vec<char> = seps.iter().copied().filter(|c| c.is_whitespace()).collect();
    let trimmed = line.trim_matches(|c: char| ws.contains(&c));
    let mut fields: Vec<&str> = Vec::new();
    let mut start = 0;
    let bytes: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut i = 0;
    while i < bytes.len() {
        let (idx, c) = bytes[i];
        if !seps.contains(&c) {
            i += 1;
            continue;
        }
        fields.push(&trimmed[start..idx]);
        // A non-whitespace delimiter may be followed by whitespace, and that
        // run belongs to the same separator rather than making a field.
        i += 1;
        while i < bytes.len() && ws.contains(&bytes[i].1) {
            i += 1;
        }
        // Consecutive whitespace separators collapse into one.
        if c.is_whitespace() {
            while i < bytes.len() && seps.contains(&bytes[i].1) && bytes[i].1.is_whitespace() {
                i += 1;
            }
        }
        start = bytes.get(i).map(|(b, _)| *b).unwrap_or(trimmed.len());
    }
    fields.push(&trimmed[start..]);

    // The last variable takes the whole remainder, delimiters and all, rather
    // than the fields rejoined with a space.
    let last_join;
    if fields.len() > vars.len() {
        let keep = vars.len() - 1;
        let rest_starts = trimmed.len() - fields[keep..].join("").len() - (fields.len() - keep - 1);
        last_join = trimmed[rest_starts..].to_string();
        fields.truncate(keep);
        fields.push(&last_join);
    }
    for (i, v) in vars.iter().enumerate() {
        ex.state.set_var(v, fields.get(i).copied().unwrap_or("").to_string());
    }
    Ok(i32::from(unterminated))
}

fn command(ex: &mut Exec, ctx: &Ctx, args: &[String]) -> Result<i32, Flow> {
    match args.first().map(String::as_str) {
        Some("-v") => {
            let Some(name) = args.get(1) else {
                return Ok(1);
            };
            if crate::builtins::KEYWORDS.contains(&name.as_str())
                || ex.state.funcs.contains_key(name)
                || is_builtin(name)
            {
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
        // `command -V` is `type` under another name.
        Some("-V") => type_builtin(ex, ctx, "command", &args[1..]),
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
