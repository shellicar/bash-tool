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
    /// `|&` — bash's shorthand for `2>&1 |`. Kept as its own token because
    /// the redirect it stands for lands on the LEFT stage, after whatever
    /// redirects that stage already carries (`echo a 2>/dev/null |& cat`
    /// prints back as `echo a 2> /dev/null 2>&1 | cat`).
    PipeAmp,
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
    GreatPipe, // >|
    LessAmp,   // <&
    AmpGreat,  // &>
    AmpDGreat, // &>>
    LParen,
    RParen,
    Newline,
    Eof,
}

pub struct Lexer<'a> {
    text: &'a str,
    src: &'a [u8],
    pos: usize,
    /// Where the token `next_token` last returned began, comments and blanks
    /// already skipped. The command-substitution scanner needs it: it parses
    /// the interior and then has to know where the `)` that stopped the parse
    /// actually sits in the source.
    token_start: usize,
    in_command_substitution: bool,
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
    #[error("syntax error inside the command substitution at byte {0}: {1}")]
    CommandSubstitution(usize, String),
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            text: src,
            src: src.as_bytes(),
            pos: 0,
            token_start: 0,
            in_command_substitution: false,
            pending_heredocs: Vec::new(),
            bodies: VecDeque::new(),
        }
    }

    pub fn token_start(&self) -> usize {
        self.token_start
    }

    /// Mark this lexer as reading the interior of a `$(...)`. It changes one
    /// thing: how a heredoc body ends. See `capture_heredoc_body`.
    pub fn set_in_command_substitution(&mut self) {
        self.in_command_substitution = true;
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
        Ok(self.scan_matched_parts(open, close, name)?.0)
    }

    /// Returns the span and its STRUCTURE: the same text with every quoted or
    /// substituted region dropped. Only the structure can answer the
    /// arithmetic tie-break, because `(( $(case x in x) esac);; ))` holds a
    /// `)` that closes a case pattern and belongs to nobody out here.
    fn scan_matched_parts(
        &mut self,
        open: u8,
        close: u8,
        name: &'static str,
    ) -> Result<(String, String), LexError> {
        let start = self.pos;
        let mut depth = 0i32;
        let mut out = Vec::new();
        let mut structure = Vec::new();
        out.push(self.bump().unwrap()); // consume opening delimiter
        structure.push(open);
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
                // A nested substitution is a span of its own, never more
                // depth for this scan: the `)` in `(( $(case x in x) esac) ))`
                // closes a case pattern and is invisible out here.
                Some(b'$') if self.peek_at(1) == Some(b'(') => {
                    self.pos += 1;
                    out.push(b'$');
                    out.extend_from_slice(self.scan_dollar_paren()?.as_bytes());
                }
                Some(b'$') if self.peek_at(1) == Some(b'{') => {
                    self.pos += 1;
                    out.push(b'$');
                    out.extend_from_slice(self.scan_param_expansion()?.as_bytes());
                }
                Some(b'`') => out.extend_from_slice(self.scan_backticks()?.as_bytes()),
                Some(c) if open != close && c == open => {
                    depth += 1;
                    out.push(self.bump().unwrap());
                    structure.push(c);
                }
                Some(c) if c == close => {
                    out.push(self.bump().unwrap());
                    structure.push(c);
                    if open == close || { depth -= 1; depth == 0 } {
                        return Ok((
                            String::from_utf8_lossy(&out).into_owned(),
                            String::from_utf8_lossy(&structure).into_owned(),
                        ));
                    }
                }
                Some(_) => {
                    let c = self.bump().unwrap();
                    out.push(c);
                    structure.push(c);
                }
            }
        }
    }

    /// `$(`: arithmetic if the balanced span really is `$((expr))`, otherwise
    /// a command substitution. `$((echo a); echo b)` ends in `b)`, so it falls
    /// through to the command-substitution scanner, which is the same
    /// tie-break bash makes for the bare `((` token.
    fn scan_dollar_paren(&mut self) -> Result<String, LexError> {
        if self.peek_at(1) == Some(b'(') {
            let start = self.pos;
            if let Ok((span, structure)) =
                self.scan_matched_parts(b'(', b')', "arithmetic expansion $((...))")
            {
                if span.ends_with("))") && is_arith_interior(&structure) {
                    return Ok(span);
                }
            }
            self.pos = start;
        }
        self.scan_command_substitution()
    }

    /// `$(...)`, `<(...)`, `>(...)`. Counting brackets cannot find the closing
    /// `)`, because a `)` also ends a case pattern, sits inside a comment, and
    /// sits inside a heredoc body. Bash's own scanner PARSES the interior and
    /// takes the `)` the parse stops at, so this does too: `$(case a in a)
    /// echo z; esac)` closes at the last paren, not the pattern's.
    ///
    /// The interior parse is thrown away — the token stays opaque text, and
    /// the walker re-parses it when the substitution is actually evaluated.
    fn scan_command_substitution(&mut self) -> Result<String, LexError> {
        let open = self.pos; // at `(`
        let inner = open + 1;
        let end = crate::parser::scan_until_unmatched_rparen(&self.text[inner..])
            .map_err(|e| LexError::CommandSubstitution(open, e.to_string()))?;
        self.pos = inner + end + 1; // past the `)`
        Ok(self.text[open..self.pos].to_string())
    }

    /// `${...}` ends at the FIRST unescaped `}`: a bare `{` inside does not
    /// nest, so `${IFS+d{}}` is the word `${IFS+d{}` followed by a literal
    /// `}`. Nested `${`, `$(`, backticks and quoted spans are recursed into,
    /// which is what keeps `${x:-$(echo })}` and `${x-'}'}` whole.
    fn scan_param_expansion(&mut self) -> Result<String, LexError> {
        let start = self.pos;
        let mut out = Vec::new();
        out.push(self.bump().unwrap()); // `{`
        loop {
            match self.peek() {
                None => {
                    return Err(LexError::UnterminatedBracket(
                        "parameter expansion ${...}",
                        start,
                    ))
                }
                Some(b'}') => {
                    out.push(self.bump().unwrap());
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                Some(b'\\') => {
                    out.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                Some(c @ (b'\'' | b'"')) => {
                    out.extend_from_slice(self.scan_quoted(c)?.as_bytes());
                }
                Some(b'`') => out.extend_from_slice(self.scan_backticks()?.as_bytes()),
                Some(b'$') if self.peek_at(1) == Some(b'\'') => {
                    self.pos += 1;
                    out.push(b'$');
                    out.extend_from_slice(self.scan_quoted_span(b'\'', true)?.as_bytes());
                }
                Some(b'$') if self.peek_at(1) == Some(b'(') => {
                    self.pos += 1;
                    out.push(b'$');
                    out.extend_from_slice(self.scan_dollar_paren()?.as_bytes());
                }
                Some(b'$') if self.peek_at(1) == Some(b'{') => {
                    self.pos += 1;
                    out.push(b'$');
                    out.extend_from_slice(self.scan_param_expansion()?.as_bytes());
                }
                Some(_) => out.push(self.bump().unwrap()),
            }
        }
    }

    /// `` `...` `` tracks no quote state at all — the span ends at the first
    /// backtick a backslash did not escape. bash rejects `` `echo "a`b"` ``
    /// as an unterminated backtick rather than reading the inner one as
    /// quoted text.
    fn scan_backticks(&mut self) -> Result<String, LexError> {
        let start = self.pos;
        let mut out = Vec::new();
        out.push(self.bump().unwrap()); // opening backtick
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedBracket("backtick substitution", start)),
                Some(b'\\') => {
                    out.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        out.push(c);
                    }
                }
                Some(b'`') => {
                    out.push(self.bump().unwrap());
                    return Ok(String::from_utf8_lossy(&out).into_owned());
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
                // Line continuation. bash removes it before tokenizing, in
                // double quotes as well as out of them, but never inside
                // single quotes: `"a\<newline>b"` is `ab`, `'a\<newline>b'`
                // keeps both characters.
                Some(b'\\') if quote == b'"' && self.peek_at(1) == Some(b'\n') => self.pos += 2,
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
                    let span = self.scan_dollar_paren()?;
                    out.extend_from_slice(span.as_bytes());
                }
                Some(b'$') if quote == b'"' && self.peek_at(1) == Some(b'{') => {
                    self.pos += 1;
                    out.push(b'$');
                    let span = self.scan_param_expansion()?;
                    out.extend_from_slice(span.as_bytes());
                }
                Some(b'`') if quote == b'"' => {
                    let span = self.scan_backticks()?;
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
                    text.extend_from_slice(self.scan_backticks()?.as_bytes());
                }
                Some(b'$') if self.peek_at(1) == Some(b'(') => {
                    self.pos += 1; // consume "$"
                    text.push(b'$');
                    text.extend_from_slice(self.scan_dollar_paren()?.as_bytes());
                }
                Some(b'$') if self.peek_at(1) == Some(b'{') => {
                    self.pos += 1;
                    text.push(b'$');
                    text.extend_from_slice(self.scan_param_expansion()?.as_bytes());
                }
                // Process substitution is a word-level construct (it expands
                // to a filename), not a redirect: `diff <(sort a) <(sort b)`.
                Some(c @ (b'<' | b'>')) if self.peek_at(1) == Some(b'(') => {
                    self.pos += 1;
                    text.push(c);
                    text.extend_from_slice(self.scan_command_substitution()?.as_bytes());
                }
                // Array assignment: `x=(a b)` is one word; `(` is otherwise
                // a boundary. Only after a literal `=` so ordinary words
                // never swallow a subshell.
                Some(b'(') if text.last() == Some(&b'=') => {
                    text.extend_from_slice(
                        self.scan_matched(b'(', b')', "array assignment")?.as_bytes(),
                    );
                }
                Some(b'\\') if self.peek_at(1) == Some(b'\n') => self.pos += 2,
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
            // Inside `[[ ]]` bash reads `(` and `)` as tokens in their own
            // right, so `[[ (-n a) ]]` and `[[ (a && b) || (c) ]]` need no
            // spaces around them. Two things stay part of the word: an
            // extglob head (`@(a)b`, always live in `[[ ]]` whatever shopt
            // says), and the operand after `=~`, which bash reads as a regex
            // where parens are the regex's own.
            let regex = chunks.last().is_some_and(|c| c == "=~");
            let mut chunk = Vec::<u8>::new();
            loop {
                match self.peek() {
                    None | Some(b' ') | Some(b'\t') | Some(b'\n') => break,
                    Some(b'\'') => chunk.extend_from_slice(self.scan_quoted(b'\'')?.as_bytes()),
                    Some(b'"') => chunk.extend_from_slice(self.scan_quoted(b'"')?.as_bytes()),
                    Some(b'`') => chunk.extend_from_slice(self.scan_backticks()?.as_bytes()),
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
                        chunk.extend_from_slice(self.scan_dollar_paren()?.as_bytes());
                    }
                    Some(b'$') if self.peek_at(1) == Some(b'{') => {
                        self.pos += 1;
                        chunk.push(b'$');
                        chunk.extend_from_slice(self.scan_param_expansion()?.as_bytes());
                    }
                    // In regex position a paren group is part of the operand,
                    // and it may hold blanks: `[[ $v =~ (one two) ]]`.
                    Some(b'(') if regex => {
                        chunk.extend_from_slice(
                            self.scan_matched(b'(', b')', "regex group")?.as_bytes(),
                        );
                    }
                    Some(b'[') => match self.scan_subscript() {
                        Some(span) => chunk.extend_from_slice(span.as_bytes()),
                        None => chunk.push(self.bump().unwrap()),
                    },
                    Some(b'(') if !regex => {
                        if matches!(chunk.last(), Some(b'?' | b'*' | b'+' | b'@' | b'!')) {
                            chunk.extend_from_slice(
                                self.scan_matched(b'(', b')', "extglob pattern")?.as_bytes(),
                            );
                        } else {
                            if chunk.is_empty() {
                                chunk.push(self.bump().unwrap());
                            }
                            break;
                        }
                    }
                    Some(b')') if !regex => {
                        if chunk.is_empty() {
                            chunk.push(self.bump().unwrap());
                        }
                        break;
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

    /// A `[`...`]` span inside `[[ ]]` is one piece of a word, parens and all:
    /// `index[7<(4+2)]` is a single chunk. An unmatched `[` is ordinary text
    /// (`[[ a[ == b ]]` is legal in bash), so this looks before it consumes,
    /// and never crosses a blank.
    fn scan_subscript(&mut self) -> Option<String> {
        let mut i = self.pos;
        let mut depth = 0i32;
        while let Some(c) = self.src.get(i).copied() {
            match c {
                b' ' | b'\t' | b'\n' => return None,
                b'\\' => i += 1,
                // A quoted `]` closes nothing: `[[ ']' =~ [']'] ]]` is bash.
                b'\'' | b'"' => {
                    i += 1;
                    while self.src.get(i).copied() != Some(c) {
                        match self.src.get(i) {
                            None => return None,
                            Some(b'\\') if c == b'"' => i += 1,
                            Some(_) => {}
                        }
                        i += 1;
                    }
                }
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        let span = String::from_utf8_lossy(&self.src[self.pos..=i]).into_owned();
                        self.pos = i + 1;
                        return Some(span);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
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
            let line_end = self.pos;
            let line = String::from_utf8_lossy(&self.src[line_start..line_end]).into_owned();
            let had_newline = self.peek() == Some(b'\n');
            if had_newline {
                self.pos += 1;
            }
            let compare = if strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
            if compare == delimiter {
                break;
            }
            // Inside a command substitution the delimiter only has to BEGIN
            // the line: bash ends the body there and carries on parsing the
            // rest of it, which is what lets `$(cat <<EOF ... EOF)` close
            // both the heredoc and the substitution on one line. Outside one,
            // `EOFX` does not end an `EOF` heredoc.
            if self.in_command_substitution {
                if let Some(rest) = compare.strip_prefix(delimiter) {
                    self.pos = line_end - rest.len();
                    break;
                }
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
        self.token_start = self.pos;
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
            Some(b'|') if self.peek_at(1) == Some(b'&') => {
                self.pos += 2;
                Ok(Token::PipeAmp)
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
                match self.scan_matched_parts(b'(', b')', "arithmetic command ((...))") {
                    Ok((text, structure))
                        if text.ends_with("))") && is_arith_interior(&structure) =>
                    {
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
            Some(b'>') if self.peek_at(1) == Some(b'|') => {
                self.pos += 2;
                Ok(Token::GreatPipe)
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
