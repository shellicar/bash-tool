//! The only place a shell starts things concurrently is a pipeline, so the
//! only trace lines allowed to permute are one pipeline's stages. This
//! reports, per command, the widest pipeline it contains, which is exactly how
//! many adjacent trace lines a comparator may accept out of order.
//!
//! Reads a JSON array of command strings on stdin, writes a JSON array of
//! integers on stdout, one per command. 1 means no pipeline, so nothing may
//! permute; a parse failure reports 1 for the same reason.
//!
//! `cargo run --release --example pipeline_shape -p bash-parser < commands.json`
use bash_parser::{CaseCommand, Command, Connection, Connector, ForCommand, IfCommand};

fn widest(cmd: &Command) -> usize {
    match cmd {
        Command::Connection(Connection { left, right, connector }) => {
            let (l, r) = (widest(left), widest(right));
            if *connector == Connector::Pipe {
                // A pipeline is a left-leaning chain, so its width is the
                // number of leaves along it, not the deeper of two branches.
                stages(cmd)
            } else {
                l.max(r)
            }
        }
        Command::Invert(i) | Command::Time(i) | Command::Background(i) | Command::Subshell(i) | Command::Group(i) => widest(i),
        Command::Redirected { command, .. } => widest(command),
        Command::For(ForCommand { body, .. }) => widest(body),
        Command::ArithFor { body, .. } => widest(body),
        Command::While { cond, body } | Command::Until { cond, body } => widest(cond).max(widest(body)),
        Command::If(IfCommand { branches, else_branch }) => {
            let mut w = 1;
            for (c, b) in branches {
                w = w.max(widest(c)).max(widest(b));
            }
            if let Some(e) = else_branch {
                w = w.max(widest(e));
            }
            w
        }
        Command::Case(CaseCommand { arms, .. }) => {
            let mut w = 1;
            for a in arms {
                if let Some(b) = &a.body {
                    w = w.max(widest(b));
                }
            }
            w
        }
        Command::FunctionDef { body, .. } => widest(body),
        Command::Simple(_) | Command::Cond(_) | Command::Arith { .. } => 1,
    }
}

fn stages(cmd: &Command) -> usize {
    match cmd {
        Command::Connection(Connection { left, right, connector }) if *connector == Connector::Pipe => {
            stages(left) + stages(right)
        }
        _ => 1,
    }
}

fn main() {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).expect("stdin");
    let commands: Vec<String> = serde_json::from_str(&input).expect("a JSON array of strings");
    let widths: Vec<usize> = commands
        .iter()
        .map(|c| bash_parser::parse(c).map(|ast| widest(&ast)).unwrap_or(1))
        .collect();
    println!("{}", serde_json::to_string(&widths).expect("serialise"));
}
