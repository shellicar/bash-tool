//! Every expectation here was first put to bash 5.3 at ~/repos/gnu/bash/bash,
//! which decides what a construct is; the tests record its answers.

use bash_inspect::{inspect, render, Construct};

/// Inspection takes a tree, so a test states its script the way the tool does:
/// parse first, then inspect what came back.
fn findings(source: &str) -> Vec<Construct> {
    let tree = bash_parser::parse(source).expect("script should parse");
    inspect(&tree).iter().map(|f| f.construct).collect()
}

fn approved(source: &str) -> bool {
    findings(source).is_empty()
}

#[test]
fn an_ordinary_script_has_nothing_to_refuse() {
    assert!(approved("cd /tmp && ls -la | grep foo > out.txt"));
}

#[test]
fn set_o_posix_is_refused() {
    assert_eq!(findings("set -o posix"), [Construct::PosixMode]);
}

/// CLAUDE.md: a construct on a branch that would not have been taken is
/// refused too, because whether this run reaches the line is luck.
#[test]
fn a_construct_on_an_untaken_branch_is_still_refused() {
    assert_eq!(findings("if false; then set -o posix; fi"), [Construct::PosixMode]);
}

/// While a finding carries no location, a second occurrence of the same
/// construct gives the reader nothing to act on that the first did not.
#[test]
fn a_construct_that_occurs_twice_is_reported_once() {
    assert_eq!(findings("set -o posix\necho hi\nset -o posix\n"), [Construct::PosixMode]);
}

#[test]
fn posix_mode_is_found_through_every_spelling_bash_accepts() {
    // bash 5.3: each of these leaves `set -o | grep posix` reading "on".
    for script in [
        "set -xo posix",
        "set -ox posix",
        "set -o pipefail -o posix",
        "\"set\" -o posix",
        "set -o \"posix\"",
        "set -o po\"six\"",
    ] {
        assert_eq!(findings(script), [Construct::PosixMode], "script: {script:?}");
    }
}

/// bash 5.3 prints the option list for `set -o -x posix` and leaves posix off,
/// because the word after `-o` starts with a dash.
#[test]
fn an_option_name_bash_does_not_read_is_not_a_finding() {
    assert!(approved("set -o -x posix"));
    assert!(approved("set -- -o posix"));
    assert!(approved("set -o pipefail"));
    assert!(approved("set -e"));
}

/// Computed text keeps its runtime refusal; guessing at it here would refuse
/// scripts that never ask for posix mode.
#[test]
fn a_computed_option_name_is_left_to_the_runtime_refusal() {
    assert!(approved("set -o \"$mode\""));
    assert!(approved("set -o $mode"));
}

#[test]
fn a_word_that_merely_spells_a_construct_is_not_one() {
    assert!(approved("echo set -o posix"));
    assert!(approved("grep -r 'set -o posix' ."));
}

/// The parser captures a heredoc body verbatim and never tokenizes it, so a
/// tree walk cannot mistake one for commands.
#[test]
fn a_heredoc_body_is_text_not_commands() {
    assert!(approved("cat <<EOF\nset -o posix\nEOF\n"));
    assert!(approved("cat <<-'EOF'\n\tset -o posix\n\tEOF\n"));
}

#[test]
fn a_case_pattern_is_not_a_command() {
    assert!(approved("case $x in set) echo a;; esac"));
    assert!(approved("case $x in (set) echo a;; esac"));
}

#[test]
fn an_assignment_prefix_does_not_hide_the_command_word() {
    assert_eq!(findings("FOO=bar set -o posix"), [Construct::PosixMode]);
}

/// Every branch of the tree is walked, not just the top level.
#[test]
fn a_construct_is_found_wherever_the_tree_holds_it() {
    for script in [
        "echo a | set -o posix",
        "(set -o posix)",
        "{ set -o posix; }",
        "f() { set -o posix; }",
        "while true; do set -o posix; done",
        "until set -o posix; do :; done",
        "for f in a b; do set -o posix; done",
        "case $x in a) set -o posix;; esac",
        "! set -o posix",
        "time set -o posix",
        "set -o posix &",
        "{ set -o posix; } 2>/dev/null",
        "if :; then :; else set -o posix; fi",
    ] {
        assert_eq!(findings(script), [Construct::PosixMode], "script: {script:?}");
    }
}

/// CLAUDE.md: absence of output reads as success, so the rendered findings say
/// in words that nothing ran, and name each construct and what to do instead.
#[test]
fn the_rendering_tells_its_reader_what_and_that_nothing_ran() {
    let tree = bash_parser::parse("echo hi\nset -o posix\n").unwrap();
    let text = render(&inspect(&tree), "script.sh");

    assert!(text.contains("script.sh: error: set -o posix:"), "{text}");
    assert!(text.contains("script.sh: note: no equivalent:"), "{text}");
    assert!(text.contains("Nothing ran"), "{text}");
    assert!(text.contains("1 construct cannot be executed"), "{text}");
}

/// A tripwire, not a wish. `select` and `coproc` have no AST node, so the
/// parser refuses them before a tree exists and inspection never sees them.
/// When the nodes land this test fails, which is the reminder that the
/// construct table already holds their text and wants wiring to the new nodes.
#[test]
fn select_and_coproc_do_not_reach_inspection_yet() {
    for script in ["select x in a b; do echo $x; done", "coproc cat"] {
        assert!(
            bash_parser::parse(script).is_err(),
            "{script:?} parses now: wire its node into bash-inspect's tree walk"
        );
    }
}
