//! `[[ ]]` evaluation — the one construct that is walker-native on both
//! axes (docs/ast-execution.md): its operators are `test`'s vocabulary, but
//! `==`/`!=` glob-match instead of comparing, `=~` writes BASH_REMATCH back
//! into the calling shell, and operands are expanded without word splitting
//! — none of which a subprocess could do on the walker's behalf.

use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;

use bash_parser::{CondExpr, Word};

use crate::arith;
use crate::expand;
use crate::walk::{Ctx, Exec, Flow};

pub fn eval(ex: &mut Exec, ctx: &Ctx, expr: &CondExpr) -> Result<bool, Flow> {
    match expr {
        CondExpr::Or(l, r) => Ok(eval(ex, ctx, l)? || eval(ex, ctx, r)?),
        CondExpr::And(l, r) => Ok(eval(ex, ctx, l)? && eval(ex, ctx, r)?),
        CondExpr::Not(inner) => Ok(!eval(ex, ctx, inner)?),
        CondExpr::Group(inner) => eval(ex, ctx, inner),
        CondExpr::Term(w) => {
            let v = expand::expand_single(ex, ctx, w)?;
            Ok(!v.is_empty())
        }
        CondExpr::Unary { op, operand } => eval_unary(ex, ctx, op, operand),
        CondExpr::Binary { op, left, right } => eval_binary(ex, ctx, op, left, right),
    }
}

fn eval_unary(ex: &mut Exec, ctx: &Ctx, op: &str, operand: &Word) -> Result<bool, Flow> {
    let v = expand::expand_single(ex, ctx, operand)?;
    let path = ex.state.resolve(&v);
    let meta = || std::fs::metadata(&path);
    let lmeta = || std::fs::symlink_metadata(&path);
    Ok(match op {
        "-z" => v.is_empty(),
        "-n" => !v.is_empty(),
        "-v" => ex.state.get_var(&v).is_some(),
        "-e" | "-a" => meta().is_ok(),
        "-f" => meta().map(|m| m.is_file()).unwrap_or(false),
        "-d" => meta().map(|m| m.is_dir()).unwrap_or(false),
        "-s" => meta().map(|m| m.len() > 0).unwrap_or(false),
        "-L" | "-h" => lmeta().map(|m| m.file_type().is_symlink()).unwrap_or(false),
        "-p" => meta().map(|m| m.file_type().is_fifo()).unwrap_or(false),
        "-S" => meta().map(|m| m.file_type().is_socket()).unwrap_or(false),
        "-b" => meta().map(|m| m.file_type().is_block_device()).unwrap_or(false),
        "-c" => meta().map(|m| m.file_type().is_char_device()).unwrap_or(false),
        "-k" => meta().map(|m| m.permissions().mode() & 0o1000 != 0).unwrap_or(false),
        "-g" => meta().map(|m| m.permissions().mode() & 0o2000 != 0).unwrap_or(false),
        "-u" => meta().map(|m| m.permissions().mode() & 0o4000 != 0).unwrap_or(false),
        // access(2) honors effective ids and ACLs — the same call bash makes.
        "-r" => unsafe { access(&path.to_string_lossy(), libc::R_OK) },
        "-w" => unsafe { access(&path.to_string_lossy(), libc::W_OK) },
        "-x" => unsafe { access(&path.to_string_lossy(), libc::X_OK) },
        "-O" => meta().map(|m| m.uid() == unsafe { libc::geteuid() }).unwrap_or(false),
        "-G" => meta().map(|m| m.gid() == unsafe { libc::getegid() }).unwrap_or(false),
        "-t" => v
            .parse::<i32>()
            .map(|fd| unsafe { libc::isatty(fd) == 1 })
            .unwrap_or(false),
        "-N" => false, // "modified since last read" — atime tracking, not kept
        other => {
            return Err(Flow::Fatal(format!(
                "[[ {other} ]]: unary operator not supported by bash-walker"
            )))
        }
    })
}

unsafe fn access(path: &str, mode: i32) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

fn eval_binary(
    ex: &mut Exec,
    ctx: &Ctx,
    op: &str,
    left: &Word,
    right: &Word,
) -> Result<bool, Flow> {
    let l = expand::expand_single(ex, ctx, left)?;
    Ok(match op {
        "==" | "=" | "!=" => {
            // The right side is a PATTERN unless quoted — quoted fragments
            // are escaped so `"$x"*` is literal-text-then-star.
            let parts = expand::expand_parts(ex, ctx, right)?;
            let pat = expand::glob_pattern_from_parts(&parts);
            let pattern = glob::Pattern::new(&pat)
                .map_err(|e| Flow::Fatal(format!("[[ ]]: bad pattern {pat:?}: {e}")))?;
            let matched = pattern.matches(&l);
            if op == "!=" { !matched } else { matched }
        }
        "=~" => {
            // Quoted fragments of the regex are literal (bash's rule);
            // matches land in BASH_REMATCH.
            let parts = expand::expand_parts(ex, ctx, right)?;
            let pat = expand::regex_from_parts(&parts);
            let re = regex::Regex::new(&pat)
                .map_err(|e| Flow::Fatal(format!("[[ =~ ]]: bad regex {pat:?}: {e}")))?;
            match re.captures(&l) {
                Some(caps) => {
                    ex.state.rematch = caps
                        .iter()
                        .map(|c| c.map(|m| m.as_str().to_string()).unwrap_or_default())
                        .collect();
                    true
                }
                None => {
                    ex.state.rematch.clear();
                    false
                }
            }
        }
        "<" => l < expand::expand_single(ex, ctx, right)?,
        ">" => l > expand::expand_single(ex, ctx, right)?,
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            let r = expand::expand_single(ex, ctx, right)?;
            let ln = arith::eval(&l, ex.state).map_err(|e| Flow::Fatal(e.to_string()))?;
            let rn = arith::eval(&r, ex.state).map_err(|e| Flow::Fatal(e.to_string()))?;
            match op {
                "-eq" => ln == rn,
                "-ne" => ln != rn,
                "-lt" => ln < rn,
                "-le" => ln <= rn,
                "-gt" => ln > rn,
                _ => ln >= rn,
            }
        }
        "-nt" | "-ot" | "-ef" => {
            let r = expand::expand_single(ex, ctx, right)?;
            let lm = std::fs::metadata(ex.state.resolve(&l));
            let rm = std::fs::metadata(ex.state.resolve(&r));
            match (op, lm, rm) {
                ("-nt", Ok(a), Ok(b)) => {
                    a.modified().ok() > b.modified().ok()
                }
                ("-nt", Ok(_), Err(_)) => true,
                ("-ot", Ok(a), Ok(b)) => a.modified().ok() < b.modified().ok(),
                ("-ot", Err(_), Ok(_)) => true,
                ("-ef", Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                _ => false,
            }
        }
        other => {
            return Err(Flow::Fatal(format!(
                "[[ {other} ]]: binary operator not supported by bash-walker"
            )))
        }
    })
}
