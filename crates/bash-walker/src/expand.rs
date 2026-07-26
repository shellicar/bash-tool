//! Word expansion, bash's pipeline in bash's order: brace expansion, tilde,
//! parameter/command/arithmetic expansion, field splitting (IFS), pathname
//! expansion (glob), quote removal. Words arrive with their quotes intact
//! (the parser deliberately keeps them); quote context decides what each
//! stage may touch.
//!
//! Unsupported forms fail loudly by name (`Flow::Fatal`) — a wrong expansion
//! is worse than an honest error.

use std::path::PathBuf;

use bash_parser::Word;

use crate::arith;
use crate::walk::{Ctx, Exec, Flow};

/// One expanded fragment. `quoted` fragments are immune to field splitting
/// and glob-metacharacter interpretation; `Break` is a field boundary from
/// IFS splitting (`hard` = produced by an explicit non-whitespace IFS
/// delimiter, which keeps an empty field alive where whitespace would not).
#[derive(Debug, Clone)]
enum Item {
    Text { s: String, quoted: bool },
    Break { hard: bool },
}

/// Full expansion to argv fields: every stage, splitting and globbing on.
pub fn expand_fields(ex: &mut Exec, ctx: &Ctx, words: &[Word]) -> Result<Vec<String>, Flow> {
    let mut fields = Vec::new();
    for word in words {
        for raw in brace_expand(&word.text) {
            let items = expand_items(ex, ctx, &raw, true)?;
            assemble(&mut fields, items, true);
        }
    }
    Ok(fields)
}

/// One string, no splitting, no globbing, quotes removed — assignment
/// values, case subjects, `[[ ]]` operands, heredoc targets.
pub fn expand_single(ex: &mut Exec, ctx: &Ctx, word: &Word) -> Result<String, Flow> {
    let items = expand_items(ex, ctx, &word.text, false)?;
    Ok(items
        .into_iter()
        .filter_map(|i| match i {
            Item::Text { s, .. } => Some(s),
            Item::Break { .. } => Some(" ".to_string()),
        })
        .collect())
}

/// A redirect target: expanded with splitting+globbing, then required to be
/// exactly one field — bash's "ambiguous redirect" rule.
pub fn expand_redirect_target(ex: &mut Exec, ctx: &Ctx, word: &Word) -> Result<String, Flow> {
    let fields = expand_fields(ex, ctx, std::slice::from_ref(word))?;
    if fields.len() != 1 {
        return Err(Flow::RedirectFailed(format!("{}: ambiguous redirect", word.text)));
    }
    Ok(fields.into_iter().next().unwrap())
}

/// Expanded (fragment, was-quoted) parts — the caller builds a glob pattern
/// or a regex from them, escaping the quoted parts, so `"$x"*` stays literal
/// text plus a live star.
pub fn expand_parts(ex: &mut Exec, ctx: &Ctx, word: &Word) -> Result<Vec<(String, bool)>, Flow> {
    let items = expand_items(ex, ctx, &word.text, false)?;
    Ok(items
        .into_iter()
        .filter_map(|i| match i {
            Item::Text { s, quoted } => Some((s, quoted)),
            Item::Break { .. } => Some((" ".to_string(), false)),
        })
        .collect())
}

pub fn glob_pattern_from_parts(parts: &[(String, bool)]) -> String {
    let mut pat = String::new();
    for (s, quoted) in parts {
        if *quoted {
            pat.push_str(&glob::Pattern::escape(s));
        } else {
            pat.push_str(s);
        }
    }
    pat
}

pub fn regex_from_parts(parts: &[(String, bool)]) -> String {
    let mut pat = String::new();
    for (s, quoted) in parts {
        if *quoted {
            pat.push_str(&regex::escape(s));
        } else {
            pat.push_str(s);
        }
    }
    pat
}

/// Heredoc bodies and arithmetic interiors: only `$`-constructs and
/// backticks expand; quotes are ordinary characters. `\$`, `` \` ``, `\\`
/// drop their backslash (bash's heredoc escape set).
pub fn expand_textual(ex: &mut Exec, ctx: &Ctx, raw: &str) -> Result<String, Flow> {
    let b = raw.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if i + 1 < b.len() && matches!(b[i + 1], b'$' | b'`' | b'\\') => {
                out.push(b[i + 1] as char);
                i += 2;
            }
            // same latent infinite loop as the dquote arm: an unrecognized
            // backslash escape must still advance
            b'\\' => {
                out.push('\\');
                i += 1;
            }
            b'$' | b'`' => {
                let (text, next) = expand_dollar(ex, ctx, raw, i)?;
                match text {
                    Expanded::One(s) => out.push_str(&s),
                    Expanded::Many(fields) => out.push_str(&fields.join(" ")),
                    Expanded::NotSpecial => {
                        out.push(b[i] as char);
                        i += 1;
                        continue;
                    }
                }
                i = next;
            }
            _ => {
                let start = i;
                while i < b.len() && !matches!(b[i], b'\\' | b'$' | b'`') {
                    i += 1;
                }
                out.push_str(&raw[start..i]);
            }
        }
    }
    Ok(out)
}

/// Field assembly: concatenate items into fields at the breaks, apply the
/// empty-field rules, then pathname-expand fields that still carry live
/// glob metacharacters.
fn assemble(fields: &mut Vec<String>, items: Vec<Item>, do_glob: bool) {
    let mut text = String::new();
    let mut pattern = String::new();
    let mut has_quoted = false;
    let mut has_live_glob = false;
    let mut started = false;

    let finish = |text: &mut String,
                      pattern: &mut String,
                      has_quoted: &mut bool,
                      has_live_glob: &mut bool,
                      started: &mut bool,
                      fields: &mut Vec<String>,
                      unconditional: bool| {
        if !text.is_empty() || *has_quoted || unconditional {
            if do_glob && *has_live_glob {
                fields.extend(glob_field(text, pattern));
            } else {
                fields.push(std::mem::take(text));
            }
        }
        text.clear();
        pattern.clear();
        *has_quoted = false;
        *has_live_glob = false;
        *started = false;
    };

    for item in items {
        match item {
            Item::Text { s, quoted } => {
                started = true;
                if quoted {
                    has_quoted = true;
                    pattern.push_str(&glob::Pattern::escape(&s));
                } else {
                    if s.contains(['*', '?', '[']) {
                        has_live_glob = true;
                    }
                    pattern.push_str(&s);
                }
                text.push_str(&s);
            }
            Item::Break { hard } => {
                // A soft (whitespace) leading break with nothing before it
                // produces no field; a hard delimiter always closes one.
                if hard {
                    finish(&mut text, &mut pattern, &mut has_quoted, &mut has_live_glob, &mut started, fields, true);
                } else if started {
                    finish(&mut text, &mut pattern, &mut has_quoted, &mut has_live_glob, &mut started, fields, false);
                }
            }
        }
    }
    finish(&mut text, &mut pattern, &mut has_quoted, &mut has_live_glob, &mut started, fields, false);
}

/// Pathname expansion for one field. No match leaves the word as-is (bash
/// with nullglob off); results come back sorted from the glob crate.
fn glob_field(text: &str, pattern: &str) -> Vec<String> {
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: true,
    };
    match glob::glob_with(pattern, options) {
        Ok(paths) => {
            let matches: Vec<String> = paths
                .filter_map(Result::ok)
                .map(|p: PathBuf| p.to_string_lossy().into_owned())
                .collect();
            if matches.is_empty() {
                vec![text.to_string()]
            } else {
                matches
            }
        }
        Err(_) => vec![text.to_string()],
    }
}

/// Brace expansion: `{a,b}` alternatives and `{n..m}`/`{a..z}` ranges,
/// nested, quote- and `$`-aware (a `${...}` or quoted brace never expands).
/// Purely textual and first, exactly like bash.
fn brace_expand(raw: &str) -> Vec<String> {
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'$' => {
                // skip the whole ${...}/$(...) span so its braces stay
                i += 1;
                if i < b.len() && (b[i] == b'{' || b[i] == b'(') {
                    let (open, close) = if b[i] == b'{' { (b'{', b'}') } else { (b'(', b')') };
                    let mut depth = 0;
                    while i < b.len() {
                        if b[i] == open {
                            depth += 1;
                        } else if b[i] == close {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        i += 1;
                    }
                }
            }
            b'{' => {
                if let Some((alts, end)) = scan_brace_alternatives(b, i) {
                    let prefix = &raw[..i];
                    let suffix = &raw[end + 1..];
                    let mut out = Vec::new();
                    for alt in alts {
                        for rest in brace_expand(&format!("{prefix}{alt}{suffix}")) {
                            out.push(rest);
                        }
                    }
                    return out;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    vec![raw.to_string()]
}

/// The alternatives inside a brace at `start`, or None if this brace is not
/// an expansion (no top-level comma and not a range).
fn scan_brace_alternatives(b: &[u8], start: usize) -> Option<(Vec<String>, usize)> {
    let mut depth = 0;
    let mut i = start;
    let mut splits = vec![start];
    let mut end = None;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            b',' if depth == 1 => splits.push(i),
            _ => {}
        }
        i += 1;
    }
    let end = end?;
    let inner = std::str::from_utf8(&b[start + 1..end]).ok()?;
    if splits.len() == 1 {
        return range_alternatives(inner).map(|alts| (alts, end));
    }
    let mut alts = Vec::new();
    let mut prev = start + 1;
    for s in splits.iter().skip(1) {
        alts.push(String::from_utf8_lossy(&b[prev..*s]).into_owned());
        prev = s + 1;
    }
    alts.push(String::from_utf8_lossy(&b[prev..end]).into_owned());
    Some((alts, end))
}

fn range_alternatives(inner: &str) -> Option<Vec<String>> {
    let (from, rest) = inner.split_once("..")?;
    let (to, step) = match rest.split_once("..") {
        Some((to, step)) => (to, step.parse::<i64>().ok()?),
        None => (rest, 1),
    };
    let step = if step == 0 { 1 } else { step.abs() };
    if let (Ok(a), Ok(b)) = (from.parse::<i64>(), to.parse::<i64>()) {
        let width = if (from.starts_with('0') && from.len() > 1)
            || (to.starts_with('0') && to.len() > 1)
        {
            from.len().max(to.len())
        } else {
            0
        };
        let mut out = Vec::new();
        let mut v = a;
        loop {
            out.push(format!("{v:0width$}"));
            if a <= b {
                v += step;
                if v > b {
                    break;
                }
            } else {
                v -= step;
                if v < b {
                    break;
                }
            }
        }
        return Some(out);
    }
    let (ac, bc) = (single_char(from)?, single_char(to)?);
    let (mut v, end) = (ac as u32, bc as u32);
    let mut out = Vec::new();
    loop {
        out.push(char::from_u32(v)?.to_string());
        if ac <= bc {
            v += step as u32;
            if v > end {
                break;
            }
        } else {
            v -= step as u32;
            if v < end {
                break;
            }
        }
    }
    Some(out)
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

enum Expanded {
    One(String),
    /// `$@`/`$*`: pre-separated values (the caller decides field breaks).
    Many(Vec<String>),
    /// The `$` was not a live expansion (e.g. `$` at end of word).
    NotSpecial,
}

/// The character walk over one (brace-expanded) raw word.
fn expand_items(ex: &mut Exec, ctx: &Ctx, raw: &str, split: bool) -> Result<Vec<Item>, Flow> {
    let raw = tilde_expand(ex, raw);
    let b = raw.as_bytes();
    let mut items = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                items.push(Item::Text { s: raw[start..i].to_string(), quoted: true });
                i += 1; // closing quote
            }
            b'"' => {
                i += 1;
                let mut lit = String::new();
                while i < b.len() && b[i] != b'"' {
                    match b[i] {
                        b'\\' if i + 1 < b.len()
                            && matches!(b[i + 1], b'$' | b'`' | b'"' | b'\\') =>
                        {
                            lit.push(b[i + 1] as char);
                            i += 2;
                        }
                        b'\\' if i + 1 < b.len() && b[i + 1] == b'\n' => i += 2,
                        // backslash before an ordinary char stays literal
                        // (`\|` in `grep "a\|b"`). Without this arm the run-
                        // slurp below never advances past the backslash — an
                        // INFINITE LOOP found live: the walker hung on
                        // `grep "x\|y"` while bash took 0.1s.
                        b'\\' => {
                            lit.push('\\');
                            i += 1;
                        }
                        b'$' | b'`' => {
                            match expand_dollar(ex, ctx, &raw, i)? {
                                (Expanded::One(s), next) => {
                                    lit.push_str(&s);
                                    i = next;
                                }
                                (Expanded::Many(vals), next) => {
                                    // "$@": each value its own field, joined
                                    // to whatever literal text surrounds it.
                                    for (k, v) in vals.iter().enumerate() {
                                        if k == 0 {
                                            lit.push_str(v);
                                        } else {
                                            items.push(Item::Text { s: std::mem::take(&mut lit), quoted: true });
                                            items.push(Item::Break { hard: true });
                                            lit.push_str(v);
                                        }
                                    }
                                    i = next;
                                }
                                (Expanded::NotSpecial, _) => {
                                    lit.push(b[i] as char);
                                    i += 1;
                                }
                            }
                        }
                        _ => {
                            // slurp the literal run as a str slice — per-byte
                            // `as char` pushes mangle multibyte UTF-8
                            let start = i;
                            while i < b.len() && !matches!(b[i], b'"' | b'\\' | b'$' | b'`') {
                                i += 1;
                            }
                            lit.push_str(&raw[start..i]);
                        }
                    }
                }
                items.push(Item::Text { s: lit, quoted: true });
                i += 1;
            }
            b'\\' => {
                if i + 1 < b.len() {
                    if b[i + 1] == b'\n' {
                        i += 2; // line continuation vanishes
                    } else {
                        // escape the WHOLE next char, not just its first byte
                        let n = utf8_len(b[i + 1]);
                        let end = (i + 1 + n).min(b.len());
                        items.push(Item::Text { s: raw[i + 1..end].to_string(), quoted: true });
                        i = end;
                    }
                } else {
                    items.push(Item::Text { s: "\\".to_string(), quoted: true });
                    i += 1;
                }
            }
            b'$' if i + 1 < b.len() && b[i + 1] == b'\'' => {
                let (s, next) = ansi_c_quote(&raw, i + 1);
                items.push(Item::Text { s, quoted: true });
                i = next;
            }
            b'$' | b'`' => match expand_dollar(ex, ctx, &raw, i)? {
                (Expanded::One(s), next) => {
                    if split {
                        push_split(&mut items, &s, ex);
                    } else {
                        items.push(Item::Text { s, quoted: false });
                    }
                    i = next;
                }
                (Expanded::Many(vals), next) => {
                    for (k, v) in vals.iter().enumerate() {
                        if k > 0 {
                            items.push(Item::Break { hard: true });
                        }
                        if split {
                            push_split(&mut items, v, ex);
                        } else {
                            items.push(Item::Text { s: v.clone(), quoted: false });
                        }
                    }
                    i = next;
                }
                (Expanded::NotSpecial, _) => {
                    items.push(Item::Text { s: (b[i] as char).to_string(), quoted: false });
                    i += 1;
                }
            },
            b'<' | b'>' if i + 1 < b.len() && b[i + 1] == b'(' => {
                let end = matched_paren(b, i + 1)
                    .ok_or_else(|| Flow::Fatal("unbalanced process substitution".into()))?;
                if b[i] == b'>' {
                    return Err(Flow::Fatal(
                        ">(...) process substitution is not supported by bash-walker".into(),
                    ));
                }
                let inner = &raw[i + 2..end];
                let path = crate::walk::run_procsub(ex, ctx, inner)?;
                items.push(Item::Text { s: path, quoted: true });
                i = end + 1;
            }
            _ => {
                let start = i;
                while i < b.len()
                    && !matches!(b[i], b'\'' | b'"' | b'\\' | b'$' | b'`')
                    && !(matches!(b[i], b'<' | b'>') && i + 1 < b.len() && b[i + 1] == b'(')
                {
                    i += 1;
                }
                items.push(Item::Text { s: raw[start..i].to_string(), quoted: false });
            }
        }
    }
    Ok(items)
}

/// IFS-split one expansion result into items. Whitespace runs are soft
/// breaks; a non-whitespace IFS character is a hard break (keeps empty
/// fields). IFS unset = default; IFS empty = no splitting at all.
fn push_split(items: &mut Vec<Item>, value: &str, ex: &Exec) {
    let ifs = ex.state.get_var("IFS").unwrap_or_else(|| " \t\n".to_string());
    if ifs.is_empty() {
        items.push(Item::Text { s: value.to_string(), quoted: false });
        return;
    }
    let ws: Vec<char> = ifs.chars().filter(|c| c.is_whitespace()).collect();
    let hard: Vec<char> = ifs.chars().filter(|c| !c.is_whitespace()).collect();
    let mut cur = String::new();
    let mut pending_soft = false;
    let mut any = false;
    for c in value.chars() {
        if ws.contains(&c) {
            if !cur.is_empty() {
                items.push(Item::Text { s: std::mem::take(&mut cur), quoted: false });
                any = true;
            }
            pending_soft = true;
        } else if hard.contains(&c) {
            if !cur.is_empty() {
                items.push(Item::Text { s: std::mem::take(&mut cur), quoted: false });
            }
            items.push(Item::Break { hard: true });
            pending_soft = false;
            any = true;
        } else {
            if pending_soft {
                items.push(Item::Break { hard: false });
                pending_soft = false;
            }
            cur.push(c);
        }
    }
    if pending_soft && !cur.is_empty() {
        items.push(Item::Break { hard: false });
    }
    if !cur.is_empty() {
        items.push(Item::Text { s: cur, quoted: false });
    } else if !any && value.is_empty() {
        // empty value: contributes nothing, but an adjacent literal still
        // forms a field — nothing to push.
    }
    // trailing whitespace: a soft break only matters if more content follows,
    // which the next item naturally handles.
    if pending_soft {
        items.push(Item::Break { hard: false });
    }
}

fn tilde_expand(ex: &Exec, raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            if let Some(home) = ex.state.get_var("HOME") {
                return format!("{home}{rest}");
            }
        }
    }
    raw.to_string()
}

fn ansi_c_quote(raw: &str, quote_start: usize) -> (String, usize) {
    let b = raw.as_bytes();
    let mut out = String::new();
    let mut i = quote_start + 1;
    while i < b.len() && b[i] != b'\'' {
        if b[i] != b'\\' {
            let start = i;
            while i < b.len() && b[i] != b'\\' && b[i] != b'\'' {
                i += 1;
            }
            out.push_str(&raw[start..i]);
            continue;
        }
        if i + 1 < b.len() {
            let (c, used) = match b[i + 1] {
                b'n' => ('\n', 2),
                b't' => ('\t', 2),
                b'r' => ('\r', 2),
                b'\\' => ('\\', 2),
                b'\'' => ('\'', 2),
                b'"' => ('"', 2),
                b'0' => ('\0', 2),
                b'a' => ('\x07', 2),
                b'b' => ('\x08', 2),
                b'e' => ('\x1b', 2),
                b'f' => ('\x0c', 2),
                b'v' => ('\x0b', 2),
                b'x' => {
                    let hex: String = raw[i + 2..]
                        .chars()
                        .take_while(|c| c.is_ascii_hexdigit())
                        .take(2)
                        .collect();
                    let v = u8::from_str_radix(&hex, 16).unwrap_or(b'x');
                    (v as char, 2 + hex.len())
                }
                other => (other as char, 2),
            };
            out.push(c);
            i += used;
        } else {
            out.push('\\');
            i += 1;
        }
    }
    (out, i + 1)
}

fn utf8_len(b0: u8) -> usize {
    match b0 {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

fn matched_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn matched_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// One `$...`/`` `...` `` construct at byte `at`. Returns the value and the
/// index just past the construct.
fn expand_dollar(
    ex: &mut Exec,
    ctx: &Ctx,
    raw: &str,
    at: usize,
) -> Result<(Expanded, usize), Flow> {
    let b = raw.as_bytes();
    if b[at] == b'`' {
        let mut end = at + 1;
        while end < b.len() && b[end] != b'`' {
            if b[end] == b'\\' {
                end += 1;
            }
            end += 1;
        }
        let inner = raw[at + 1..end.min(b.len())].replace("\\`", "`").replace("\\$", "$");
        let out = crate::walk::run_capture(ex, ctx, &inner)?;
        return Ok((Expanded::One(out), (end + 1).min(b.len())));
    }
    debug_assert_eq!(b[at], b'$');
    if at + 1 >= b.len() {
        return Ok((Expanded::NotSpecial, at));
    }
    match b[at + 1] {
        b'(' if at + 2 < b.len() && b[at + 2] == b'(' => {
            // $((...)) — but honor bash's own tie-break: if the span doesn't
            // close with )), it was $( (subshell...) ).
            if let Some(end) = matched_paren(b, at + 1) {
                if end >= 1 && b[end - 1] == b')' && matched_paren(b, at + 2) == Some(end - 1) {
                    let inner = &raw[at + 3..end - 1];
                    let text = expand_textual(ex, ctx, inner)?;
                    let v = arith::eval(&text, ex.state)
                        .map_err(|e| Flow::Fatal(e.to_string()))?;
                    return Ok((Expanded::One(v.to_string()), end + 1));
                }
            }
            let end = matched_paren(b, at + 1)
                .ok_or_else(|| Flow::Fatal("unbalanced $(".into()))?;
            let out = crate::walk::run_capture(ex, ctx, &raw[at + 2..end])?;
            Ok((Expanded::One(out), end + 1))
        }
        b'(' => {
            let end = matched_paren(b, at + 1)
                .ok_or_else(|| Flow::Fatal("unbalanced $(".into()))?;
            let out = crate::walk::run_capture(ex, ctx, &raw[at + 2..end])?;
            Ok((Expanded::One(out), end + 1))
        }
        b'{' => {
            let end = matched_brace(b, at + 1)
                .ok_or_else(|| Flow::Fatal("unbalanced ${".into()))?;
            let inner = &raw[at + 2..end];
            let v = expand_braced_param(ex, ctx, inner)?;
            Ok((v, end + 1))
        }
        c if c.is_ascii_alphabetic() || c == b'_' => {
            let mut end = at + 1;
            while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
                end += 1;
            }
            let name = &raw[at + 1..end];
            Ok((param_value(ex, name)?, end))
        }
        c if c.is_ascii_digit() => {
            let name = &raw[at + 1..at + 2];
            Ok((param_value(ex, name)?, at + 2))
        }
        b'?' | b'$' | b'!' | b'#' | b'@' | b'*' | b'-' => {
            let name = &raw[at + 1..at + 2];
            Ok((param_value(ex, name)?, at + 2))
        }
        _ => Ok((Expanded::NotSpecial, at)),
    }
}

/// A parameter's bare value, special names included.
fn param_value(ex: &mut Exec, name: &str) -> Result<Expanded, Flow> {
    let v = match name {
        "?" => Some(ex.state.last_status.to_string()),
        "$" => Some(std::process::id().to_string()),
        "!" => ex.state.last_background_pid.map(|p| p.to_string()),
        "#" => Some(ex.state.positional.len().to_string()),
        "@" | "*" => return Ok(Expanded::Many(ex.state.positional.clone())),
        "-" => Some(String::new()),
        "0" => Some("bash".to_string()),
        d if d.chars().all(|c| c.is_ascii_digit()) => {
            let n: usize = d.parse().unwrap();
            if n == 0 {
                Some("bash".to_string())
            } else {
                ex.state.positional.get(n - 1).cloned()
            }
        }
        "RANDOM" => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            Some((nanos % 32768).to_string())
        }
        _ => ex.state.get_var(name),
    };
    match v {
        Some(v) => Ok(Expanded::One(v)),
        None if ex.state.flags.nounset => {
            Err(Flow::Fatal(format!("{name}: unbound variable")))
        }
        None => Ok(Expanded::One(String::new())),
    }
}

/// `${...}` operator forms. Anything not implemented errors by name.
fn expand_braced_param(ex: &mut Exec, ctx: &Ctx, inner: &str) -> Result<Expanded, Flow> {
    // ${#NAME} — length
    if let Some(name) = inner.strip_prefix('#') {
        if !name.is_empty() && name != "@" && name != "*" {
            if let Expanded::One(v) = param_value(ex, name)? {
                return Ok(Expanded::One(v.chars().count().to_string()));
            }
        }
    }
    if inner.starts_with('!') {
        return Err(Flow::Fatal(format!(
            "${{{inner}}}: indirect expansion is not supported by bash-walker"
        )));
    }
    // BASH_REMATCH[n] — the one array read that must work (=~ fills it).
    if let Some(idx) = inner
        .strip_prefix("BASH_REMATCH[")
        .and_then(|r| r.strip_suffix(']'))
    {
        let n: usize = idx
            .parse()
            .map_err(|_| Flow::Fatal(format!("${{{inner}}}: bad subscript")))?;
        return Ok(Expanded::One(
            ex.state.rematch.get(n).cloned().unwrap_or_default(),
        ));
    }

    // Split NAME from the operator.
    let name_end = inner
        .char_indices()
        .find(|(k, c)| {
            !(c.is_ascii_alphanumeric() || *c == '_') && !(*k == 0 && "?@*#!$-0123456789".contains(*c))
        })
        .map(|(k, _)| k)
        .unwrap_or(inner.len());
    let (name, op) = inner.split_at(name_end);
    if name.is_empty() {
        return Err(Flow::Fatal(format!("${{{inner}}}: bad substitution")));
    }
    if op.starts_with('[') {
        return Err(Flow::Fatal(format!(
            "${{{inner}}}: arrays are not supported by bash-walker"
        )));
    }
    let current = match param_value(ex, name)? {
        Expanded::One(v) if v.is_empty() && ex.state.get_var(name).is_none() && !is_special(name) => None,
        Expanded::One(v) => Some(v),
        many @ Expanded::Many(_) => {
            if op.is_empty() {
                return Ok(many);
            }
            return Err(Flow::Fatal(format!(
                "${{{inner}}}: operators on $@/$* are not supported by bash-walker"
            )));
        }
        Expanded::NotSpecial => None,
    };
    if op.is_empty() {
        return Ok(Expanded::One(current.unwrap_or_default()));
    }

    let word_of = |ex: &mut Exec, w: &str| -> Result<String, Flow> {
        let items = expand_items(ex, ctx, w, false)?;
        Ok(items
            .into_iter()
            .map(|i| match i {
                Item::Text { s, .. } => s,
                Item::Break { .. } => " ".to_string(),
            })
            .collect())
    };

    // :- - := = :+ + :? ?
    for (pat, colon) in [(":-", true), (":=", true), (":+", true), (":?", true), ("-", false), ("=", false), ("+", false), ("?", false)] {
        if let Some(w) = op.strip_prefix(pat) {
            let kind = pat.trim_start_matches(':');
            let unset = current.is_none();
            let empty_counts = colon;
            let use_word = match kind {
                "+" => !(unset || (empty_counts && current.as_deref() == Some(""))),
                _ => unset || (empty_counts && current.as_deref() == Some("")),
            };
            return match kind {
                "+" => Ok(Expanded::One(if use_word {
                    word_of(ex, w)?
                } else {
                    String::new()
                })),
                "-" => Ok(Expanded::One(if use_word {
                    word_of(ex, w)?
                } else {
                    current.unwrap_or_default()
                })),
                "=" => {
                    if use_word {
                        let v = word_of(ex, w)?;
                        ex.state.set_var(name, v.clone());
                        Ok(Expanded::One(v))
                    } else {
                        Ok(Expanded::One(current.unwrap_or_default()))
                    }
                }
                "?" => {
                    if use_word {
                        let msg = if w.is_empty() {
                            "parameter null or not set".to_string()
                        } else {
                            word_of(ex, w)?
                        };
                        Err(Flow::Fatal(format!("{name}: {msg}")))
                    } else {
                        Ok(Expanded::One(current.unwrap_or_default()))
                    }
                }
                _ => unreachable!(),
            };
        }
    }

    let value = current.unwrap_or_default();

    // ${x#pat} ${x##pat} ${x%pat} ${x%%pat}
    for (pfx, prefix_side, longest) in
        [("##", true, true), ("#", true, false), ("%%", false, true), ("%", false, false)]
    {
        if let Some(pat) = op.strip_prefix(pfx) {
            let pat = word_of(ex, pat)?;
            let pattern = glob::Pattern::new(&pat)
                .map_err(|e| Flow::Fatal(format!("bad pattern {pat:?}: {e}")))?;
            return Ok(Expanded::One(strip_match(&value, &pattern, prefix_side, longest)));
        }
    }

    // ${x/pat/rep} ${x//pat/rep}
    if let Some(rest) = op.strip_prefix('/') {
        let (all, rest) = match rest.strip_prefix('/') {
            Some(r) => (true, r),
            None => (false, rest),
        };
        if rest.starts_with('#') || rest.starts_with('%') {
            return Err(Flow::Fatal(format!(
                "${{{inner}}}: anchored substitution is not supported by bash-walker"
            )));
        }
        let (pat, rep) = match rest.split_once('/') {
            Some((p, r)) => (p, r),
            None => (rest, ""),
        };
        let pat = word_of(ex, pat)?;
        let rep = word_of(ex, rep)?;
        let pattern = glob::Pattern::new(&pat)
            .map_err(|e| Flow::Fatal(format!("bad pattern {pat:?}: {e}")))?;
        return Ok(Expanded::One(substitute(&value, &pattern, &rep, all)));
    }

    // ${x:off} ${x:off:len} — offsets are arithmetic.
    if let Some(rest) = op.strip_prefix(':') {
        let (off_s, len_s) = match split_top_colon(rest) {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };
        let off = arith::eval(&expand_textual(ex, ctx, off_s)?, ex.state)
            .map_err(|e| Flow::Fatal(e.to_string()))?;
        let chars: Vec<char> = value.chars().collect();
        let n = chars.len() as i64;
        let start = if off < 0 { (n + off).max(0) } else { off.min(n) } as usize;
        let end = match len_s {
            None => chars.len(),
            Some(ls) => {
                let l = arith::eval(&expand_textual(ex, ctx, ls)?, ex.state)
                    .map_err(|e| Flow::Fatal(e.to_string()))?;
                if l < 0 {
                    ((n + l).max(start as i64)) as usize
                } else {
                    (start + l as usize).min(chars.len())
                }
            }
        };
        return Ok(Expanded::One(chars[start..end.max(start)].iter().collect()));
    }

    // ${x^^} ${x,,} ${x^} ${x,}
    match op {
        "^^" => return Ok(Expanded::One(value.to_uppercase())),
        ",," => return Ok(Expanded::One(value.to_lowercase())),
        "^" => {
            let mut cs = value.chars();
            return Ok(Expanded::One(match cs.next() {
                Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
                None => value,
            }));
        }
        "," => {
            let mut cs = value.chars();
            return Ok(Expanded::One(match cs.next() {
                Some(c) => c.to_lowercase().collect::<String>() + cs.as_str(),
                None => value,
            }));
        }
        _ => {}
    }

    Err(Flow::Fatal(format!(
        "${{{inner}}}: this expansion form is not supported by bash-walker"
    )))
}

fn is_special(name: &str) -> bool {
    matches!(name, "?" | "$" | "!" | "#" | "@" | "*" | "-" | "0" | "RANDOM")
        || name.chars().all(|c| c.is_ascii_digit())
}

/// Shortest/longest prefix or suffix strip against a glob pattern.
fn strip_match(value: &str, pattern: &glob::Pattern, prefix: bool, longest: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    let n = chars.len();
    if prefix {
        let range: Vec<usize> = if longest {
            (0..=n).rev().collect()
        } else {
            (0..=n).collect()
        };
        for k in range {
            let head: String = chars[..k].iter().collect();
            if pattern.matches(&head) {
                return chars[k..].iter().collect();
            }
        }
    } else {
        let range: Vec<usize> = if longest {
            (0..=n).collect()
        } else {
            (0..=n).rev().collect()
        };
        for k in range {
            let tail: String = chars[k..].iter().collect();
            if pattern.matches(&tail) {
                return chars[..k].iter().collect();
            }
        }
    }
    value.to_string()
}

/// `${x/pat/rep}`: longest match starting at each position, left to right.
fn substitute(value: &str, pattern: &glob::Pattern, rep: &str, all: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0;
    let mut replaced = false;
    while i < n {
        if !replaced || all {
            let mut matched_end = None;
            for j in (i..=n).rev() {
                let cand: String = chars[i..j].iter().collect();
                if pattern.matches(&cand) {
                    matched_end = Some(j);
                    break;
                }
            }
            if let Some(j) = matched_end {
                if j > i {
                    out.push_str(rep);
                    i = j;
                    replaced = true;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Split `off:len` at the first top-level colon (a `:-` inside an arithmetic
/// word would be rare enough not to guard).
fn split_top_colon(s: &str) -> Option<(&str, &str)> {
    s.split_once(':')
}
