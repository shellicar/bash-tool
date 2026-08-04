//! A bash parser scoped to and validated against this project's own real
//! usage corpus (docs/ast-execution.md), not a full POSIX/bash
//! reimplementation. Grammar informed directly by reading GNU Bash 5.3's own
//! `parse.y`/`command.h` (`~/repos/gnu/bash`).
//!
//! Current scope (first working slice): simple commands, `&&`/`||`/`;`/`&`/
//! newline sequencing, `|` pipelines, the common redirect forms, subshells
//! `( )`, brace groups `{ ; }`, and deferred (opaque, unparsed) capture of
//! `$(...)`/`` `...` ``/`${...}`/`((...))` spans. Compound keyword commands
//! (`for`/`if`/`while`/`until`/`case`/`function`/`[[ ]]`) are recognized but
//! return `ParseError::Unsupported` — real, not silent, scope for this pass.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod pp;
pub mod shape;

pub use ast::*;
pub use parser::{parse, ParseError};
pub use pp::pretty;
pub use shape::widest_pipeline;
