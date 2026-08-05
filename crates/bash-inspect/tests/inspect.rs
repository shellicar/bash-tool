//! Every expectation here was first put to bash 5.3 at ~/repos/gnu/bash/bash,
//! which decides what a construct is; the tests record its answers.

use bash_inspect::{inspect, Construct, Refusal};

fn refusal(source: &str) -> Vec<(Construct, usize, usize)> {
    match inspect(source) {
        Err(Refusal::Unsupported(report)) => report
            .findings()
            .iter()
            .map(|f| (f.construct, f.location.line, f.location.column))
            .collect(),
        Err(Refusal::Syntax(e)) => panic!("expected a refusal, got a syntax error: {e}"),
        Ok(_) => Vec::new(),
    }
}

fn approved(source: &str) -> bool {
    inspect(source).is_ok()
}

#[test]
fn an_ordinary_script_is_approved_and_hands_back_its_tree() {
    assert!(approved("cd /tmp && ls -la | grep foo > out.txt"));
}

#[test]
fn set_o_posix_is_refused_at_the_option_that_asks_for_it() {
    assert_eq!(refusal("set -o posix"), [(Construct::PosixMode, 1, 5)]);
}

#[test]
fn select_is_refused_where_the_keyword_stands() {
    assert_eq!(refusal("select x in a b; do echo $x; done"), [(Construct::Select, 1, 1)]);
}

#[test]
fn coproc_is_refused_where_the_keyword_stands() {
    assert_eq!(refusal("coproc cat"), [(Construct::Coproc, 1, 1)]);
}

/// The reader redesigns rather than retries, so a report holding one finding
/// out of three costs a second redesign. The parser stops at its first
/// refusal; inspection must not.
#[test]
fn every_finding_is_reported_not_the_first() {
    let script = "echo start\nset -o posix\nselect x in a; do echo $x; done\ncoproc cat\n";
    assert_eq!(
        refusal(script),
        [(Construct::PosixMode, 2, 5), (Construct::Select, 3, 1), (Construct::Coproc, 4, 1)]
    );
}

/// CLAUDE.md: a construct on a branch that would not have been taken is
/// refused too, because whether this run reaches the line is luck.
#[test]
fn a_construct_on_an_untaken_branch_is_still_refused() {
    assert_eq!(refusal("if false; then set -o posix; fi"), [(Construct::PosixMode, 1, 20)]);
}

#[test]
fn posix_mode_is_found_through_every_spelling_bash_accepts() {
    // bash 5.3: each of these leaves `set -o | grep posix` reading "on".
    assert_eq!(refusal("set -xo posix").len(), 1);
    assert_eq!(refusal("set -ox posix").len(), 1);
    assert_eq!(refusal("set -o pipefail -o posix").len(), 1);
    assert_eq!(refusal("\"set\" -o posix").len(), 1);
    assert_eq!(refusal("set -o \"posix\"").len(), 1);
    assert_eq!(refusal("set -o po\"six\"").len(), 1);
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
    assert!(approved("echo select"));
    assert!(approved("echo coproc"));
    assert!(approved("grep -r 'set -o posix' ."));
}

/// bash: `"select" x in a b; do ...` is a syntax error near `do`, and
/// `"coproc" cat` reports "coproc: command not found" — quoting takes away
/// reserved-word status.
#[test]
fn a_quoted_keyword_is_an_ordinary_command_word() {
    assert!(approved("\"coproc\" cat"));
}

#[test]
fn a_comment_is_not_a_command() {
    assert!(approved("# set -o posix\necho hi"));
}

/// The scan drives the parser's lexer and registers heredoc delimiters the way
/// the parser does. Without that the body comes back as word tokens and every
/// line of it reads as a command.
#[test]
fn a_heredoc_body_is_text_not_commands() {
    assert!(approved("cat <<EOF\nset -o posix\nselect\ncoproc\nEOF\n"));
    assert!(approved("cat <<-'EOF'\n\tset -o posix\n\tEOF\n"));
}

#[test]
fn a_case_pattern_written_with_a_paren_is_not_command_position() {
    assert!(approved("case $x in (select) echo a;; (coproc) echo b;; esac"));
    assert!(approved("case $x in select) echo a;; esac"));
}

#[test]
fn an_assignment_prefix_does_not_hide_the_command_word() {
    assert_eq!(refusal("FOO=bar set -o posix"), [(Construct::PosixMode, 1, 13)]);
}

#[test]
fn a_construct_inside_a_pipeline_or_a_subshell_is_found() {
    assert_eq!(refusal("echo a | set -o posix"), [(Construct::PosixMode, 1, 14)]);
    assert_eq!(refusal("(set -o posix)"), [(Construct::PosixMode, 1, 6)]);
    assert_eq!(refusal("f() { set -o posix; }"), [(Construct::PosixMode, 1, 11)]);
    assert_eq!(refusal("while true; do set -o posix; done"), [(Construct::PosixMode, 1, 20)]);
}

#[test]
fn text_that_is_not_bash_syntax_stays_the_parsers_answer() {
    assert!(matches!(inspect("echo 'unterminated"), Err(Refusal::Syntax(_))));
}

/// CLAUDE.md: absence of output reads as success, so the report says in words
/// that nothing ran, names each construct, and gives a location for each.
#[test]
fn the_report_tells_its_reader_where_what_and_that_nothing_ran() {
    let Err(Refusal::Unsupported(report)) = inspect("echo hi\nselect x in a; do :; done\n") else {
        panic!("expected a refusal");
    };
    let text = report.render("script.sh");
    assert!(text.contains("script.sh:2:1: error: select:"), "{text}");
    assert!(text.contains("script.sh:2:1: note: no equivalent:"), "{text}");
    assert!(text.contains("Nothing ran"), "{text}");
    assert!(text.contains("1 construct cannot be executed"), "{text}");
}

#[test]
fn the_report_counts_findings_in_the_plural() {
    let Err(Refusal::Unsupported(report)) = inspect("set -o posix\ncoproc cat\n") else {
        panic!("expected a refusal");
    };
    assert!(report.render("-").contains("2 constructs cannot be executed"));
}

/// The scan works out command position from the token stream, which is a claim
/// about grammar the parser also makes. Where a script parses, the parser's
/// tree is the authority: walk it, count the `set` commands that ask for posix
/// mode, and hold the scan to the same number.
#[test]
fn command_position_agrees_with_the_parsers_own_tree() {
    let scripts = [
        "set -o posix",
        "echo set -o posix",
        "if false; then set -o posix; else set -o posix; fi",
        "FOO=1 set -o posix; echo set",
        "case $x in a) set -o posix;; b) echo set -o posix;; esac",
        "for f in a b; do set -o posix; done",
        "cat <<EOF\nset -o posix\nEOF",
        "! set -o posix",
        "time set -o posix",
        "{ set -o posix; } 2>/dev/null",
        "echo a > posix; set -o posix",
        "set -o pipefail; set -o posix; set -e",
    ];
    for script in scripts {
        let tree = bash_parser::parse(script).expect("script should parse");
        let expected = count_posix_in_tree(&tree);
        let actual = refusal(script).len();
        assert_eq!(actual, expected, "script: {script:?}");
    }
}

fn count_posix_in_tree(cmd: &bash_parser::Command) -> usize {
    use bash_parser::Command as C;
    match cmd {
        C::Simple(s) => {
            let is_set = s.program.as_ref().is_some_and(|p| p.text.trim_matches('"') == "set");
            let asks = s.args.windows(2).any(|w| {
                let flag = w[0].text.trim_matches('"');
                (flag.starts_with('-') || flag.starts_with('+'))
                    && flag != "--"
                    && flag[1..].contains('o')
                    && w[1].text.trim_matches('"') == "posix"
            });
            usize::from(is_set && asks)
        }
        C::Connection(c) => count_posix_in_tree(&c.left) + count_posix_in_tree(&c.right),
        C::Invert(c) | C::Time(c) | C::Background(c) | C::Subshell(c) | C::Group(c) => {
            count_posix_in_tree(c)
        }
        C::Redirected { command, .. } => count_posix_in_tree(command),
        C::FunctionDef { body, .. } => count_posix_in_tree(body),
        C::For(f) => count_posix_in_tree(&f.body),
        C::ArithFor { body, .. } => count_posix_in_tree(body),
        C::While { cond, body } | C::Until { cond, body } => {
            count_posix_in_tree(cond) + count_posix_in_tree(body)
        }
        C::If(i) => {
            i.branches.iter().map(|(c, b)| count_posix_in_tree(c) + count_posix_in_tree(b)).sum::<usize>()
                + i.else_branch.as_ref().map_or(0, |b| count_posix_in_tree(b))
        }
        C::Case(c) => c.arms.iter().filter_map(|a| a.body.as_ref()).map(|b| count_posix_in_tree(b)).sum(),
        C::Cond(_) | C::Arith { .. } => 0,
    }
}
