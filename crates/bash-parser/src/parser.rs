//! Recursive-descent parser over the corpus-scoped grammar
//! (docs/ast-execution.md): simple commands, `&&`/`||`/`;`/`&`/`|`
//! connections, redirects (including trailing redirects after a compound
//! command's closer), subshells `( )`, brace groups `{ ; }`,
//! `for`/`if`/`while`/`until`/`case`/`function` definitions, `[[ ]]`
//! conditionals, `((...))` arithmetic commands, and `!`/`time` wrappers.
//! Substitution interiors stay opaque (lexer's job). `select` and `coproc`
//! remain a named `ParseError::Unsupported` — real, honest scope, not a
//! silent gap.

use std::collections::VecDeque;

use crate::ast::*;
use crate::lexer::{LexError, Lexer, Token};

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error("unexpected token {0}")]
    Unexpected(String),
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("'{0}' is not yet supported by this parser (see docs/ast-execution.md)")]
    Unsupported(&'static str),
}

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

    fn expect(&mut self, expected: Token) -> Result<(), ParseError> {
        let got = self.advance()?;
        if got == expected {
            Ok(())
        } else {
            Err(ParseError::Unexpected(format!("expected {expected:?}, got {got:?}")))
        }
    }

    /// Reserved words arrive as ordinary `Word` tokens; a quoted or escaped
    /// keyword never matches because its text still carries the quote or
    /// backslash characters (`'if'`, `\if`), same effect as bash's
    /// only-unquoted-words-are-reserved rule.
    fn expect_word(&mut self, s: &str) -> Result<(), ParseError> {
        match self.advance()? {
            Token::Word(w, _) if w == s => Ok(()),
            t => Err(ParseError::Unexpected(format!("expected '{s}', got {t:?}"))),
        }
    }

    fn advance_word(&mut self, what: &str) -> Result<Word, ParseError> {
        match self.advance()? {
            Token::Word(text, quoted) => Ok(Word { text, quoted }),
            t => Err(ParseError::Unexpected(format!("expected {what}, got {t:?}"))),
        }
    }

    /// Parse a full program: a sequence of top-level commands (bash's
    /// `simple_list simple_list_terminator | ...`, parse.y:433 onward,
    /// scoped to this grammar).
    pub fn parse_program(&mut self) -> Result<Command, ParseError> {
        self.parse_command_list(|t| matches!(t, Token::Eof))
    }

    /// A sequence of `&&`/`||`/`|`-commands joined by `;`/`&`/newline — the
    /// same grammar at the top level AND inside `( )`/`{ }`/compound-command
    /// bodies (bash's `compound_list`). `is_end` tells the caller's
    /// terminator apart from a real next command: `Eof` at the top level,
    /// `RParen` inside a subshell, a reserved word (`done`, `fi`, `esac`,
    /// `}`) inside a compound body, a case-arm terminator inside `case`.
    fn parse_command_list(&mut self, is_end: impl Fn(&Token) -> bool) -> Result<Command, ParseError> {
        self.skip_newlines()?;
        // `folded` is every element already terminated; `element` is the one
        // the next `;`, `&` or newline terminates. They are kept apart because
        // `&` backgrounds the element it follows and nothing else: bash runs
        // `a; b & c` as a, then b in the background, then c. Backgrounding the
        // whole accumulated sequence instead swept a's output and state into a
        // detached child the shell never waits for.
        let mut folded: Option<Command> = None;
        let mut element = self.parse_and_or()?;
        loop {
            if is_end(self.peek()?) {
                break;
            }
            match self.peek()? {
                Token::Semi | Token::Amp => {
                    if matches!(self.peek()?, Token::Amp) {
                        element = Command::Background(Box::new(element));
                    }
                    self.advance()?;
                    self.skip_newlines()?;
                    if is_end(self.peek()?) {
                        break;
                    }
                    folded = Some(Self::join_sequence(folded, element));
                    element = self.parse_and_or()?;
                }
                Token::Newline => {
                    self.skip_newlines()?;
                    if is_end(self.peek()?) {
                        break;
                    }
                    folded = Some(Self::join_sequence(folded, element));
                    element = self.parse_and_or()?;
                }
                other => return Err(ParseError::Unexpected(format!("{other:?}"))),
            }
        }
        Ok(Self::join_sequence(folded, element))
    }

    /// `;` chains left-leaning, the same shape as bash's own `cm_connection`.
    fn join_sequence(folded: Option<Command>, element: Command) -> Command {
        match folded {
            Some(left) => Command::Connection(Connection {
                left: Box::new(left),
                right: Box::new(element),
                connector: Connector::Seq,
            }),
            None => element,
        }
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
    /// `!` and `time` prefix the whole pipeline (bash's `pipeline_command`
    /// production), not just its first command.
    fn parse_pipeline(&mut self) -> Result<Command, ParseError> {
        match self.peek()? {
            Token::Word(w, _) if w == "!" => {
                self.advance()?;
                return Ok(Command::Invert(Box::new(self.parse_pipeline()?)));
            }
            Token::Word(w, _) if w == "time" => {
                self.advance()?;
                if matches!(self.peek()?, Token::Word(f, _) if f == "-p") {
                    self.advance()?; // POSIX-format flag; not represented in the AST
                }
                return Ok(Command::Time(Box::new(self.parse_pipeline()?)));
            }
            _ => {}
        }
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
        let cmd = match self.peek()? {
            Token::LParen => {
                self.advance()?;
                let inner = self.parse_command_list(|t| matches!(t, Token::RParen))?;
                self.skip_newlines()?;
                self.expect(Token::RParen)?;
                Command::Subshell(Box::new(inner))
            }
            Token::Arith(_) => {
                let Token::Arith(expr) = self.advance()? else { unreachable!() };
                Command::Arith { expr }
            }
            Token::Word(_, _) => {
                let kw = match self.peek()? {
                    Token::Word(w, _) => w.clone(),
                    _ => unreachable!(),
                };
                match kw.as_str() {
                    "{" => {
                        self.advance()?;
                        let inner = self.parse_command_list(|t| matches!(t, Token::Word(w, _) if w == "}"))?;
                        self.skip_newlines()?;
                        self.expect_word("}")?;
                        Command::Group(Box::new(inner))
                    }
                    "for" => self.parse_for()?,
                    "if" => self.parse_if()?,
                    "while" => {
                        self.advance()?;
                        let (cond, body) = self.parse_loop_cond_and_body()?;
                        Command::While { cond, body }
                    }
                    "until" => {
                        self.advance()?;
                        let (cond, body) = self.parse_loop_cond_and_body()?;
                        Command::Until { cond, body }
                    }
                    "case" => self.parse_case()?,
                    "function" => self.parse_function_keyword()?,
                    "[[" => self.parse_cond()?,
                    "select" => return Err(ParseError::Unsupported("select")),
                    "coproc" => return Err(ParseError::Unsupported("coproc")),
                    _ => return self.parse_simple_or_funcdef(),
                }
            }
            _ => return self.parse_simple_or_funcdef(),
        };
        self.attach_trailing_redirects(cmd)
    }

    /// Redirects after a compound command's closer — `{ cmds; } > file`,
    /// `done < input`, `(cmds) 2>&1`. Bash attaches these to the compound
    /// node itself (the repeated `t->next = $2` pattern in parse.y's grammar
    /// actions); here they wrap it. One of the three known corpus gaps from
    /// the first pass.
    fn attach_trailing_redirects(&mut self, cmd: Command) -> Result<Command, ParseError> {
        let mut redirects = Vec::new();
        loop {
            match self.peek()? {
                Token::Great | Token::DGreat | Token::Less | Token::DLess | Token::DLessDash
                | Token::DLessLess | Token::GreatAmp | Token::LessAmp | Token::AmpGreat
                | Token::AmpDGreat => redirects.push(self.parse_redirect(None)?),
                Token::Fd(n) => {
                    let n = *n;
                    self.advance()?;
                    redirects.push(self.parse_redirect(Some(n))?);
                }
                _ => break,
            }
        }
        if redirects.is_empty() {
            Ok(cmd)
        } else {
            Ok(Command::Redirected { command: Box::new(cmd), redirects })
        }
    }

    fn parse_loop_cond_and_body(&mut self) -> Result<(Box<Command>, Box<Command>), ParseError> {
        let cond = self.parse_command_list(|t| matches!(t, Token::Word(w, _) if w == "do"))?;
        let body = self.parse_do_done()?;
        Ok((Box::new(cond), body))
    }

    fn parse_do_done(&mut self) -> Result<Box<Command>, ParseError> {
        self.expect_word("do")?;
        let body = self.parse_command_list(|t| matches!(t, Token::Word(w, _) if w == "done"))?;
        self.expect_word("done")?;
        Ok(Box::new(body))
    }

    fn consume_list_separators(&mut self) -> Result<(), ParseError> {
        if matches!(self.peek()?, Token::Semi) {
            self.advance()?;
        }
        self.skip_newlines()
    }

    fn parse_for(&mut self) -> Result<Command, ParseError> {
        self.expect_word("for")?;
        if matches!(self.peek()?, Token::Arith(_)) {
            let Token::Arith(expr) = self.advance()? else { unreachable!() };
            self.consume_list_separators()?;
            let body = self.parse_do_done()?;
            return Ok(Command::ArithFor { expr, body });
        }
        let var = match self.advance()? {
            Token::Word(w, _) => w,
            t => return Err(ParseError::Unexpected(format!("expected for-loop variable, got {t:?}"))),
        };
        self.skip_newlines()?;
        let mut words = Vec::new();
        if matches!(self.peek()?, Token::Word(w, _) if w == "in") {
            self.advance()?;
            while matches!(self.peek()?, Token::Word(_, _)) {
                let Token::Word(text, quoted) = self.advance()? else { unreachable!() };
                words.push(Word { text, quoted });
            }
        }
        self.consume_list_separators()?;
        let body = self.parse_do_done()?;
        Ok(Command::For(ForCommand { var, words, body }))
    }

    fn parse_if(&mut self) -> Result<Command, ParseError> {
        self.expect_word("if")?;
        let mut branches = Vec::new();
        let mut else_branch = None;
        loop {
            let cond = self.parse_command_list(|t| matches!(t, Token::Word(w, _) if w == "then"))?;
            self.expect_word("then")?;
            let body = self.parse_command_list(
                |t| matches!(t, Token::Word(w, _) if w == "elif" || w == "else" || w == "fi"),
            )?;
            branches.push((Box::new(cond), Box::new(body)));
            match self.advance()? {
                Token::Word(w, _) if w == "elif" => continue,
                Token::Word(w, _) if w == "else" => {
                    let body = self.parse_command_list(|t| matches!(t, Token::Word(w, _) if w == "fi"))?;
                    self.expect_word("fi")?;
                    else_branch = Some(Box::new(body));
                    break;
                }
                Token::Word(w, _) if w == "fi" => break,
                t => return Err(ParseError::Unexpected(format!("expected elif/else/fi, got {t:?}"))),
            }
        }
        Ok(Command::If(IfCommand { branches, else_branch }))
    }

    fn parse_case(&mut self) -> Result<Command, ParseError> {
        self.expect_word("case")?;
        let word = self.advance_word("case subject word")?;
        self.skip_newlines()?;
        self.expect_word("in")?;
        let mut arms = Vec::new();
        loop {
            self.skip_newlines()?;
            if matches!(self.peek()?, Token::Word(w, _) if w == "esac") {
                self.advance()?;
                break;
            }
            if matches!(self.peek()?, Token::LParen) {
                self.advance()?; // optional `(` before the pattern list
            }
            let mut patterns = Vec::new();
            loop {
                patterns.push(self.advance_word("case pattern")?);
                if matches!(self.peek()?, Token::Pipe) {
                    self.advance()?;
                } else {
                    break;
                }
            }
            self.expect(Token::RParen)?;
            self.skip_newlines()?;
            let body = if self.at_case_arm_end()? {
                None
            } else {
                Some(Box::new(self.parse_command_list(|t| {
                    matches!(t, Token::DSemi | Token::SemiAmp | Token::DSemiAmp)
                        || matches!(t, Token::Word(w, _) if w == "esac")
                })?))
            };
            let terminator = match self.peek()? {
                Token::DSemi => {
                    self.advance()?;
                    CaseTerminator::Stop
                }
                Token::SemiAmp => {
                    self.advance()?;
                    CaseTerminator::Fallthrough
                }
                Token::DSemiAmp => {
                    self.advance()?;
                    CaseTerminator::TestNext
                }
                // last arm may omit `;;` — `esac` stays for the loop head
                _ => CaseTerminator::Stop,
            };
            arms.push(CaseArm { patterns, body, terminator });
        }
        Ok(Command::Case(CaseCommand { word, arms }))
    }

    fn at_case_arm_end(&mut self) -> Result<bool, ParseError> {
        Ok(matches!(self.peek()?, Token::DSemi | Token::SemiAmp | Token::DSemiAmp)
            || matches!(self.peek()?, Token::Word(w, _) if w == "esac"))
    }

    fn parse_function_keyword(&mut self) -> Result<Command, ParseError> {
        self.expect_word("function")?;
        let name = match self.advance()? {
            Token::Word(w, _) => w,
            t => return Err(ParseError::Unexpected(format!("expected function name, got {t:?}"))),
        };
        if matches!(self.peek()?, Token::LParen) {
            self.advance()?;
            self.expect(Token::RParen)?;
        }
        self.skip_newlines()?;
        let body = self.parse_command()?;
        Ok(Command::FunctionDef { name, body: Box::new(body) })
    }

    /// `[[ ]]` — bash parses this with a separate hand-written
    /// recursive-descent parser, not the bison grammar (parse.y:5031-5249);
    /// mirrored here over the lexer's raw whitespace-split chunks so regex
    /// operands like `^(a|b)$` never meet the operator tokenizer.
    fn parse_cond(&mut self) -> Result<Command, ParseError> {
        self.expect_word("[[")?;
        // Safe to read the lexer directly: expect_word just drained the
        // lookahead, so its position is exactly after `[[`.
        let chunks = self.lexer.cond_chunks()?;
        let expr = parse_cond_chunks(&chunks)?;
        Ok(Command::Cond(expr))
    }

    fn parse_simple_or_funcdef(&mut self) -> Result<Command, ParseError> {
        let mut assignments = Vec::new();
        let mut program: Option<Word> = None;
        let mut args = Vec::new();
        let mut redirects = Vec::new();

        loop {
            match self.peek()? {
                Token::Word(w, quoted) => {
                    let w = w.clone();
                    let quoted = *quoted;
                    if program.is_none() {
                        if let Some((name, v, append)) = split_assignment(&w) {
                            self.advance()?;
                            assignments.push(Assign {
                                name,
                                value: Word { text: v, quoted },
                                append,
                            });
                            continue;
                        }
                    }
                    self.advance()?;
                    if program.is_none() {
                        // `name() body` — the POSIX function-definition form.
                        // Only checkable after the name is consumed (single-
                        // token lookahead), which is why this lives here and
                        // not in parse_command.
                        if assignments.is_empty()
                            && redirects.is_empty()
                            && is_name(&w)
                            && matches!(self.peek()?, Token::LParen)
                        {
                            self.advance()?;
                            self.expect(Token::RParen)?;
                            self.skip_newlines()?;
                            let body = self.parse_command()?;
                            return Ok(Command::FunctionDef { name: w, body: Box::new(body) });
                        }
                        program = Some(Word { text: w, quoted });
                    } else {
                        args.push(Word { text: w, quoted });
                    }
                }
                Token::Great | Token::DGreat | Token::Less | Token::DLess | Token::DLessDash
                | Token::DLessLess | Token::GreatAmp | Token::LessAmp | Token::AmpGreat
                | Token::AmpDGreat => {
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

        if program.is_none() && assignments.is_empty() && redirects.is_empty() {
            return Err(ParseError::UnexpectedEof);
        }
        Ok(Command::Simple(SimpleCommand { assignments, program, args, redirects }))
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
            Token::LessAmp => RedirectOp::DupIn,
            Token::AmpGreat => RedirectOp::OutErr,
            Token::AmpDGreat => RedirectOp::AppendOutErr,
            t => return Err(ParseError::Unexpected(format!("{t:?}"))),
        };
        let target = match self.advance()? {
            Token::Word(w, quoted) => Word { text: w, quoted },
            t => return Err(ParseError::Unexpected(format!("expected redirect target, got {t:?}"))),
        };
        // Heredoc: the body starts after the newline that ends THIS line,
        // which may still hold tokens belonging to the command
        // (`cat <<EOF | grep x`) — so the lexer defers capture until it
        // consumes that newline, and `parse()` matches bodies back to
        // redirects in source order afterwards. The delimiter's own quoting
        // (`<<'EOF'`, meaning "no expansion inside the body") is stripped
        // before comparing against body lines — bash matches the terminator
        // on its bare text, not the literal quote characters (found by a
        // failing test: `'EOF'` never matched a body line reading `EOF`).
        match op {
            RedirectOp::Heredoc | RedirectOp::HeredocStrip => {
                let bare_delim: String =
                    target.text.chars().filter(|c| *c != '\'' && *c != '"').collect();
                self.lexer.register_heredoc(bare_delim, matches!(op, RedirectOp::HeredocStrip));
            }
            _ => {}
        }
        Ok(Redirect { op, fd, target, heredoc_body: None })
    }
}

/// `NAME=value` at the *start* of a word only — bash's `token_is_assignment`
/// (parse.y) checks the whole prefix is a valid identifier before the `=`.
/// `NAME=value`, or `NAME+=value` which appends. Without the `+=` form the
/// whole word fails the name test and becomes the program name, so
/// `PATH+=:/opt/bin make` would try to run `PATH+=:/opt/bin`.
fn split_assignment(w: &str) -> Option<(String, String, bool)> {
    let eq = w.find('=')?;
    let (head, rest) = w.split_at(eq);
    let (name, append) = match head.strip_suffix('+') {
        Some(n) => (n, true),
        None => (head, false),
    };
    if !is_name(name) {
        return None;
    }
    Some((name.to_string(), rest[1..].to_string(), append))
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

const COND_UNARY_OPS: &[&str] = &[
    "-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-k", "-p", "-r", "-s", "-t", "-u", "-w",
    "-x", "-G", "-L", "-N", "-O", "-S", "-z", "-n", "-o", "-v", "-R",
];
const COND_BINARY_OPS: &[&str] = &[
    "==", "=", "!=", "=~", "<", ">", "-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef",
];

/// The `[[ ]]` mini-grammar (parse.y:5031-5249): or → and → term, `!`
/// negation, `( )` grouping, unary and binary tests. Right-recursive like
/// bash's own `cond_or`/`cond_and` — associativity is semantically
/// irrelevant for `&&`/`||` chains.
struct CondParser<'a> {
    chunks: &'a [String],
    i: usize,
}

impl<'a> CondParser<'a> {
    fn peek(&self) -> Option<&str> {
        self.chunks.get(self.i).map(|s| s.as_str())
    }

    fn bump_word(&mut self) -> Result<Word, ParseError> {
        match self.chunks.get(self.i) {
            Some(s) => {
                self.i += 1;
                Ok(Word { text: s.clone(), quoted: s.contains('\'') || s.contains('"') })
            }
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn parse_or(&mut self) -> Result<CondExpr, ParseError> {
        let left = self.parse_and()?;
        if self.peek() == Some("||") {
            self.i += 1;
            let right = self.parse_or()?;
            return Ok(CondExpr::Or(Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<CondExpr, ParseError> {
        let left = self.parse_term()?;
        if self.peek() == Some("&&") {
            self.i += 1;
            let right = self.parse_and()?;
            return Ok(CondExpr::And(Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> Result<CondExpr, ParseError> {
        match self.peek() {
            None => Err(ParseError::UnexpectedEof),
            Some("(") => {
                self.i += 1;
                let inner = self.parse_or()?;
                if self.peek() == Some(")") {
                    self.i += 1;
                    Ok(CondExpr::Group(Box::new(inner)))
                } else {
                    Err(ParseError::Unexpected("expected ')' in [[ ]]".to_string()))
                }
            }
            Some("!") => {
                self.i += 1;
                Ok(CondExpr::Not(Box::new(self.parse_term()?)))
            }
            // `[[ -f ]]` (operand missing) is bash's one-argument -n sugar on
            // the string "-f", not a unary test — hence the lookahead count.
            Some(op) if COND_UNARY_OPS.contains(&op) && self.chunks.len() - self.i >= 2 => {
                let op = op.to_string();
                self.i += 1;
                let operand = self.bump_word()?;
                Ok(CondExpr::Unary { op, operand })
            }
            Some(_) => {
                let left = self.bump_word()?;
                match self.peek() {
                    Some(op) if COND_BINARY_OPS.contains(&op) => {
                        let op = op.to_string();
                        self.i += 1;
                        let right = self.bump_word()?;
                        Ok(CondExpr::Binary { op, left, right })
                    }
                    _ => Ok(CondExpr::Term(left)),
                }
            }
        }
    }
}

fn parse_cond_chunks(chunks: &[String]) -> Result<CondExpr, ParseError> {
    let mut p = CondParser { chunks, i: 0 };
    let expr = p.parse_or()?;
    if p.i != chunks.len() {
        return Err(ParseError::Unexpected(format!(
            "trailing tokens in [[ ]]: {:?}",
            &chunks[p.i..]
        )));
    }
    Ok(expr)
}

pub fn parse(src: &str) -> Result<Command, ParseError> {
    let mut parser = Parser::new(src);
    let mut cmd = parser.parse_program()?;
    let mut bodies = parser.lexer.take_bodies();
    fill_heredoc_bodies(&mut cmd, &mut bodies);
    Ok(cmd)
}

/// Match captured heredoc bodies back to their redirects. The lexer queues
/// bodies in the order the `<<`s appeared; an in-order walk of the AST
/// visits heredoc redirects in that same source order.
fn fill_heredoc_bodies(cmd: &mut Command, bodies: &mut VecDeque<String>) {
    match cmd {
        Command::Simple(s) => {
            for r in &mut s.redirects {
                fill_redirect(r, bodies);
            }
        }
        Command::Connection(c) => {
            fill_heredoc_bodies(&mut c.left, bodies);
            fill_heredoc_bodies(&mut c.right, bodies);
        }
        Command::Invert(inner)
        | Command::Time(inner)
        | Command::Background(inner)
        | Command::Subshell(inner)
        | Command::Group(inner) => fill_heredoc_bodies(inner, bodies),
        Command::Redirected { command, redirects } => {
            fill_heredoc_bodies(command, bodies);
            for r in redirects {
                fill_redirect(r, bodies);
            }
        }
        Command::For(f) => fill_heredoc_bodies(&mut f.body, bodies),
        Command::ArithFor { body, .. } => fill_heredoc_bodies(body, bodies),
        Command::If(i) => {
            for (cond, body) in &mut i.branches {
                fill_heredoc_bodies(cond, bodies);
                fill_heredoc_bodies(body, bodies);
            }
            if let Some(e) = &mut i.else_branch {
                fill_heredoc_bodies(e, bodies);
            }
        }
        Command::Case(c) => {
            for arm in &mut c.arms {
                if let Some(b) = &mut arm.body {
                    fill_heredoc_bodies(b, bodies);
                }
            }
        }
        Command::While { cond, body } | Command::Until { cond, body } => {
            fill_heredoc_bodies(cond, bodies);
            fill_heredoc_bodies(body, bodies);
        }
        Command::FunctionDef { body, .. } => fill_heredoc_bodies(body, bodies),
        Command::Cond(_) | Command::Arith { .. } => {}
    }
}

fn fill_redirect(r: &mut Redirect, bodies: &mut VecDeque<String>) {
    if matches!(r.op, RedirectOp::Heredoc | RedirectOp::HeredocStrip) {
        r.heredoc_body = bodies.pop_front();
    }
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
    fn quoted_close_paren_does_not_end_a_command_substitution() {
        // scan_matched counted brackets without quote state — a `)` inside
        // a quoted string ended the `$(...)` span early (real corpus
        // failure, bash-valid command rejected).
        let cmd = parse(r#"echo $(git log -S "foo)bar" --oneline)"#).unwrap();
        let s = simple(&cmd);
        let expected = r#"$(git log -S "foo)bar" --oneline)"#;
        let actual = &s.args[0].text;
        assert_eq!(actual, expected);
    }

    #[test]
    fn double_quotes_inside_a_substitution_inside_double_quotes_nest() {
        // Substitutions stay active inside double quotes, so the inner
        // quotes belong to the inner span — the outer quote must not close
        // at the first `"` it meets.
        let cmd = parse(r#"echo "$(date "+%Y")""#).unwrap();
        let s = simple(&cmd);
        let expected = r#""$(date "+%Y")""#;
        let actual = &s.args[0].text;
        assert_eq!(actual, expected);
    }

    #[test]
    fn unterminated_param_expansion_inside_double_quotes_is_an_error() {
        // bash rejects `grep "user/${" ...` — `${` is live inside double
        // quotes; treating it as literal text silently accepted a command
        // real bash refuses.
        let actual = parse(r#"grep -rn "user/${" src"#);
        assert!(actual.is_err());
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
    fn ampersand_redirect_is_one_out_err_redirect_not_a_background_command() {
        let cmd = parse("cmd &> /dev/null").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.redirects.len(), 1);
        assert_eq!(s.redirects[0].op, RedirectOp::OutErr);
    }

    #[test]
    fn redirect_only_command_has_no_program() {
        let cmd = parse("> file.txt").unwrap();
        let s = simple(&cmd);
        assert!(s.program.is_none());
        assert_eq!(s.redirects.len(), 1);
    }

    #[test]
    fn parses_leading_assignment_with_no_program() {
        let cmd = parse("FOO=bar").unwrap();
        let s = simple(&cmd);
        assert!(s.program.is_none());
        assert_eq!(s.assignments[0].name, "FOO");
        assert_eq!(s.assignments[0].value.text, "bar");
    }

    #[test]
    fn array_assignment_is_one_word() {
        let cmd = parse("FOO=(a b c)").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.assignments[0].value.text, "(a b c)");
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
    fn newline_sequencing_matches_semicolon() {
        let cmd = parse("echo a\necho b").unwrap();
        match cmd {
            Command::Connection(Connection { connector: Connector::Seq, .. }) => {}
            other => panic!("expected Seq connection across the newline, got {other:?}"),
        }
    }

    #[test]
    fn trailing_ampersand_is_a_background_command() {
        let cmd = parse("sleep 5 &").unwrap();
        assert!(matches!(cmd, Command::Background(_)));
    }

    #[test]
    fn ampersand_backgrounds_only_the_element_before_it() {
        let cmd = parse("echo before; sleep 1 & echo after").unwrap();
        match cmd {
            Command::Connection(Connection { connector: Connector::Seq, left, .. }) => match *left {
                Command::Connection(Connection { connector: Connector::Seq, right, .. }) => {
                    assert!(matches!(*right, Command::Background(_)));
                }
                other => panic!("expected the first two elements joined by ;, got {other:?}"),
            },
            other => panic!("expected a ; sequence at the top, got {other:?}"),
        }
    }

    #[test]
    fn a_command_before_a_trailing_ampersand_stays_in_the_foreground() {
        let cmd = parse("echo a; sleep 5 &").unwrap();
        match cmd {
            Command::Connection(Connection { connector: Connector::Seq, left, .. }) => {
                assert!(matches!(*left, Command::Simple(_)));
            }
            other => panic!("expected `echo a` outside the background job, got {other:?}"),
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
    fn heredoc_keeps_same_line_tokens_after_the_delimiter() {
        // Body capture starts after the LINE's newline, not right after the
        // delimiter word — `| grep b` here belongs to the command. The first
        // pass discarded those tokens.
        let cmd = parse("cat <<EOF | grep b\na\nb\nEOF\n").unwrap();
        match cmd {
            Command::Connection(Connection { connector: Connector::Pipe, left, .. }) => {
                let s = simple(&left);
                let expected = "a\nb\n";
                let actual = s.redirects[0].heredoc_body.as_ref().unwrap();
                assert_eq!(actual, expected);
            }
            other => panic!("expected Pipe connection, got {other:?}"),
        }
    }

    #[test]
    fn two_heredocs_get_their_own_bodies_in_source_order() {
        let cmd = parse("cat <<A; cat <<B\none\nA\ntwo\nB\n").unwrap();
        match cmd {
            Command::Connection(Connection { left, right, .. }) => {
                let expected_first = "one\n";
                let actual_first = simple(&left).redirects[0].heredoc_body.as_ref().unwrap();
                assert_eq!(actual_first, expected_first);
                let expected_second = "two\n";
                let actual_second = simple(&right).redirects[0].heredoc_body.as_ref().unwrap();
                assert_eq!(actual_second, expected_second);
            }
            other => panic!("expected Connection, got {other:?}"),
        }
    }

    #[test]
    fn braces_are_literal_text_inside_arguments() {
        // bash treats {/} as reserved words, special only in command
        // position — `find -exec`'s `{}` is an ordinary argument. The lexer
        // used to emit brace tokens unconditionally, breaking this.
        let cmd = parse(r"find . -name '*.rs' -exec rm {} \;").unwrap();
        let s = simple(&cmd);
        let expected = "{}";
        let actual = &s.args[5].text;
        assert_eq!(actual, expected);
    }

    #[test]
    fn brace_group_parses_in_command_position() {
        let cmd = parse("{ echo a; echo b; }").unwrap();
        match cmd {
            Command::Group(inner) => match *inner {
                Command::Connection(Connection { connector: Connector::Seq, .. }) => {}
                other => panic!("expected Seq inside the group, got {other:?}"),
            },
            other => panic!("expected Group, got {other:?}"),
        }
    }

    #[test]
    fn redirect_after_group_closer_attaches_to_the_whole_group() {
        // One of the three known corpus gaps from the first pass: real bash
        // attaches `{ cmds; } > file`'s redirect to the compound command.
        let cmd = parse("{ echo a; echo b; } > out.txt").unwrap();
        match cmd {
            Command::Redirected { command, redirects } => {
                assert!(matches!(*command, Command::Group(_)));
                assert_eq!(redirects.len(), 1);
            }
            other => panic!("expected Redirected(Group), got {other:?}"),
        }
    }

    #[test]
    fn fd_redirect_after_subshell_closer_keeps_its_fd() {
        let cmd = parse("(echo x) 2> err.log").unwrap();
        match cmd {
            Command::Redirected { command, redirects } => {
                assert!(matches!(*command, Command::Subshell(_)));
                assert_eq!(redirects[0].fd, Some(2));
            }
            other => panic!("expected Redirected(Subshell), got {other:?}"),
        }
    }

    #[test]
    fn process_substitution_is_an_opaque_word() {
        let cmd = parse("diff <(sort a.txt) <(sort b.txt)").unwrap();
        let s = simple(&cmd);
        let expected = "<(sort a.txt)";
        let actual = &s.args[0].text;
        assert_eq!(actual, expected);
    }

    #[test]
    fn keywords_are_only_special_in_command_position_not_as_arguments() {
        // bash: `grep -l for file.txt` must not try to start a for-loop.
        let cmd = parse("grep -l for file.txt").unwrap();
        let s = simple(&cmd);
        assert_eq!(s.program.as_ref().unwrap().text, "grep");
        assert_eq!(s.args[1].text, "for");
    }

    #[test]
    fn parses_while_loop() {
        let cmd = parse("while true; do echo hi; done").unwrap();
        assert!(matches!(cmd, Command::While { .. }));
    }

    #[test]
    fn parses_until_loop() {
        let cmd = parse("until test -f done.flag; do sleep 1; done").unwrap();
        assert!(matches!(cmd, Command::Until { .. }));
    }

    #[test]
    fn while_loop_with_trailing_input_redirect() {
        let cmd = parse("while read x; do echo $x; done < input.txt").unwrap();
        match cmd {
            Command::Redirected { command, redirects } => {
                assert!(matches!(*command, Command::While { .. }));
                assert_eq!(redirects[0].op, RedirectOp::In);
            }
            other => panic!("expected Redirected(While), got {other:?}"),
        }
    }

    #[test]
    fn parses_for_with_word_list() {
        let cmd = parse("for x in a b c; do echo $x; done").unwrap();
        match cmd {
            Command::For(f) => {
                assert_eq!(f.var, "x");
                assert_eq!(f.words.len(), 3);
            }
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn for_without_in_iterates_positional_parameters() {
        let cmd = parse("for x; do echo $x; done").unwrap();
        match cmd {
            Command::For(f) => assert!(f.words.is_empty()),
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn parses_c_style_arithmetic_for() {
        let cmd = parse("for ((i=0; i<3; i++)); do echo $i; done").unwrap();
        match cmd {
            Command::ArithFor { expr, .. } => assert_eq!(expr, "((i=0; i<3; i++))"),
            other => panic!("expected ArithFor, got {other:?}"),
        }
    }

    #[test]
    fn parses_if_elif_else() {
        let cmd = parse("if a; then b; elif c; then d; else e; fi").unwrap();
        match cmd {
            Command::If(i) => {
                assert_eq!(i.branches.len(), 2);
                assert!(i.else_branch.is_some());
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn parses_case_with_multi_pattern_arm() {
        let cmd = parse("case $x in a|b) echo one;; *) echo two;; esac").unwrap();
        match cmd {
            Command::Case(c) => {
                assert_eq!(c.arms.len(), 2);
                assert_eq!(c.arms[0].patterns.len(), 2);
            }
            other => panic!("expected Case, got {other:?}"),
        }
    }

    #[test]
    fn case_fallthrough_terminator_is_distinguished() {
        let cmd = parse("case $x in a) b;& c) d;; esac").unwrap();
        match cmd {
            Command::Case(c) => {
                assert_eq!(c.arms[0].terminator, CaseTerminator::Fallthrough);
                assert_eq!(c.arms[1].terminator, CaseTerminator::Stop);
            }
            other => panic!("expected Case, got {other:?}"),
        }
    }

    #[test]
    fn parses_posix_function_definition() {
        let cmd = parse("greet() { echo hi; }").unwrap();
        match cmd {
            Command::FunctionDef { name, body } => {
                assert_eq!(name, "greet");
                assert!(matches!(*body, Command::Group(_)));
            }
            other => panic!("expected FunctionDef, got {other:?}"),
        }
    }

    #[test]
    fn parses_function_keyword_definition() {
        let cmd = parse("function greet { echo hi; }").unwrap();
        match cmd {
            Command::FunctionDef { name, .. } => assert_eq!(name, "greet"),
            other => panic!("expected FunctionDef, got {other:?}"),
        }
    }

    #[test]
    fn parses_cond_binary_test() {
        let cmd = parse("[[ $x == y* ]]").unwrap();
        match cmd {
            Command::Cond(CondExpr::Binary { op, left, right }) => {
                assert_eq!(op, "==");
                assert_eq!(left.text, "$x");
                assert_eq!(right.text, "y*");
            }
            other => panic!("expected Cond(Binary), got {other:?}"),
        }
    }

    #[test]
    fn cond_regex_operand_keeps_its_metacharacters() {
        // `|` and `(` inside the regex must never meet the operator
        // tokenizer — the whole reason [[ ]] gets its own scanner.
        let cmd = parse("[[ $x =~ ^(a|b)$ ]]").unwrap();
        match cmd {
            Command::Cond(CondExpr::Binary { op, right, .. }) => {
                assert_eq!(op, "=~");
                assert_eq!(right.text, "^(a|b)$");
            }
            other => panic!("expected Cond(Binary), got {other:?}"),
        }
    }

    #[test]
    fn cond_and_combines_two_tests() {
        let cmd = parse("[[ -f x.txt && -n $y ]]").unwrap();
        match cmd {
            Command::Cond(CondExpr::And(left, right)) => {
                assert!(matches!(*left, CondExpr::Unary { .. }));
                assert!(matches!(*right, CondExpr::Unary { .. }));
            }
            other => panic!("expected Cond(And), got {other:?}"),
        }
    }

    #[test]
    fn cond_in_an_if_condition() {
        let cmd = parse(r#"if [[ -f x ]]; then echo yes; fi"#).unwrap();
        match cmd {
            Command::If(i) => assert!(matches!(*i.branches[0].0, Command::Cond(_))),
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn parses_arithmetic_command() {
        let cmd = parse("((i++))").unwrap();
        match cmd {
            Command::Arith { expr } => assert_eq!(expr, "((i++))"),
            other => panic!("expected Arith, got {other:?}"),
        }
    }

    #[test]
    fn adjacent_parens_that_are_not_arithmetic_stay_subshells() {
        // `((echo a); echo b)` is a subshell whose first command is itself
        // parenthesized — the `))`-lookahead must not swallow it as arith.
        let cmd = parse("((echo a); echo b)").unwrap();
        match cmd {
            Command::Subshell(inner) => match *inner {
                Command::Connection(Connection { connector: Connector::Seq, left, .. }) => {
                    assert!(matches!(*left, Command::Subshell(_)));
                }
                other => panic!("expected Seq inside subshell, got {other:?}"),
            },
            other => panic!("expected Subshell, got {other:?}"),
        }
    }

    #[test]
    fn bang_inverts_the_whole_pipeline() {
        let cmd = parse("! grep -q pattern file | wc -l").unwrap();
        match cmd {
            Command::Invert(inner) => {
                assert!(matches!(*inner, Command::Connection(Connection { connector: Connector::Pipe, .. })));
            }
            other => panic!("expected Invert, got {other:?}"),
        }
    }

    #[test]
    fn time_wraps_the_pipeline() {
        let cmd = parse("time cargo build").unwrap();
        assert!(matches!(cmd, Command::Time(_)));
    }

    #[test]
    fn select_is_a_named_unsupported_error_not_silent() {
        let err = parse("select x in a b; do echo $x; done").unwrap_err();
        assert!(matches!(err, ParseError::Unsupported("select")));
    }

    #[test]
    fn a_double_paren_span_of_two_subshells_is_not_arithmetic() {
        // bash runs this as nested subshells printing a then b; the
        // `))`-lookahead claims it because the span happens to end in two
        // closers.
        let cmd = parse("((echo a) && (echo b))").unwrap();
        match cmd {
            Command::Subshell(_) => {}
            other => panic!("expected Subshell, got {other:?}"),
        }
    }

    #[test]
    fn an_out_of_range_fd_prefix_is_reported_not_panicked_on() {
        let expected = true;

        let actual =
            std::panic::catch_unwind(|| parse("echo 99999999999999999999>x")).is_ok();

        assert_eq!(actual, expected);
    }

    #[test]
    fn ansi_c_quoting_survives_inside_a_command_substitution() {
        let expected = r"$(printf $'don\'t')";

        let cmd = parse(r"echo $(printf $'don\'t')").unwrap();
        let actual = &simple(&cmd).args[0].text;

        assert_eq!(actual, expected);
    }

    #[test]
    fn ansi_c_quoting_survives_inside_a_cond_expression() {
        let cmd = parse(r"[[ $x == $'a\'b' ]]").unwrap();
        match cmd {
            Command::Cond(CondExpr::Binary { right, .. }) => {
                let expected = r"$'a\'b'";

                let actual = right.text;

                assert_eq!(actual, expected);
            }
            other => panic!("expected Cond(Binary), got {other:?}"),
        }
    }

    #[test]
    fn an_append_assignment_is_not_taken_as_the_program_name() {
        let expected = "make";

        let cmd = parse("PATH+=:/opt/bin make").unwrap();
        let actual = &simple(&cmd).program.as_ref().unwrap().text;

        assert_eq!(actual, expected);
    }
}
