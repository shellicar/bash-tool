//! Human-readable AST printer — an indented tree, not `{:#?}`'s Rust-struct
//! dump, so a real command's parsed structure can actually be read at a
//! glance rather than decoded from `Debug` output.

use crate::ast::*;
use std::fmt::Write;

pub fn pretty(cmd: &Command) -> String {
    let mut out = String::new();
    write_command(&mut out, cmd, 0);
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_word(out: &mut String, w: &Word) {
    let _ = write!(out, "{:?}", w.text);
}

fn write_redirect(out: &mut String, r: &Redirect, depth: usize) {
    indent(out, depth);
    let sym = match r.op {
        RedirectOp::Out => ">",
        RedirectOp::Append => ">>",
        RedirectOp::In => "<",
        RedirectOp::DupOut => ">&",
        RedirectOp::DupIn => "<&",
        RedirectOp::OutErr => "&>",
        RedirectOp::AppendOutErr => "&>>",
        RedirectOp::Heredoc => "<<",
        RedirectOp::HeredocStrip => "<<-",
        RedirectOp::HereString => "<<<",
    };
    match r.fd {
        Some(fd) => {
            let _ = write!(out, "redirect {fd}{sym} ");
        }
        None => {
            let _ = write!(out, "redirect {sym} ");
        }
    }
    write_word(out, &r.target);
    out.push('\n');
    if let Some(body) = &r.heredoc_body {
        indent(out, depth + 1);
        out.push_str("heredoc-body: ");
        let _ = write!(out, "{body:?}");
        out.push('\n');
    }
}

fn write_simple(out: &mut String, s: &SimpleCommand, depth: usize) {
    indent(out, depth);
    out.push_str("simple\n");
    for (k, v) in &s.assignments {
        indent(out, depth + 1);
        let _ = write!(out, "assign {k}=");
        write_word(out, v);
        out.push('\n');
    }
    if let Some(p) = &s.program {
        indent(out, depth + 1);
        out.push_str("program ");
        write_word(out, p);
        out.push('\n');
    }
    for a in &s.args {
        indent(out, depth + 1);
        out.push_str("arg ");
        write_word(out, a);
        out.push('\n');
    }
    for r in &s.redirects {
        write_redirect(out, r, depth + 1);
    }
}

fn connector_label(c: Connector) -> &'static str {
    match c {
        Connector::And => "&&",
        Connector::Or => "||",
        Connector::Seq => ";",
        Connector::Pipe => "|",
    }
}

fn write_command(out: &mut String, cmd: &Command, depth: usize) {
    match cmd {
        Command::Simple(s) => write_simple(out, s, depth),
        Command::Connection(Connection { left, right, connector }) => {
            indent(out, depth);
            let _ = writeln!(out, "connection {}", connector_label(*connector));
            write_command(out, left, depth + 1);
            write_command(out, right, depth + 1);
        }
        Command::Invert(inner) => {
            indent(out, depth);
            out.push_str("invert (!)\n");
            write_command(out, inner, depth + 1);
        }
        Command::Time(inner) => {
            indent(out, depth);
            out.push_str("time\n");
            write_command(out, inner, depth + 1);
        }
        Command::Background(inner) => {
            indent(out, depth);
            out.push_str("background (&)\n");
            write_command(out, inner, depth + 1);
        }
        Command::Redirected { command, redirects } => {
            indent(out, depth);
            out.push_str("redirected\n");
            write_command(out, command, depth + 1);
            for r in redirects {
                write_redirect(out, r, depth + 1);
            }
        }
        Command::Subshell(inner) => {
            indent(out, depth);
            out.push_str("subshell ( )\n");
            write_command(out, inner, depth + 1);
        }
        Command::Group(inner) => {
            indent(out, depth);
            out.push_str("group { }\n");
            write_command(out, inner, depth + 1);
        }
        Command::For(f) => {
            indent(out, depth);
            let _ = write!(out, "for {}", f.var);
            if !f.words.is_empty() {
                out.push_str(" in");
                for w in &f.words {
                    out.push(' ');
                    write_word(out, w);
                }
            }
            out.push('\n');
            write_command(out, &f.body, depth + 1);
        }
        Command::ArithFor { expr, body } => {
            indent(out, depth);
            let _ = writeln!(out, "arith-for {expr:?}");
            write_command(out, body, depth + 1);
        }
        Command::Arith { expr } => {
            indent(out, depth);
            let _ = writeln!(out, "arith {expr:?}");
        }
        Command::If(i) => {
            indent(out, depth);
            out.push_str("if\n");
            for (cond, body) in &i.branches {
                indent(out, depth + 1);
                out.push_str("branch:\n");
                write_command(out, cond, depth + 2);
                write_command(out, body, depth + 2);
            }
            if let Some(e) = &i.else_branch {
                indent(out, depth + 1);
                out.push_str("else:\n");
                write_command(out, e, depth + 2);
            }
        }
        Command::Case(c) => {
            indent(out, depth);
            out.push_str("case ");
            write_word(out, &c.word);
            out.push('\n');
            for arm in &c.arms {
                indent(out, depth + 1);
                out.push_str("arm ");
                for p in &arm.patterns {
                    write_word(out, p);
                    out.push(' ');
                }
                out.push('\n');
                if let Some(body) = &arm.body {
                    write_command(out, body, depth + 2);
                }
            }
        }
        Command::While { cond, body } => {
            indent(out, depth);
            out.push_str("while\n");
            write_command(out, cond, depth + 1);
            write_command(out, body, depth + 1);
        }
        Command::Until { cond, body } => {
            indent(out, depth);
            out.push_str("until\n");
            write_command(out, cond, depth + 1);
            write_command(out, body, depth + 1);
        }
        Command::Cond(expr) => {
            indent(out, depth);
            out.push_str("[[ ]]\n");
            write_cond(out, expr, depth + 1);
        }
        Command::FunctionDef { name, body } => {
            indent(out, depth);
            let _ = writeln!(out, "function {name}");
            write_command(out, body, depth + 1);
        }
    }
}

fn write_cond(out: &mut String, expr: &CondExpr, depth: usize) {
    indent(out, depth);
    match expr {
        CondExpr::Or(l, r) => {
            out.push_str("or\n");
            write_cond(out, l, depth + 1);
            write_cond(out, r, depth + 1);
        }
        CondExpr::And(l, r) => {
            out.push_str("and\n");
            write_cond(out, l, depth + 1);
            write_cond(out, r, depth + 1);
        }
        CondExpr::Not(inner) => {
            out.push_str("not\n");
            write_cond(out, inner, depth + 1);
        }
        CondExpr::Group(inner) => {
            out.push_str("( )\n");
            write_cond(out, inner, depth + 1);
        }
        CondExpr::Unary { op, operand } => {
            let _ = write!(out, "unary {op} ");
            write_word(out, operand);
            out.push('\n');
        }
        CondExpr::Binary { op, left, right } => {
            let _ = write!(out, "binary {op} ");
            write_word(out, left);
            out.push_str(" ");
            write_word(out, right);
            out.push('\n');
        }
        CondExpr::Term(w) => {
            out.push_str("term ");
            write_word(out, w);
            out.push('\n');
        }
    }
}
