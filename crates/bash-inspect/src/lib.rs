//! The middle of three phases: parse, inspect, walk.
//!
//! bash-parser answers whether text is bash syntax and has no opinion beyond
//! that. This crate reads the tree it produced and reports which constructs
//! the walker cannot execute. bash-walker executes a tree. The three are
//! composed by the caller, which is what lets the caller refuse before any of
//! the script runs.
//!
//! This crate only reports. It does not decide, does not parse, and does not
//! execute: it takes a tree and returns findings, and an empty result is the
//! answer that nothing here is refused.
//!
//! Refusing before execution is the whole point. A construct refused part-way
//! through a walk has already let everything above it take effect, so the
//! caller approved one script and got part of one with no way to tell which
//! part (CLAUDE.md, "Unsupported constructs are refused by static inspection,
//! before anything runs").
//!
//! What a tree cannot show keeps its runtime refusal — computed text, `eval`,
//! `source`, aliases. The two are an addition, not a replacement.

use bash_parser::Command;

mod construct;
mod find;
mod report;

pub use construct::{Construct, Instead};
pub use report::{render, Finding};

/// Every construct in this tree that the walker cannot execute. Empty means
/// nothing here is refused, which is the caller's cue to run it.
pub fn inspect(tree: &Command) -> Vec<Finding> {
    find::find(tree).into_iter().map(|construct| Finding { construct }).collect()
}
