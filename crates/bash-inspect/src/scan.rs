//! Finding the refused constructs in the source.
//!
//! The scan drives bash-parser's own lexer rather than reading the text
//! directly. That is deliberate: quoting, comments, `$(...)` spans and heredoc
//! bodies all have to be understood to tell a construct from a word that
//! merely spells one, and CLAUDE.md's traps record what it costs when the same
//! question gets a second implementation.
//!
//! It runs over the whole source independently of the parse, so a script the
//! parser rejects at its first `select` still gets every later finding
//! reported — the reader redesigns rather than retries, and half the
//! constraint means redesigning twice.

use bash_parser::lexer::{Lexer, Token};

use crate::construct::Construct;

struct ScanWord {
    /// Quoting still in place, exactly as the lexer saw it: `"set"` is five
    /// characters here. Reserved-word status turns on that, the `set` builtin
    /// does not.
    text: String,
    quoted: bool,
}

/// The reserved words that introduce a command list without ending the command
/// the scan is accumulating. `for`, `case`, `select` and `function` are absent
/// on purpose: what follows each of them is a name, not a command.
const OPENERS: &[&str] =
    &["!", "time", "{", "if", "then", "elif", "else", "while", "until", "do"];

/// Each refused construct the source holds, in source order, once each. A
/// second occurrence carries nothing the reader can act on while findings have
/// no location to tell them apart.
pub fn scan(source: &str) -> Vec<Construct> {
    let mut lexer = Lexer::new(source);
    let mut found: Vec<Construct> = Vec::new();
    let mut current: Vec<ScanWord> = Vec::new();
    let mut redirect_target = false;
    let mut pending_heredoc: Option<bool> = None;

    while let Ok(token) = lexer.next_token() {
        match token {
            Token::Word(text, quoted) => {
                if let Some(strip) = pending_heredoc.take() {
                    // Register the delimiter the way the parser does. Without
                    // it the lexer never captures the body, and every line of
                    // a heredoc comes back as ordinary word tokens — a
                    // `set -o posix` written inside one would read as a
                    // command.
                    let bare = text.chars().filter(|c| *c != '\'' && *c != '"').collect();
                    lexer.register_heredoc(bare, strip);
                }
                if redirect_target {
                    redirect_target = false;
                } else {
                    current.push(ScanWord { text, quoted });
                }
            }
            Token::DLess => {
                redirect_target = true;
                pending_heredoc = Some(false);
            }
            Token::DLessDash => {
                redirect_target = true;
                pending_heredoc = Some(true);
            }
            Token::Great
            | Token::DGreat
            | Token::Less
            | Token::DLessLess
            | Token::GreatAmp
            | Token::LessAmp
            | Token::AmpGreat
            | Token::AmpDGreat => redirect_target = true,
            Token::Fd(_) | Token::Arith(_) => {}
            Token::Eof => {
                check(&current, &mut found);
                break;
            }
            _ => {
                check(&current, &mut found);
                current.clear();
            }
        }
    }
    found
}

fn check(words: &[ScanWord], found: &mut Vec<Construct>) {
    let mut i = 0;
    while let Some(word) = words.get(i) {
        if (!word.quoted && OPENERS.contains(&word.text.as_str())) || is_assignment(&word.text) {
            i += 1;
        } else {
            break;
        }
    }
    let Some(word) = words.get(i) else { return };

    let construct = if !word.quoted && word.text == "select" {
        Construct::Select
    } else if !word.quoted && word.text == "coproc" {
        Construct::Coproc
    } else if literal(&word.text).as_deref() == Some("set")
        && posix_option(&words[i + 1..])
    {
        Construct::PosixMode
    } else {
        return;
    };
    if !found.contains(&construct) {
        found.push(construct);
    }
}

/// `set` reads the option name from the following word whenever `o` appears in
/// the flag bundle, and treats a following word that itself starts with a dash
/// as no name at all. Verified against bash 5.3: `set -o posix`, `set -xo
/// posix` and `set -ox posix` all turn posix mode on, while `set -o -x posix`
/// merely prints the option list.
fn posix_option(args: &[ScanWord]) -> bool {
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

/// `NAME=value` or `NAME+=value` in front of the command word, mirroring the
/// parser's own assignment-prefix test.
fn is_assignment(text: &str) -> bool {
    let Some(eq) = text.find('=') else { return false };
    let name = text[..eq].strip_suffix('+').unwrap_or(&text[..eq]);
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// The word's value with quoting removed, or `None` when the value cannot be
/// known without running: a `$` or a backtick anywhere makes it computed.
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
