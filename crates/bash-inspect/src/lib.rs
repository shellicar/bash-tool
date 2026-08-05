//! The stage between parsing and execution.
//!
//! Three stages, three questions. bash-parser answers whether text is bash
//! syntax and has no opinion beyond that. This crate answers whether the
//! result can be executed. bash-walker executes a tree that has already been
//! approved.
//!
//! The answer arrives before anything runs, which is the whole point: a
//! construct refused part-way through a walk has already let everything above
//! it take effect, so the caller approved one script and got part of one, with
//! no way to tell which part. Refusing up front leaves no partial state to
//! reason about (CLAUDE.md, "Unsupported constructs are refused by static
//! inspection, before anything runs").
//!
//! What the pass cannot see keeps its runtime refusal — computed text, `eval`,
//! `source`, aliases. The two are an addition, not a replacement.

use bash_parser::{Command, ParseError};

mod construct;
mod report;
mod scan;

pub use construct::{Construct, Instead};
pub use report::{Finding, Report};

#[derive(Debug)]
pub enum Refusal {
    /// Not bash syntax. The parser's answer, passed through unchanged.
    Syntax(ParseError),
    /// Bash syntax the walker cannot execute, holding every finding.
    Unsupported(Report),
}

/// Approve a script for execution, or refuse it with everything wrong with it.
///
/// An `Ok` tree is the thing the walker runs, so what was approved and what
/// runs are the same object.
pub fn inspect(source: &str) -> Result<Command, Refusal> {
    let found = scan::scan(source);
    match bash_parser::parse(source) {
        Ok(tree) => {
            // A parse that succeeded proves there was no `select` or `coproc`
            // in command position, because the parser refuses both. So a scan
            // match here was something else wearing the shape — a `case`
            // pattern written `(select)` reads as command position and is the
            // one form that can. Drop those rather than guess.
            let found: Vec<_> =
                found.into_iter().filter(|c| *c == Construct::PosixMode).collect();
            if found.is_empty() {
                Ok(tree)
            } else {
                Err(Refusal::Unsupported(Report::new(found)))
            }
        }
        // The parser refused a construct this pass owns. It reports one and
        // stops; the scan has the rest of the script, so the reader still gets
        // every finding.
        Err(e @ ParseError::Unsupported(_)) => {
            if found.is_empty() {
                Err(Refusal::Syntax(e))
            } else {
                Err(Refusal::Unsupported(Report::new(found)))
            }
        }
        Err(e) => Err(Refusal::Syntax(e)),
    }
}
