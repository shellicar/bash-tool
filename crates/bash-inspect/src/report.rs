//! What inspection hands back when it refuses, shaped by what its reader is:
//! a Claude who will redesign the script and cannot ask a follow-up
//! (CLAUDE.md, "What the output owes its reader").
//!
//! No line and column yet. Positions belong on the AST, which is the parser
//! crate's work and not done, and a location guessed at from the text would be
//! wrong often enough to send a reader to the wrong line.

use crate::construct::{Construct, Instead};

#[derive(Debug, Clone, Copy)]
pub struct Finding {
    pub construct: Construct,
}

#[derive(Debug, Clone)]
pub struct Report {
    findings: Vec<Finding>,
}

impl Report {
    pub(crate) fn new(found: Vec<Construct>) -> Report {
        Report { findings: found.into_iter().map(|construct| Finding { construct }).collect() }
    }

    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Compiler-style, minus the position it has no source for: an `error`
    /// line naming the construct and a `note` line saying what to do instead,
    /// both prefixed with what was inspected. The closing line says in words
    /// that nothing ran, because output that stops without saying so reads as
    /// success.
    pub fn render(&self, origin: &str) -> String {
        let mut out = String::new();
        for f in &self.findings {
            out.push_str(&format!(
                "{origin}: error: {}: this is {}\n",
                f.construct.name(),
                f.construct.summary()
            ));
            let note = match f.construct.instead() {
                Instead::Use(text) => format!("instead, {text}"),
                Instead::NoEquivalent(text) => format!("no equivalent: {text}"),
            };
            out.push_str(&format!("{origin}: note: {note}\n"));
        }
        let n = self.findings.len();
        let plural = if n == 1 { "construct" } else { "constructs" };
        out.push_str(&format!(
            "\nbash-inspect: {n} {plural} cannot be executed. Nothing ran: inspection \
             finishes before execution begins, so no part of this script has taken effect.\n"
        ));
        out
    }
}
