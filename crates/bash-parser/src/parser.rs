//! Recursive-descent parser over the core grammar (docs/ast-execution.md's
//! "scoped-subset" table): simple commands, `&&`/`||`/`;`/`|` connections,
//! redirects, subshells `( )`, and brace groups `{ ; }`. Compound keyword
//! commands (`for`/`if`/`while`/`until`/`case`/`function`/`[[ ]]`) are
//! recognized by keyword but return `Unsupported` for now — real, honest
//! scope for this first pass, not a silent gap: see the crate's README.

use crate::ast::*;
use crate::lexer::{LexError, Lexer, Token};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error("unexpected token {0:?}")]
    Unexpected(String),
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("'{0}' is not yet supported by this parser (see docs/ast-execution.md)")]
    Unsupported(&'static str),
}

const COMPOUND_KEYWORDS: &[&str] = &["for", "if", "while", "until", "case", "function", "[["];

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    lookahead: Option<Token>,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str) -> Self {
        Self { lexer: Lexer::new(src), lookahead: None }
    }

    fn peek(&mut self) -> Result<&Token, ParseError> {
        if self.lookahead.is_none() {
            self.lookahead = Some(self.lexer.next_token()?);
        }
        Ok(self.lookahead.as_ref().unwrap())
    }

    fn advance(&mut self) -> Result<Token, ParseError> {
        match self.lookahead.take() {
            Some(t) => Ok(t),
            None => Ok(self.lexer.next_token()?),
        }
    }

    fn skip_newlines(&mut self) -> Result<(), ParseError> {
        while matches!(self.peek()?, Token::Newline) {
            self.advance()?;
        }
        Ok(())
    }

    /// Parse a full program: a sequence of top-level commands (bash's
    /// `simple_list simple_list_terminator | ...`, parse.y:433 onward,
    /// scoped to the core grammar).
    pub fn parse_program(&mut self) -> Result<Command, ParseError> {
        self.parse_command_list(|t| matches!(t, Token::Eof))
    }

    /// A sequence of `&&`/`||`/`|`-commands joined by `;`/`&`/newline — the
    /// same grammar at the top level AND inside `( )`/`{ }` (bash's
    /// `compound_list`, parse.y). `is_end` tells the caller's terminator
    /// apart from a real next command (`Eof` at the top level, `RParen`/
    /// `RBrace` inside a subshell/group) — found missing by a real corpus
    /// failure: `( a; b; c )` and `( cmd & )` only parsed their first command
    /// before this was factored out of `parse_program` and reused here.
    fn parse_command_list(&mut self, is_end: impl Fn(&Token) -> bool) -> Result<Command, ParseError> {
        self.skip_newlines()?;
        let mut cmd = self.parse_and_or()?;
        loop {
            if is_end(self.peek()?) {
                break;
            }
            match self.peek()? {
                Token::Semi | Token::Amp => {
                    let connector = if matches!(self.peek()?, Token::Amp) {
                        Connector::SeqAsync
                    } else {
                        Connector::Seq
                    };
                    self.advance()?;
                    self.skip_newlines()?;
                    if is_end(self.peek()?) {
                        break;
                    }
                    let right = self.parse_and_or()?;
                    cmd = Command::Connection(Connection { left: Box::new(cmd), right: Box::new(right), connector });
                }
                Token::Newline => {
                    self.skip_newlines()?;
                    if is_end(self.peek()?) {
                        break;
                    }
                    let right = self.parse_and_or()?;
                    cmd = Command::Connection(Connection {
                        left: Box::new(cmd),
                        right: Box::new(right),
                        connector: Connector::Seq,
                    });
                }
                other => return Err(ParseError::Unexpected(format!("{other:?}"))),
            }
        }
        Ok(cmd)
    }

    /// `&&` / `||` — left-associative, both bind looser than `|`.
    fn parse_and_or(&mut self) -> Result<Command, ParseError> {
        let mut left = self.parse_pipeline()?;
        loop {
            let connector = match self.peek()? {
                Token::And => Connector::And,
                Token::Or => Connector::Or,
                _ => break,
            };
            self.advance()?;
            self.skip_newlines()?;
            let right = self.parse_pipeline()?;
            left = Command::Connection(Connection { left: Box::new(left), right: Box::new(right), connector });
        }
        Ok(left)
    }

    /// `|` — binds tighter than `&&`/`||`, matching bash's own precedence.
    fn parse_pipeline(&mut self) -> Result<Command, ParseError> {
        let mut left = self.parse_command()?;
        while matches!(self.peek()?, Token::Pipe) {
            self.advance()?;
            self.skip_newlines()?;
            let right = self.parse_command()?;
            left = Command::Connection(Connection { left: Box::new(left), right: Box::new(right), connector: Connector::Pipe });
        }
        Ok(left)
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        match self.peek()? {
            Token::LParen => {
                self.advance()?;
                let inner = self.parse_command_list(|t| matches!(t, Token::RParen))?;
                self.skip_newlines()?;
                self.expect(Token::RParen)?;
                Ok(Command::Subshell(Box::new(inner)))
            }
            Token::LBrace => {
                self.advance()?;
                let inner = self.parse_command_list(|t| matches!(t, Token::RBrace))?;
                self.skip_newlines()?;
                self.expect(Token::RBrace)?;
                Ok(Command::Group(Box::new(inner)))
            }
            Token::Word(w, _) if COMPOUND_KEYWORDS.contains(&w.as_str()) => {
                // Leak-free: keyword recognized, but the sub-grammar to parse
                // its body isn't built yet. See crate README for scope.
                Err(ParseError::Unsupported(match w.as_str() {
                    "for" => "for",
                    "if" => "if/elif/else",
                    "while" => "while",
                    "until" => "until",
                    "case" => "case",
                    "function" => "function definitions",
                    "[[" => "[[ ]] conditionals",
                    _ => unreachable!(),
                }))
            }
            _ => self.parse_simple_command().map(Command::Simple),
        }
    }

    fn expect(&mut self, expected: Token) -> Result<(), ParseError> {
        let got = self.advance()?;
        if got == expected {
            Ok(())
        } else {
            Err(ParseError::Unexpected(format!("expected {expected:?}, got {got:?}")))
        }
    }

    fn parse_simple_command(&mut self) -> Result<SimpleCommand, ParseError> {
        let mut assignments = Vec::new();
        let mut program = None;
        let mut args = Vec::new();
        let mut redirects = Vec::new();

        loop {
            match self.peek()? {
                Token::Word(w, quoted) => {
                    let w = w.clone();
                    let quoted = *quoted;
                    if program.is_none() {
                        if let Some((k, v)) = split_assignment(&w) {
                            self.advance()?;
                            assignments.push((k, Word { text: v, quoted }));
                            continue;
                        }
                    }
                    self.advance()?;
                    let word = Word { text: w, quoted };
                    if program.is_none() {
                        program = Some(word);
                    } else {
                        args.push(word);
                    }
                }
                Token::Great | Token::DGreat | Token::Less | Token::DLess | Token::DLessDash
                | Token::DLessLess | Token::GreatAmp => {
                    redirects.push(self.parse_redirect(None)?);
                }
                Token::Fd(n) => {
                    let n = *n;
                    self.advance()?;
                    redirects.push(self.parse_redirect(Some(n))?);
                }
                _ => break,
            }
        }

        if program.is_none() && assignments.is_empty() {
            return Err(ParseError::UnexpectedEof);
        }
        Ok(SimpleCommand { assignments, program, args, redirects })
    }

    fn parse_redirect(&mut self, fd: Option<u32>) -> Result<Redirect, ParseError> {
        let op = match self.advance()? {
            Token::Great => RedirectOp::Out,
            Token::DGreat => RedirectOp::Append,
            Token::Less => RedirectOp::In,
            Token::DLess => RedirectOp::Heredoc,
            Token::DLessDash => RedirectOp::HeredocStrip,
            Token::DLessLess => RedirectOp::HereString,
            Token::GreatAmp => RedirectOp::DupOut,
            t => return Err(ParseError::Unexpected(format!("{t:?}"))),
        };
        let target = match self.advance()? {
            Token::Word(w, quoted) => Word { text: w, quoted },
            t => return Err(ParseError::Unexpected(format!("expected redirect target, got {t:?}"))),
        };
        // Heredoc: switch the lexer into raw-line capture immediately — the
        // body must never be tokenized as bash syntax (docs/ast-execution.md,
        // "words are not fully parsed at parse time"; heredoc bodies are the
        // same principle, triggered by a redirect instead of a bracket).
        // Safe to call now: `advance()` above just cleared the lookahead
        // buffer, so the lexer's position is exactly after the delimiter word.
        //
        // The delimiter's own quoting (`<<'EOF'`/`<<"EOF"`, meaning "no
        // expansion inside the body") is stripped before comparing against
        // body lines — bash matches the terminator on its bare text, not the
        // literal quote characters (found by a failing test: `'EOF'` never
        // matched a body line reading `EOF`, so parsing silently consumed to
        // end-of-input instead of stopping at the real terminator).
        let bare_delim: String = target.text.chars().filter(|c| *c != '\'' && *c != '"').collect();
        let heredoc_body = match op {
            RedirectOp::Heredoc => Some(self.lexer.capture_heredoc(&bare_delim, false)),
            RedirectOp::HeredocStrip => Some(self.lexer.capture_heredoc(&bare_delim, true)),
            _ => None,
        };
        Ok(Redirect { op, fd, target, heredoc_body })
    }
}

/// `NAME=value` at the *start* of a word only — bash's `token_is_assignment`
/// (parse.y) checks the whole prefix is a valid identifier before the `=`.
fn split_assignment(w: &str) -> Option<(String, String)> {
    let eq = w.find('=')?;
    let (name, rest) = w.split_at(eq);
    if name.is_empty() || !name.chars().next().unwrap().is_ascii_alphabetic() && name.chars().next().unwrap() != '_' {
        return None;
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name.to_string(), rest[1..].to_string()))
}

pub fn parse(src: &str) -> Result<Command, ParseError> {
    Parser::new(src).parse_program()
}


#[cfg(test)]
mod tests {
    use super::*;

    fn simple(cmd: &Command) -> &SimpleCommand {
        match cmd {
            Command::Simple(s) => s,
            other => panic!("expected Simple, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_bare_word() {
        let cmd = parse("echo hello").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.program.as_ref().unwrap().text, "echo");
        assert_eq!(s.args[0].text, "hello");
    }

    #[test]
    fn parses_quoted_words_without_losing_the_quotes() {
        let cmd = parse(r#"echo "hello world""#).unwrap();
        let s = simple(&cmd);
        assert_eq!(s.args[0].text, "\"hello world\"");
        assert!(s.args[0].quoted);
    }

    #[test]
    fn captures_command_substitution_as_opaque_text() {
        let cmd = parse("echo $(git rev-parse HEAD)").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.args[0].text, "$(git rev-parse HEAD)");
    }

    #[test]
    fn parses_and_or_left_associative() {
        let cmd = parse("a && b || c").unwrap();
        match cmd {
            Command::Connection(Connection { connector: Connector::Or, left, .. }) => {
                match *left {
                    Command::Connection(Connection { connector: Connector::And, .. }) => {}
                    other => panic!("expected nested And on the left, got {other:?}"),
                }
            }
            other => panic!("expected top-level Or, got {other:?}"),
        }
    }

    #[test]
    fn pipe_binds_tighter_than_and() {
        let cmd = parse("a | b && c").unwrap();
        match cmd {
            Command::Connection(Connection { connector: Connector::And, left, .. }) => match *left {
                Command::Connection(Connection { connector: Connector::Pipe, .. }) => {}
                other => panic!("expected Pipe nested under And, got {other:?}"),
            },
            other => panic!("expected top-level And, got {other:?}"),
        }
    }


    #[test]
    fn fd_prefixed_redirect_is_one_redirect_not_a_stray_argument() {
        // Found live via the AST printer: `2>&1` was silently splitting into
        // a bogus "2" argument plus an fd-less `>&1` redirect.
        let cmd = parse("echo hi 2>&1").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.args.len(), 1, "args: {:?}", s.args);
        assert_eq!(s.redirects.len(), 1);
        assert_eq!(s.redirects[0].fd, Some(2));
    }

    #[test]
    fn parses_redirects() {
        let cmd = parse("cmd > out.txt 2>&1").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.redirects.len(), 2);
    }

    #[test]
    fn parses_leading_assignment_with_no_program() {
        let cmd = parse("FOO=bar").unwrap();
        let s = simple(&cmd);
        assert!(s.program.is_none());
        assert_eq!(s.assignments[0].0, "FOO");
        assert_eq!(s.assignments[0].1.text, "bar");
    }

    #[test]
    fn subshell_body_supports_full_sequencing_not_just_and_or() {
        // Found by a real corpus failure: `( a; b; c )` and `( cmd & )` only
        // parsed their first command before parse_command_list existed —
        // Subshell/Group called parse_and_or (&&/||/|  only) directly.
        let cmd = parse("(echo a; echo b)").unwrap();
        match cmd {
            Command::Subshell(inner) => match *inner {
                Command::Connection(Connection { connector: Connector::Seq, .. }) => {}
                other => panic!("expected Seq inside the subshell, got {other:?}"),
            },
            other => panic!("expected Subshell, got {other:?}"),
        }
    }

    #[test]
    fn parses_subshell() {
        let cmd = parse("(echo hi)").unwrap();
        assert!(matches!(cmd, Command::Subshell(_)));
    }

    #[test]
    fn compound_keywords_are_a_named_unsupported_error_not_silent() {
        let err = parse("for x in a b; do echo $x; done").unwrap_err();
        assert!(matches!(err, ParseError::Unsupported("for")));
    }

    #[test]
    fn newline_sequencing_matches_semicolon() {
        let cmd = parse("echo a\necho b").unwrap();
        match cmd {
            Command::Connection(Connection { connector: Connector::Seq, .. }) => {}
            other => panic!("expected Seq connection across the newline, got {other:?}"),
        }
    }
    #[test]
    fn heredoc_body_is_captured_verbatim_not_tokenized() {
        // The exact failure mode found against the real corpus: source
        // containing `(`/`{` inside a heredoc body must not be read as bash
        // syntax.
        let cmd = parse("cat > f.py << 'EOF'\nimport sys\ndef foo(x):\n    return {x: 1}\nEOF\n").unwrap();
        let s = simple(&cmd);
        let body = s.redirects[1].heredoc_body.as_ref().unwrap();
        assert!(body.contains("def foo(x):"));
        assert!(body.contains("return {x: 1}"));
        assert!(!body.contains("EOF"));
    }

    #[test]
    fn keywords_are_only_special_in_command_position_not_as_arguments() {
        // bash: `grep -l for file.txt` must not try to start a for-loop.
        // parse_command's keyword check only ever fires at the point a NEW
        // command starts; parse_simple_command's own arg-consuming loop
        // never re-checks a word it swallows as an argument against
        // COMPOUND_KEYWORDS. Proven directly here, not just asserted.
        let cmd = parse("grep -l for file.txt").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.program.as_ref().unwrap().text, "grep");
        assert_eq!(s.args[1].text, "for");
    }

}
