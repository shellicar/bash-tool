//! Parse a script from stdin (or a path). Exit 0 if it parses, 1 with the
//! error on stderr if it does not.
//! `cargo run --example parse_check -p bash-parser -- file.sh`
use std::io::Read;

fn main() {
    let src = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(path).expect("read script"),
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).expect("read stdin");
            s
        }
    };
    match bash_parser::parse(&src) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
