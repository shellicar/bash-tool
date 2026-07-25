//! Scratch validation, not a real example: run the parser against a real
//! sample from the project's own corpus and tally outcomes. `cargo run
//! --example corpus_check -p bash-parser -- /tmp/corpus_sample.json`
use std::collections::HashMap;

fn main() {
    let path = std::env::args().nth(1).expect("path to a JSON array of strings");
    let raw = std::fs::read_to_string(path).unwrap();
    let commands: Vec<String> = serde_json::from_str(&raw).unwrap();

    let mut ok = 0;
    let mut unsupported: HashMap<String, usize> = HashMap::new();
    let mut other_errors = 0;
    let mut error_samples = Vec::new();

    for cmd in &commands {
        match bash_parser::parse(cmd) {
            Ok(_) => ok += 1,
            Err(bash_parser::ParseError::Unsupported(what)) => {
                *unsupported.entry(what.to_string()).or_insert(0) += 1;
            }
            Err(e) => {
                other_errors += 1;
                if error_samples.len() < 15 {
                    error_samples.push((cmd.chars().take(120).collect::<String>(), e.to_string()));
                }
            }
        }
    }

    println!("total: {}", commands.len());
    println!("parsed ok: {} ({:.1}%)", ok, 100.0 * ok as f64 / commands.len() as f64);
    println!("unsupported (named compound keyword):");
    let mut u: Vec<_> = unsupported.into_iter().collect();
    u.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (what, c) in u {
        println!("  {c:5}  {what}");
    }
    println!("other parse errors: {other_errors}");
    for (cmd, err) in error_samples {
        println!("  CMD: {cmd:?}");
        println!("  ERR: {err}");
    }
}
