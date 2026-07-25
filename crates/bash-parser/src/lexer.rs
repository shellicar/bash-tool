//! Word/operator tokenizer. Quote- and bracket-aware: `$(...)`, `` `...` ``,
//! `${...}`, `((...))`, and single/double-quoted spans are captured as one
//! opaque token each — this IS `parse_matched_pair()`'s job
//! (docs/ast-execution.md), just not yet split into its own reusable scanner
//! module. Deliberately does NOT expand or interpret what's inside those
//! spans; the resulting `Word.text` still contains the literal bracket
//! characters, exactly like bash's own `WORD` token.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(String, bool), // (text, was-quoted-anywhere)
    /// A digit run immediately followed by `<`/`>` with no whitespace — an
    /// fd-prefixed redirect (`2>&1`, `1>&2`). Only emitted in that exact
    /// adjacency; `123 foo` or `123abc` is an ordinary `Word` (found live via
    /// the AST printer: `2>&1` was silently splitting into a bogus `"2"`
    /// argument plus an fd-less `>&1` redirect before this existed).
    Fd(u32),
    And,                // &&
    Or,                 // ||
    Pipe,               // |
    Semi,               // ;
    Amp,                // &
    Great,              // >
    DGreat,             // >>
    Less,               // <
    DLess,              // <<
    DLessDash,          // <<-
    DLessLess,          // <<<
    GreatAmp,           // >&  or N>&M forms folded in as plain words upstream for now
    LParen,
    RParen,
    LBrace,
    RBrace,
    Newline,
    Eof,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
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
        Self { src: src.as_bytes(), pos: 0 }
    }

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
        let start = self.pos;
        let mut out = Vec::new();
        out.push(self.bump().unwrap());
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedQuote(start)),
                Some(b'\\') if quote == b'"' => {
                    out.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
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
        matches!(
            c,
            b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' | b'{' | b'}'
        )
    }

    fn scan_word(&mut self) -> Result<(String, bool), LexError> {
        let mut text = String::new();
        let mut quoted = false;
        loop {
            match self.peek() {
                Some(b'\'') => {
                    quoted = true;
                    text.push_str(&self.scan_quoted(b'\'')?);
                }
                Some(b'"') => {
                    quoted = true;
                    text.push_str(&self.scan_quoted(b'"')?);
                }
                Some(b'`') => {
                    text.push_str(&self.scan_matched(b'`', b'`', "backtick substitution")?);
                }
                Some(b'$') if self.peek_at(1) == Some(b'(') && self.peek_at(2) == Some(b'(') => {
                    self.pos += 2; // consume "$("
                    text.push('$');
                    text.push_str(&self.scan_matched(b'(', b')', "arithmetic expansion $((...))")?);
                    // closes only the inner `)`; consume the matching outer one too
                    if self.peek() == Some(b')') {
                        text.push(self.bump().unwrap() as char);
                    }
                }
                Some(b'$') if self.peek_at(1) == Some(b'(') => {
                    self.pos += 1; // consume "$"
                    text.push('$');
                    text.push_str(&self.scan_matched(b'(', b')', "command substitution $(...)")?);
                }
                Some(b'$') if self.peek_at(1) == Some(b'{') => {
                    self.pos += 1;
                    text.push('$');
                    text.push_str(&self.scan_matched(b'{', b'}', "parameter expansion ${...}")?);
                }
                Some(b'\\') => {
                    text.push(self.bump().unwrap() as char);
                    if let Some(c) = self.bump() {
                        text.push(c as char);
                    }
                }
                Some(c) if !self.is_word_boundary(c) => {
                    text.push(self.bump().unwrap() as char);
                }
                _ => break,
            }
        }
        Ok((text, quoted))
    }

    /// Capture a heredoc body: everything from just after the current line's
    /// end, up to (and consuming) a line that is exactly `delimiter` (after
    /// stripping leading tabs, if `strip_tabs`). This is the raw-text mode
    /// bash itself switches into once it sees `<<`/`<<-` and its delimiter —
    /// the body is NEVER tokenized as bash syntax, which is exactly the gap
    /// that broke every heredoc-containing command before this existed
    /// (source with `(`/`{` inside the body was read as real bash tokens).
    ///
    /// Simplification, scoped to the corpus's actual dominant pattern (a
    /// heredoc redirect is the last thing on its command line): starts
    /// scanning from the next newline in the source, discarding whatever
    /// (rare) tokens might follow the delimiter word on the same line. Real
    /// bash defers the whole line's tokenization until the newline; getting
    /// that fully right needs a token queue, not yet built.
    pub fn capture_heredoc(&mut self, delimiter: &str, strip_tabs: bool) -> String {
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        }
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
            let stored = if strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
            body.push_str(stored);
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
            None => Ok(Token::Eof),
            Some(b'\n') => {
                self.pos += 1;
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
            Some(b';') => {
                self.pos += 1;
                Ok(Token::Semi)
            }
            Some(b'(') => {
                self.pos += 1;
                Ok(Token::LParen)
            }
            Some(b')') => {
                self.pos += 1;
                Ok(Token::RParen)
            }
            Some(b'{') => {
                self.pos += 1;
                Ok(Token::LBrace)
            }
            Some(b'}') => {
                self.pos += 1;
                Ok(Token::RBrace)
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
            Some(b'>') => {
                self.pos += 1;
                Ok(Token::Great)
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.pos;
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    self.pos += 1;
                }
                if matches!(self.peek(), Some(b'<') | Some(b'>')) {
                    let digits = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
                    Ok(Token::Fd(digits.parse().unwrap()))
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
