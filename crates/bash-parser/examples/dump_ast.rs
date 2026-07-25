//! See the actual parsed AST for a real command.
//! `cargo run --example dump_ast -p bash-parser -- 'cmd1 && cmd2 | cmd3'`
fn main() {
    let src = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    match bash_parser::parse(&src) {
        Ok(cmd) => print!("{}", bash_parser::pretty(&cmd)),
        Err(e) => eprintln!("parse error: {e}"),
    }
}
