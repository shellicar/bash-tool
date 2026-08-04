//! Reports, per command, how many adjacent trace lines a comparator may accept
//! out of order: the width of the widest pipeline it contains.
//!
//! Reads a JSON array of command strings on stdin, writes a JSON array of
//! integers on stdout. A parse failure reports 1, so nothing may permute.
//!
//! `cargo run --release --example pipeline_shape -p bash-parser < commands.json`
fn main() {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).expect("stdin");
    let commands: Vec<String> = serde_json::from_str(&input).expect("a JSON array of strings");
    let widths: Vec<usize> = commands
        .iter()
        .map(|c| bash_parser::parse(c).map(|ast| bash_parser::widest_pipeline(&ast)).unwrap_or(1))
        .collect();
    println!("{}", serde_json::to_string(&widths).expect("serialise"));
}
