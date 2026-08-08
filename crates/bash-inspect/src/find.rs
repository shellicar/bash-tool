//! Walking the tree for constructs the walker cannot execute.
//!
//! The match on `Command` is exhaustive on purpose. `select` and `coproc` have
//! no node yet, so nothing here can produce them; when the parser gains those
//! nodes this file stops compiling, which is the point. A construct that
//! becomes representable and is silently not inspected is the failure worth
//! spending a compile error on.

use bash_parser::{Command, Word};

use crate::construct::Construct;

/// Every refused construct the tree holds, in tree order, once each. A second
/// occurrence carries nothing the reader can act on while findings have no
/// location to tell them apart.
pub fn find(cmd: &Command) -> Vec<Construct> {
    let mut found = Vec::new();
    walk(cmd, &mut found);
    found
}

fn note(found: &mut Vec<Construct>, construct: Construct) {
    if !found.contains(&construct) {
        found.push(construct);
    }
}

fn walk(cmd: &Command, found: &mut Vec<Construct>) {
    match cmd {
        Command::Simple(s) => {
            if s.program.as_ref().and_then(|p| literal(&p.text)).as_deref() == Some("set")
                && posix_option(&s.args)
            {
                note(found, Construct::PosixMode);
            }
        }
        Command::Connection(c) => {
            walk(&c.left, found);
            walk(&c.right, found);
        }
        Command::Invert(c)
        | Command::Time(c)
        | Command::Background(c)
        | Command::Subshell(c)
        | Command::Group(c) => walk(c, found),
        Command::Redirected { command, .. } => walk(command, found),
        Command::FunctionDef { body, .. } => walk(body, found),
        Command::For(f) => walk(&f.body, found),
        Command::ArithFor { body, .. } => walk(body, found),
        Command::While { cond, body } | Command::Until { cond, body } => {
            walk(cond, found);
            walk(body, found);
        }
        Command::If(i) => {
            for (cond, body) in &i.branches {
                walk(cond, found);
                walk(body, found);
            }
            if let Some(body) = &i.else_branch {
                walk(body, found);
            }
        }
        Command::Case(c) => {
            for body in c.arms.iter().filter_map(|a| a.body.as_ref()) {
                walk(body, found);
            }
        }
        // Arithmetic holds no commands, and a heredoc body is never bash
        // syntax — the parser captures it verbatim and never tokenizes it.
        Command::Cond(_) | Command::Arith { .. } => {}
    }
}

/// `set` reads the option name from the following word whenever `o` appears in
/// the flag bundle, and treats a following word that itself starts with a dash
/// as no name at all. Verified against bash 5.3: `set -o posix`, `set -xo
/// posix` and `set -ox posix` all turn posix mode on, while `set -o -x posix`
/// merely prints the option list.
fn posix_option(args: &[Word]) -> bool {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let Some(flag) = literal(&arg.text) else { return false };
        if flag == "--" || !flag.starts_with(['-', '+']) {
            return false;
        }
        if flag[1..].contains('o') {
            match args.get(i + 1).map(|w| literal(&w.text)) {
                // A computed option name is not knowable here; CLAUDE.md keeps
                // that case on its runtime refusal.
                Some(None) => return false,
                Some(Some(name)) if !name.starts_with(['-', '+']) => {
                    if name == "posix" {
                        return true;
                    }
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    false
}

/// The word's value with quoting removed, or `None` when the value cannot be
/// known without running: a `$` or a backtick anywhere makes it computed.
/// `Word.text` keeps its quotes, so `"set"` arrives here five characters long.
fn literal(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '$' | '`' => return None,
            '\\' => out.push(chars.next()?),
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    c => out.push(c),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '$' | '`' => return None,
                    // Inside double quotes a backslash escapes only these; in
                    // front of anything else it stays a literal backslash.
                    '\\' => match chars.next()? {
                        c @ ('$' | '`' | '"' | '\\' | '\n') => out.push(c),
                        c => {
                            out.push('\\');
                            out.push(c);
                        }
                    },
                    c => out.push(c),
                }
            },
            c => out.push(c),
        }
    }
    Some(out)
}
