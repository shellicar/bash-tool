//! AST shape, deliberately mirroring GNU Bash's own `command.h` `COMMAND` union
//! rather than inventing a fresh one — `cm_connection` is a left-leaning binary
//! tree of two `Command`s plus a connector, not an N-ary list (see
//! docs/ast-execution.md, "AST node shape"). Redirects thread through every
//! variant, attached after the fact, same as bash's own grammar actions.

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Word {
    /// The literal text as bash's parser saw it. `$(...)`/`` ` ` ``/`${...}`/
    /// `((...))` interiors are NOT resolved here — they stay as raw, opaque
    /// substrings inside this text, exactly like bash's own
    /// `parse_matched_pair()` (docs/ast-execution.md, "words are not fully
    /// parsed at parse time"). Only re-parsed when a substitution is actually
    /// evaluated.
    pub text: String,
    pub quoted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Connector {
    And,      // &&
    Or,       // ||
    Seq,      // ;  and `&`, whose own element is wrapped in Command::Background
    Pipe,     // |
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RedirectOp {
    // The corpus-scoped forms (docs/ast-execution.md feature-prevalence
    // table). Bash's real grammar has 19 r_instruction variants; still
    // unrepresented: `<>`, `{fd}>file` REDIR_WORD forms.
    Out,          // >
    Append,       // >>
    In,           // <
    DupOut,       // >&N or N>&M
    DupIn,        // <&N
    OutErr,       // &>   (stdout+stderr to file)
    AppendOutErr, // &>>
    Heredoc,      // <<
    HeredocStrip, // <<-
    HereString,   // <<<
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Redirect {
    pub op: RedirectOp,
    pub fd: Option<u32>,
    pub target: Word,
    /// Populated only for `Heredoc`/`HeredocStrip`: the captured raw body
    /// text, read verbatim (never tokenized as bash syntax) between the
    /// line after the redirect and a line matching `target`'s text.
    pub heredoc_body: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimpleCommand {
    /// Leading `VAR=val` assignments before the program name (parse.y's
    /// `clean_simple_command()` split). Real bash allows a bare assignment
    /// list with NO program name at all — that's the ergonomic form for
    /// setting variables in the calling shell (`cd`-like statefulness, see
    /// docs/ast-execution.md's `cd`/assignment finding). `program` is `None`
    /// in that case.
    pub assignments: Vec<(String, Word)>,
    pub program: Option<Word>,
    pub args: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Connection {
    pub left: Box<Command>,
    pub right: Box<Command>,
    pub connector: Connector,
}

/// The `[[ ]]` sub-grammar (parse.y:5031-5249, `cond_expr`/`cond_or`/
/// `cond_and`/`cond_term`) — a small separate recursive-descent parser in
/// real bash, not part of the bison rules. Mirrored here as its own node
/// family rather than folded into `Command`, matching bash's own separation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum CondExpr {
    Or(Box<CondExpr>, Box<CondExpr>),
    And(Box<CondExpr>, Box<CondExpr>),
    Not(Box<CondExpr>),
    /// Parenthesized sub-expression: `( expr )`.
    Group(Box<CondExpr>),
    /// A unary test: `-f word`, `-z word`, `-n word`, ...
    Unary { op: String, operand: Word },
    /// A binary test: `word OP word` — `=`/`==`/`!=` are glob-pattern match,
    /// not literal equality; `=~` is regex match populating capture-group
    /// state. Neither is delegable to a subprocess (docs/ast-execution.md).
    Binary { op: String, left: Word, right: Word },
    /// A bare word with no operator: `[[ x ]]` is sugar for `[[ -n x ]]`
    /// (parse.y:5176-5185) — represented directly as that rewrite so the
    /// walker never has to special-case it.
    Term(Word),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForCommand {
    pub var: String,
    /// Empty means the `for x; do` form — bash iterates over `"$@"`.
    pub words: Vec<Word>,
    pub body: Box<Command>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IfCommand {
    /// (condition, then-branch) pairs — the first is `if`, the rest are
    /// `elif`. `else`'s body, if present, is `else_branch`.
    pub branches: Vec<(Box<Command>, Box<Command>)>,
    pub else_branch: Option<Box<Command>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Option<Box<Command>>,
    /// `;;` (stop) vs `;&` (fallthrough) vs `;;&` (test-next) —
    /// parse.y:1222-1255's `CASEPAT_FALLTHROUGH`/`CASEPAT_TESTNEXT` flags.
    pub terminator: CaseTerminator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaseTerminator {
    Stop,
    Fallthrough,
    TestNext,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CaseCommand {
    pub word: Word,
    pub arms: Vec<CaseArm>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Command {
    Simple(SimpleCommand),
    Connection(Connection),
    /// `!` inversion — bash represents this as `CMD_INVERT_RETURN` flag on
    /// the wrapped node, not a new node kind; mirrored as an explicit
    /// wrapper since Rust doesn't have bash's shared-flags-field-on-every-
    /// variant shape without a lot of boilerplate.
    Invert(Box<Command>),
    /// `time [pipeline]` — bash's `CMD_TIME_PIPELINE` flag, same wrapper
    /// treatment as `Invert`.
    Time(Box<Command>),
    /// The element a `&` follows: the whole of `sleep 5 &`, and the `b` alone
    /// in `a; b & c`. `&` binds to its own element and never to the sequence
    /// around it, so that sequence stays ordinary `;` connections holding this
    /// wrapper.
    Background(Box<Command>),
    /// Redirects after a compound command's closer: `{ cmds; } > file`,
    /// `done < input`. Bash threads a `redirects` list through every
    /// COMMAND variant; a single wrapper node is semantically identical
    /// (the redirects apply to the whole wrapped command) without the
    /// per-variant boilerplate.
    Redirected { command: Box<Command>, redirects: Vec<Redirect> },
    Subshell(Box<Command>),
    Group(Box<Command>), // { ...; }
    For(ForCommand),
    /// C-style `for ((init; cond; step))` — the `((...))` span kept opaque,
    /// same deferred treatment as every other arithmetic context.
    ArithFor { expr: String, body: Box<Command> },
    If(IfCommand),
    Case(CaseCommand),
    While { cond: Box<Command>, body: Box<Command> },
    Until { cond: Box<Command>, body: Box<Command> },
    Cond(CondExpr), // [[ ... ]]
    /// `((...))` arithmetic command in command position; interior opaque
    /// until evaluation, exactly like `$((...))`.
    Arith { expr: String },
    FunctionDef { name: String, body: Box<Command> },
}
