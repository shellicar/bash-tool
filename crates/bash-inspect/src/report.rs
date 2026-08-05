//! What inspection hands back when it refuses, shaped by what its reader is:
//! a Claude who will redesign the script and cannot ask a follow-up
//! (CLAUDE.md, "What the output owes its reader").

use crate::construct::{Construct, Instead};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

impl Location {
    /// Columns are counted in characters, so a line holding multi-byte text
    /// does not report a column past its own width.
    fn of(source: &str, offset: usize) -> Location {
        let before = &source[..offset];
        let start = before.rfind('\n').map_or(0, |i| i + 1);
        Location { line: before.matches('\n').count() + 1, column: before[start..].chars().count() + 1 }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Finding {
    pub location: Location,
    pub construct: Construct,
}

#[derive(Debug, Clone)]
pub struct Report {
    findings: Vec<Finding>,
}

impl Report {
    pub(crate) fn new(source: &str, found: Vec<(Construct, usize)>) -> Report {
        let findings = found
            .into_iter()
            .map(|(construct, at)| Finding { location: Location::of(source, at), construct })
            .collect();
        Report { findings }
    }

    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// `shellcheck --format=gcc`'s shape: `origin:line:column: level: text`,
    /// an `error` line naming the construct and a `note` line saying what to
    /// do instead. The closing line says in words that nothing ran, because
    /// output that stops without saying so reads as success.
    pub fn render(&self, origin: &str) -> String {
        let mut out = String::new();
        for f in &self.findings {
            let at = format!("{origin}:{}:{}", f.location.line, f.location.column);
            out.push_str(&format!(
                "{at}: error: {}: this is {}\n",
                f.construct.name(),
                f.construct.summary()
            ));
            let note = match f.construct.instead() {
                Instead::Use(text) => format!("instead, {text}"),
                Instead::NoEquivalent(text) => format!("no equivalent: {text}"),
            };
            out.push_str(&format!("{at}: note: {note}\n"));
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
