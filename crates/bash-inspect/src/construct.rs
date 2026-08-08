//! What inspection refuses, and what it owes the reader about each one.
//!
//! Three entries, not the whole unsupported list. The rest of that list lives
//! inline in the walker's builtins and expander as scattered runtime refusals;
//! putting both sides onto one registry is separate work.
//!
//! Only `set -o posix` is reachable today. `select` and `coproc` have no AST
//! node, so the parser refuses them before a tree exists and nothing can hand
//! one here. Their entries stay because the nodes are coming and this table is
//! what names a construct and says why it will not run; every word of them was
//! checked against bash 5.3 and re-deriving it would be waste.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Construct {
    PosixMode,
    Select,
    Coproc,
}

/// What to do instead. The reader is a Claude who cannot ask a follow-up, so
/// an empty answer reads as "you missed an option" — where nothing correct
/// exists, that is said outright rather than left blank.
pub enum Instead {
    Use(&'static str),
    NoEquivalent(&'static str),
}

impl Construct {
    /// As it is written in a script, so the reader can find it by eye.
    pub fn name(self) -> &'static str {
        match self {
            Construct::PosixMode => "set -o posix",
            Construct::Select => "select",
            Construct::Coproc => "coproc",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Construct::PosixMode => {
                "posix mode. It is a whole-shell mode, not a parser flag: bash \
                 consults it across expansion, execution, variables, jobs and \
                 twenty builtins, and none of that is implemented here"
            }
            Construct::Select => {
                "the select menu loop, which prints a numbered menu of its word \
                 list to stderr and reads a choice from stdin on each iteration"
            }
            Construct::Coproc => {
                "a coprocess: a command run asynchronously with its stdin and \
                 stdout on pipes, reachable through an array of two file \
                 descriptors named after it and a NAME_PID variable"
            }
        }
    }

    pub fn instead(self) -> Instead {
        match self {
            Construct::PosixMode => Instead::NoEquivalent(
                "there is no posix mode here to request, and no subset of one. \
                 Remove the option if the script does not depend on \
                 posix-specific behaviour; if it does depend on it, the script \
                 cannot run under this shell at all.",
            ),
            Construct::Select => Instead::NoEquivalent(
                "nothing here reproduces select. A `while read` loop over a \
                 menu you print yourself covers the common case, but it is not \
                 a drop-in replacement: select also sets REPLY, prompts with \
                 PS3, reprints the menu on an empty line, and lays the menu out \
                 in columns sized to the terminal.",
            ),
            Construct::Coproc => Instead::NoEquivalent(
                "backgrounding with `&` gives the asynchrony but not the pipes, \
                 and a pair of FIFOs from `mkfifo` gives the pipes but not the \
                 file-descriptor array, which nothing here can produce.",
            ),
        }
    }
}
