//! Structural facts about a parsed command that a comparator needs.
//!
//! The only place a shell starts things concurrently is a pipeline, so the
//! only trace lines that may legitimately appear out of order are one
//! pipeline's stages. Everything else is sequenced, and a difference in
//! sequence is a difference in program.

use crate::ast::{CaseCommand, Command, Connection, Connector, ForCommand, IfCommand};

/// How many stages the widest pipeline in this command has, and so how many
/// adjacent trace lines a comparator may accept in any order. 1 means no
/// pipeline, so nothing may permute.
pub fn widest_pipeline(cmd: &Command) -> usize {
    match cmd {
        Command::Connection(Connection { left, right, connector }) => {
            if *connector == Connector::Pipe {
                // A pipeline is a left-leaning chain, so its width is the
                // number of leaves along it, not the deeper of two branches.
                stages(cmd)
            } else {
                widest_pipeline(left).max(widest_pipeline(right))
            }
        }
        Command::Invert(i)
        | Command::Time(i)
        | Command::Background(i)
        | Command::Subshell(i)
        | Command::Group(i) => widest_pipeline(i),
        Command::Redirected { command, .. } => widest_pipeline(command),
        Command::For(ForCommand { body, .. }) => widest_pipeline(body),
        Command::ArithFor { body, .. } => widest_pipeline(body),
        Command::While { cond, body } | Command::Until { cond, body } => {
            widest_pipeline(cond).max(widest_pipeline(body))
        }
        Command::If(IfCommand { branches, else_branch }) => {
            let mut w = 1;
            for (c, b) in branches {
                w = w.max(widest_pipeline(c)).max(widest_pipeline(b));
            }
            if let Some(e) = else_branch {
                w = w.max(widest_pipeline(e));
            }
            w
        }
        Command::Case(CaseCommand { arms, .. }) => {
            let mut w = 1;
            for a in arms {
                if let Some(b) = &a.body {
                    w = w.max(widest_pipeline(b));
                }
            }
            w
        }
        Command::FunctionDef { body, .. } => widest_pipeline(body),
        Command::Simple(_) | Command::Cond(_) | Command::Arith { .. } => 1,
    }
}

fn stages(cmd: &Command) -> usize {
    match cmd {
        Command::Connection(Connection { left, right, connector }) if *connector == Connector::Pipe => {
            stages(left) + stages(right)
        }
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width(src: &str) -> usize {
        widest_pipeline(&crate::parse(src).unwrap())
    }

    #[test]
    fn a_simple_command_has_no_pipeline() {
        let expected = 1;

        let actual = width("echo one");

        assert_eq!(actual, expected);
    }

    #[test]
    fn a_three_stage_pipeline_is_three_wide() {
        let expected = 3;

        let actual = width("a | b | c");

        assert_eq!(actual, expected);
    }

    #[test]
    fn the_widest_pipeline_wins_over_the_sequence_around_it() {
        let expected = 2;

        let actual = width("echo hi; ls | wc -l");

        assert_eq!(actual, expected);
    }

    #[test]
    fn a_pipeline_inside_a_loop_body_still_counts() {
        let expected = 4;

        let actual = width("for i in 1 2; do a | b | c | d; done");

        assert_eq!(actual, expected);
    }

    #[test]
    fn a_pipe_inside_a_heredoc_body_is_not_a_pipeline() {
        let expected = 1;

        let actual = width("cat <<'EOF'\nx | y | z\nEOF");

        assert_eq!(actual, expected);
    }
}
