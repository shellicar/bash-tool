//! Word/operator tokenizer. Quote- and bracket-aware: `$(...)`, `` `...` ``,
//! `${...}`, `((...))`, `<(...)`/`>(...)`, and single/double-quoted spans are
//! captured as one opaque token each — this IS `parse_matched_pair()`'s job
//! (docs/ast-execution.md), just not yet split into its own reusable scanner
//! module. Deliberately does NOT expand or interpret what's inside those
//! spans; the resulting `Word.text` still contains the literal bracket
//! characters, exactly like bash's own `WORD` token.
//!
//! `{`/`}` are NOT operator tokens. Bash treats them as reserved words —
//! special only when standing alone in command position (`{ cmds; }`) — so
//! `find -exec rm {} \;`'s `{}` must stay an ordinary word (a real corpus
//! failure before this). The parser recognizes the standalone words instead.

use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(String, bool), // (text, was-quoted-anywhere)
    /// A digit run immediately followed by `<`/`>` with no whitespace — an
    /// fd-prefixed redirect (`2>&1`, `1>&2`). Only emitted in that exact
    /// adjacency; `123 foo` or `123abc` is an ordinary `Word` (found live via
    /// the AST printer: `2>&1` was silently splitting into a bogus `"2"`
    /// argument plus an fd-less `>&1` redirect before this existed).
    Fd(u32),
    /// A `((...))` arithmetic-command span, delimiters included, interior
    /// opaque — the same deferred treatment as `$((...))`. Only emitted when
    /// the balanced span really ends in `))`; `((echo a); echo b)` falls back
    /// to nested subshells, mirroring how bash itself disambiguates.
    Arith(String),
    And,      // &&
    Or,       // ||
    Pipe,     // |
    Semi,     // ;
    DSemi,    // ;;   (case arm terminator)
    SemiAmp,  // ;&   (case fallthrough)
    DSemiAmp, // ;;&  (case test-next)
    Amp,      // &
    Great,     // >
    DGreat,    // >>
    Less,      // <
    DLess,     // <<
    DLessDash, // <<-
    DLessLess, // <<<
    GreatAmp,  // >&
    LessAmp,   // <&
    AmpGreat,  // &>
    AmpDGreat, // &>>
    LParen,
    RParen,
    Newline,
    Eof,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    /// Heredocs seen on the current line, awaiting their bodies. Bash defers
    /// body capture until the newline that ends the line the `<<` appeared
    /// on — tokens after the delimiter word (`cat <<EOF | grep x`) belong to
    /// the command, not the body. The parser registers each delimiter here;
    /// `next_token` captures all pending bodies, in order, when it consumes
    /// that newline (or hits EOF).
    pending_heredocs: Vec<(String, bool)>,
    bodies: VecDeque<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LexError {
    #[error("unterminated quote starting at byte {0}")]
    UnterminatedQuote(usize),
    #[error("unterminated {0} starting at byte {1}")]
    UnterminatedBracket(&'static str, usize),
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self { src: src.as_bytes(), pos: 0, pending_heredocs: Vec::new(), bodies: VecDeque::new() }
    }

    pub fn register_heredoc(&mut self, delimiter: String, strip_tabs: bool) {
        self.pending_heredocs.push((delimiter, strip_tabs));
    }

    /// Captured heredoc bodies, in source order. The parser matches them
    /// back to `Redirect` nodes after the parse (an in-order AST walk visits
    /// heredoc redirects in source order).
    pub fn take_bodies(&mut self) -> VecDeque<String> {
        std::mem::take(&mut self.bodies)
    }

}

/// `((` opens arithmetic only if what sits between the outer parens could be
/// an expression. `((echo a) && (echo b))` also ends in `))`, but its interior
/// closes a paren before it opens one, which no expression does. Bash resolves
/// the same ambiguity by parsing the interior and rewinding when that fails.
fn is_arith_interior(text: &str) -> bool {
    let inner = &text[2..text.len() - 2];
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.pos += 1;
        }
        // backslash-newline is a line continuation, not a token boundary
        while self.peek() == Some(b'\\') && self.peek_at(1) == Some(b'\n') {
            self.pos += 2;
            while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
                self.pos += 1;
            }
        }
    }

    /// Consume a bracket-matched span starting at the current `open` byte,
    /// returning the full span INCLUDING the delimiters, as opaque text.
    /// Handles nesting for `(`/`)` and `{`/`}` pairs; `` ` `` and quotes are
    /// their own delimiter (open == close).
    fn scan_matched(&mut self, open: u8, close: u8, name: &'static str) -> Result<String, LexError> {
        let start = self.pos;
        let mut depth = 0i32;
        let mut out = Vec::new();
        out.push(self.bump().unwrap()); // consume opening delimiter
        if open != close {
            depth = 1;
        }
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedBracket(name, start)),
                Some(b'\\') => {
                    out.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                // Quoted spans inside the bracket: a `)` inside `"..."` must
                // not close `$(...)` — bash's parse_matched_pair tracks quote
                // state (`qc`) for exactly this. Found live: a corpus
                // `$(git log -S "...)...")` span ended early at the quoted `)`.
                Some(c @ (b'\'' | b'"')) if c != close => {
                    let quoted = self.scan_quoted(c)?;
                    out.extend_from_slice(quoted.as_bytes());
                }
                // `$'...'` obeys its own escaping, so a `\'` inside it is a
                // literal quote and does not end the span. Without this the
                // scan closes early and the bracket never matches.
                Some(b'$') if matches!(self.peek_at(1), Some(b'\'')) => {
                    out.push(self.bump().unwrap());
                    let ansi = self.scan_quoted_span(b'\'', true)?;
                    out.extend_from_slice(ansi.as_bytes());
                }
                Some(c) if open != close && c == open => {
                    depth += 1;
                    out.push(self.bump().unwrap());
                }
                Some(c) if c == close => {
                    out.push(self.bump().unwrap());
                    if open == close || { depth -= 1; depth == 0 } {
                        return Ok(String::from_utf8_lossy(&out).into_owned());
                    }
                }
                Some(_) => out.push(self.bump().unwrap()),
            }
        }
    }

    fn scan_quoted(&mut self, quote: u8) -> Result<String, LexError> {
        self.scan_quoted_span(quote, false)
    }

    /// `ansi_c` marks the `$'...'` form, where a backslash escapes the next
    /// character. That matters for `\'`, which does NOT close the span: bash
    /// runs `echo $'quote\''` and prints `quote'`. Plain `'...'` has no
    /// escapes at all, which is why the two cannot share one rule.
    fn scan_quoted_span(&mut self, quote: u8, ansi_c: bool) -> Result<String, LexError> {
        let start = self.pos;
        let mut out = Vec::new();
        out.push(self.bump().unwrap());
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedQuote(start)),
                Some(b'\\') if ansi_c => {
                    out.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                Some(b'\\') if quote == b'"' => {
                    out.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                // Substitutions stay ACTIVE inside double quotes — a `"`
                // inside `"$(date "+%Y")"`'s inner span must not close the
                // outer quote, and an unterminated `${`/backtick inside
                // double quotes is a syntax error in bash, not literal text.
                // Mutual recursion with scan_matched gives the full nesting.
                Some(b'$') if quote == b'"' && self.peek_at(1) == Some(b'(') => {
                    self.pos += 1;
                    out.push(b'$');
                    let span = self.scan_matched(b'(', b')', "command substitution $(...)")?;
                    out.extend_from_slice(span.as_bytes());
                }
                Some(b'$') if quote == b'"' && self.peek_at(1) == Some(b'{') => {
                    self.pos += 1;
                    out.push(b'$');
                    let span = self.scan_matched(b'{', b'}', "parameter expansion ${...}")?;
                    out.extend_from_slice(span.as_bytes());
                }
                Some(b'`') if quote == b'"' => {
                    let span = self.scan_matched(b'`', b'`', "backtick substitution")?;
                    out.extend_from_slice(span.as_bytes());
                }
                Some(c) if c == quote => {
                    out.push(self.bump().unwrap());
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                Some(_) => out.push(self.bump().unwrap()),
            }
        }
    }

    fn is_word_boundary(&self, c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')')
    }

    // Accumulates BYTES, decoding once at the end: pushing `byte as char`
    // re-encodes each UTF-8 continuation byte as its own Latin-1 character,
    // silently mangling any non-ASCII word (✓, → — common in echo text).
    fn scan_word(&mut self) -> Result<(String, bool), LexError> {
        let mut text = Vec::<u8>::new();
        let mut quoted = false;
        loop {
            match self.peek() {
                Some(b'\'') => {
                    quoted = true;
                    text.extend_from_slice(self.scan_quoted(b'\'')?.as_bytes());
                }
                Some(b'"') => {
                    quoted = true;
                    text.extend_from_slice(self.scan_quoted(b'"')?.as_bytes());
                }
                // `$'...'`: ANSI-C quoting, where backslash escapes and `\'`
                // is a literal quote rather than the terminator.
                Some(b'$') if self.peek_at(1) == Some(b'\'') => {
                    quoted = true;
                    self.pos += 1;
                    text.push(b'$');
                    text.extend_from_slice(self.scan_quoted_span(b'\'', true)?.as_bytes());
                }
                Some(b'`') => {
                    text.extend_from_slice(
                        self.scan_matched(b'`', b'`', "backtick substitution")?.as_bytes(),
                    );
                }
                // `$((...))` needs no special case: the depth counter takes
                // the whole balanced span, and arithmetic-vs-command-
                // substitution is the EXPANDER's decision (bash's own
                // tie-break), not the lexer's. A previous special arm here
                // dropped the first `(` and silently corrupted `$((x+1))`
                // into `$(x+1))` — found by a walker end-to-end test.
                Some(b'$') if self.peek_at(1) == Some(b'(') => {
                    self.pos += 1; // consume "$"
                    text.push(b'$');
                    text.extend_from_slice(
                        self.scan_matched(b'(', b')', "command substitution $(...)")?.as_bytes(),
                    );
                }
                Some(b'$') if self.peek_at(1) == Some(b'{') => {
                    self.pos += 1;
                    text.push(b'$');
                    text.extend_from_slice(
                        self.scan_matched(b'{', b'}', "parameter expansion ${...}")?.as_bytes(),
                    );
                }
                // Process substitution is a word-level construct (it expands
                // to a filename), not a redirect: `diff <(sort a) <(sort b)`.
                Some(c @ (b'<' | b'>')) if self.peek_at(1) == Some(b'(') => {
                    self.pos += 1;
                    text.push(c);
                    text.extend_from_slice(
                        self.scan_matched(b'(', b')', "process substitution")?.as_bytes(),
                    );
                }
                // Array assignment: `x=(a b)` is one word; `(` is otherwise
                // a boundary. Only after a literal `=` so ordinary words
                // never swallow a subshell.
                Some(b'(') if text.last() == Some(&b'=') => {
                    text.extend_from_slice(
                        self.scan_matched(b'(', b')', "array assignment")?.as_bytes(),
                    );
                }
                Some(b'\\') => {
                    text.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        text.push(c);
                    }
                }
                Some(c) if !self.is_word_boundary(c) => {
                    text.push(self.bump().unwrap());
                }
                _ => break,
            }
        }
        Ok((String::from_utf8_lossy(&text).into_owned(), quoted))
    }

    /// The whitespace-separated chunks between `[[` and its closing `]]`,
    /// quote- and `$()`-aware, `]]` consumed. Bash parses `[[ ]]` with its
    /// own hand-rolled scanner outside the bison grammar (parse.y:5031-5249);
    /// this is the equivalent seam. Whitespace-only splitting means
    /// parenthesized groups need spaces (`[[ ( a == b ) ]]`), and a regex
    /// operand like `^(a|b)$` survives as one chunk — which is exactly why
    /// the main tokenizer can't be reused here (`|` and `(` would become
    /// operators inside the regex).
    pub fn cond_chunks(&mut self) -> Result<Vec<String>, LexError> {
        let start = self.pos;
        let mut chunks = Vec::new();
        loop {
            loop {
                match self.peek() {
                    Some(b' ') | Some(b'\t') | Some(b'\n') => self.pos += 1,
                    Some(b'\\') if self.peek_at(1) == Some(b'\n') => self.pos += 2,
                    _ => break,
                }
            }
            if self.peek().is_none() {
                return Err(LexError::UnterminatedBracket("[[ ]]", start));
            }
            // Closing `]]` — recognized before word-scanning so an operator
            // right after it (`]]; then`, `]]&&`) stays with the main
            // tokenizer instead of gluing onto the chunk.
            if self.peek() == Some(b']')
                && self.peek_at(1) == Some(b']')
                && !matches!(self.peek_at(2), Some(c) if !self.is_word_boundary(c))
            {
                self.pos += 2;
                return Ok(chunks);
            }
            let mut chunk = Vec::<u8>::new();
            loop {
                match self.peek() {
                    None | Some(b' ') | Some(b'\t') | Some(b'\n') => break,
                    Some(b'\'') => chunk.extend_from_slice(self.scan_quoted(b'\'')?.as_bytes()),
                    Some(b'"') => chunk.extend_from_slice(self.scan_quoted(b'"')?.as_bytes()),
                    Some(b'`') => chunk.extend_from_slice(
                        self.scan_matched(b'`', b'`', "backtick substitution")?.as_bytes(),
                    ),
                    // `$'...'` before the bare-quote arms, so its own escaping
                    // applies and `\'` does not close the chunk early.
                    Some(b'$') if self.peek_at(1) == Some(b'\'') => {
                        self.pos += 1;
                        chunk.push(b'$');
                        chunk.extend_from_slice(self.scan_quoted_span(b'\'', true)?.as_bytes());
                    }
                    Some(b'$') if self.peek_at(1) == Some(b'(') => {
                        self.pos += 1;
                        chunk.push(b'$');
                        chunk.extend_from_slice(
                            self.scan_matched(b'(', b')', "command substitution $(...)")?.as_bytes(),
                        );
                    }
                    Some(b'$') if self.peek_at(1) == Some(b'{') => {
                        self.pos += 1;
                        chunk.push(b'$');
                        chunk.extend_from_slice(
                            self.scan_matched(b'{', b'}', "parameter expansion ${...}")?.as_bytes(),
                        );
                    }
                    Some(b'\\') => {
                        chunk.push(self.bump().unwrap());
                        if let Some(c) = self.bump() {
                            chunk.push(c);
                        }
                    }
                    Some(_) => chunk.push(self.bump().unwrap()),
                }
            }
            chunks.push(String::from_utf8_lossy(&chunk).into_owned());
        }
    }

    fn capture_pending_heredocs(&mut self) {
        let pending = std::mem::take(&mut self.pending_heredocs);
        for (delimiter, strip_tabs) in pending {
            let body = self.capture_heredoc_body(&delimiter, strip_tabs);
            self.bodies.push_back(body);
        }
    }

    /// Capture one heredoc body: raw lines from the current position up to
    /// (and consuming) a line that is exactly `delimiter` (after stripping
    /// leading tabs, if `strip_tabs`). This is the raw-text mode bash itself
    /// switches into after the line's newline — the body is NEVER tokenized
    /// as bash syntax, which is exactly the gap that broke every
    /// heredoc-containing command before this existed (source with `(`/`{`
    /// inside the body was read as real bash tokens).
    fn capture_heredoc_body(&mut self, delimiter: &str, strip_tabs: bool) -> String {
        let mut body = String::new();
        loop {
            let line_start = self.pos;
            while !matches!(self.peek(), None | Some(b'\n')) {
                self.pos += 1;
            }
            let line = String::from_utf8_lossy(&self.src[line_start..self.pos]).into_owned();
            let had_newline = self.peek() == Some(b'\n');
            if had_newline {
                self.pos += 1;
            }
            let compare = if strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
            if compare == delimiter {
                break;
            }
            body.push_str(compare);
            body.push('\n');
            if !had_newline {
                break; // EOF with no closing delimiter found — best-effort, not an error here
            }
        }
        body
    }

    pub fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_blanks();
        match self.peek() {
            None => {
                if !self.pending_heredocs.is_empty() {
                    self.capture_pending_heredocs();
                }
                Ok(Token::Eof)
            }
            Some(b'\n') => {
                self.pos += 1;
                if !self.pending_heredocs.is_empty() {
                    self.capture_pending_heredocs();
                }
                Ok(Token::Newline)
            }
            Some(b'#') => {
                // comment: consume to end of line, then recurse for the real token
                while !matches!(self.peek(), None | Some(b'\n')) {
                    self.pos += 1;
                }
                self.next_token()
            }
            Some(b'&') if self.peek_at(1) == Some(b'&') => {
                self.pos += 2;
                Ok(Token::And)
            }
            Some(b'&') if self.peek_at(1) == Some(b'>') && self.peek_at(2) == Some(b'>') => {
                self.pos += 3;
                Ok(Token::AmpDGreat)
            }
            Some(b'&') if self.peek_at(1) == Some(b'>') => {
                self.pos += 2;
                Ok(Token::AmpGreat)
            }
            Some(b'&') => {
                self.pos += 1;
                Ok(Token::Amp)
            }
            Some(b'|') if self.peek_at(1) == Some(b'|') => {
                self.pos += 2;
                Ok(Token::Or)
            }
            Some(b'|') => {
                self.pos += 1;
                Ok(Token::Pipe)
            }
            Some(b';') if self.peek_at(1) == Some(b';') && self.peek_at(2) == Some(b'&') => {
                self.pos += 3;
                Ok(Token::DSemiAmp)
            }
            Some(b';') if self.peek_at(1) == Some(b';') => {
                self.pos += 2;
                Ok(Token::DSemi)
            }
            Some(b';') if self.peek_at(1) == Some(b'&') => {
                self.pos += 2;
                Ok(Token::SemiAmp)
            }
            Some(b';') => {
                self.pos += 1;
                Ok(Token::Semi)
            }
            Some(b'(') if self.peek_at(1) == Some(b'(') => {
                // `((...))`: arithmetic command IF the balanced span ends in
                // `))` — otherwise it was a subshell whose first command is
                // itself parenthesized (`((echo a); echo b)`), so rewind and
                // emit a plain `(`. Mirrors bash's own lookahead-to-`))`.
                let start = self.pos;
                match self.scan_matched(b'(', b')', "arithmetic command ((...))") {
                    Ok(text) if text.ends_with("))") && is_arith_interior(&text) => {
                        Ok(Token::Arith(text))
                    }
                    _ => {
                        self.pos = start + 1;
                        Ok(Token::LParen)
                    }
                }
            }
            Some(b'(') => {
                self.pos += 1;
                Ok(Token::LParen)
            }
            Some(b')') => {
                self.pos += 1;
                Ok(Token::RParen)
            }
            Some(b'<') if self.peek_at(1) == Some(b'<') && self.peek_at(2) == Some(b'<') => {
                self.pos += 3;
                Ok(Token::DLessLess)
            }
            Some(b'<') if self.peek_at(1) == Some(b'<') && self.peek_at(2) == Some(b'-') => {
                self.pos += 3;
                Ok(Token::DLessDash)
            }
            Some(b'<') if self.peek_at(1) == Some(b'<') => {
                self.pos += 2;
                Ok(Token::DLess)
            }
            Some(b'<') if self.peek_at(1) == Some(b'&') => {
                self.pos += 2;
                Ok(Token::LessAmp)
            }
            Some(b'<') if self.peek_at(1) == Some(b'(') => {
                let (text, quoted) = self.scan_word()?;
                Ok(Token::Word(text, quoted))
            }
            Some(b'<') => {
                self.pos += 1;
                Ok(Token::Less)
            }
            Some(b'>') if self.peek_at(1) == Some(b'>') => {
                self.pos += 2;
                Ok(Token::DGreat)
            }
            Some(b'>') if self.peek_at(1) == Some(b'&') => {
                self.pos += 2;
                Ok(Token::GreatAmp)
            }
            Some(b'>') if self.peek_at(1) == Some(b'(') => {
                let (text, quoted) = self.scan_word()?;
                Ok(Token::Word(text, quoted))
            }
            Some(b'>') => {
                self.pos += 1;
                Ok(Token::Great)
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.pos;
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    self.pos += 1;
                }
                // Bash lexes a digit run as an fd only while it fits a signed
                // 32-bit int; above INT_MAX the digits are an ordinary word.
                // Verified against 5.3: `echo a 2147483647>f` redirects, while
                // `echo a 2147483648>f` writes the digits as an argument.
                let fd = std::str::from_utf8(&self.src[start..self.pos])
                    .unwrap()
                    .parse::<i32>();
                if let (true, Ok(fd)) = (matches!(self.peek(), Some(b'<') | Some(b'>')), fd) {
                    Ok(Token::Fd(fd as u32))
                } else {
                    self.pos = start; // not an fd prefix — rewind, tokenize as an ordinary word
                    let (text, quoted) = self.scan_word()?;
                    Ok(Token::Word(text, quoted))
                }
            }
            Some(_) => {
                let (text, quoted) = self.scan_word()?;
                Ok(Token::Word(text, quoted))
            }
        }
    }
}
