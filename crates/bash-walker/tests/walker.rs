//! End-to-end behaviour: real scripts through parse → walk → real
//! subprocesses, asserting on the combined output and status — never on how
//! the walker got there.
//!
//! The walker's cwd and exported env are process-global by design (one
//! walker process per invocation in production), so every test serialises
//! on one mutex and restores cwd afterwards.

use std::sync::{Mutex, MutexGuard, OnceLock};

use bash_walker::ShellState;

fn lock() -> MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Run one script in a fresh state, cwd restored afterwards.
fn run(src: &str) -> (String, i32) {
    let _guard = lock();
    let saved = std::env::current_dir().unwrap();
    let mut state = ShellState::default();
    let r = bash_walker::run(src, &mut state);
    std::env::set_current_dir(saved).unwrap();
    r
}

/// Run scripts sequentially against ONE state — the persistence story.
fn run_session(scripts: &[&str]) -> Vec<(String, i32)> {
    let _guard = lock();
    let saved = std::env::current_dir().unwrap();
    let mut state = ShellState::default();
    let out = scripts
        .iter()
        .map(|s| bash_walker::run(s, &mut state))
        .collect();
    std::env::set_current_dir(saved).unwrap();
    out
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bw-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn echoes_to_the_combined_output() {
    let expected = ("hello\n".to_string(), 0);

    let actual = run("echo hello");

    assert_eq!(actual, expected);
}

#[test]
fn pipeline_feeds_stdout_to_stdin() {
    let (output, status) = run("printf 'a\\nb\\nc\\n' | wc -l");

    assert_eq!(status, 0);
    assert_eq!(output.trim(), "3");
}

#[test]
fn and_short_circuits_on_failure() {
    let expected = (String::new(), 1);

    let actual = run("false && echo no");

    assert_eq!(actual, expected);
}

#[test]
fn or_runs_the_fallback() {
    let (output, status) = run("false || echo fallback");

    assert_eq!(status, 0);
    assert_eq!(output, "fallback\n");
}

#[test]
fn cwd_persists_across_invocations() {
    let results = run_session(&["cd /tmp", "pwd"]);

    assert_eq!(results[0].1, 0);
    let expected = std::fs::canonicalize("/tmp").unwrap();
    let actual = results[1].0.trim().to_string();
    assert_eq!(actual, expected.to_string_lossy());
}

#[test]
fn variables_persist_across_invocations() {
    let results = run_session(&["x=41", "echo $((x+1))"]);

    let expected = "42\n";
    let actual = &results[1].0;
    assert_eq!(actual, expected);
}

#[test]
fn exported_variable_reaches_a_child_process() {
    let (output, status) = run("export BW_TEST_EXPORT_1=carried; printenv BW_TEST_EXPORT_1");

    assert_eq!(status, 0);
    assert_eq!(output, "carried\n");
}

#[test]
fn command_substitution_captures_stdout() {
    let (output, _) = run("echo got:$(echo inner)");

    let expected = "got:inner\n";
    assert_eq!(output, expected);
}

#[test]
fn command_substitution_failure_sets_status() {
    let (output, _) = run("x=$(false); echo $?");

    let expected = "1\n";
    assert_eq!(output, expected);
}

#[test]
fn redirect_writes_and_reads_a_file() {
    let d = temp_dir("redir");
    let (output, status) = run(&format!(
        "cd {} && echo content > out.txt && cat out.txt",
        d.display()
    ));

    assert_eq!(status, 0);
    assert_eq!(output, "content\n");
}

#[test]
fn stderr_redirect_to_dev_null_discards_it() {
    let (output, status) = run("ls /definitely-does-not-exist-bw 2>/dev/null");

    assert_ne!(status, 0);
    assert_eq!(output, "");
}

#[test]
fn two_streams_interleave_into_combined_output() {
    let (output, status) = run("echo out; echo err 1>&2");

    assert_eq!(status, 0);
    assert_eq!(output, "out\nerr\n");
}

#[test]
fn heredoc_body_expands_variables() {
    let (output, _) = run("x=world\ncat <<EOF\nhello $x\nEOF");

    let expected = "hello world\n";
    assert_eq!(output, expected);
}

#[test]
fn quoted_heredoc_delimiter_suppresses_expansion() {
    let (output, _) = run("x=world\ncat <<'EOF'\nhello $x\nEOF");

    let expected = "hello $x\n";
    assert_eq!(output, expected);
}

#[test]
fn for_loop_iterates_the_word_list() {
    let (output, _) = run("for i in 1 2 3; do echo $i; done");

    let expected = "1\n2\n3\n";
    assert_eq!(output, expected);
}

#[test]
fn while_loop_with_arithmetic_condition_and_increment() {
    let (output, _) = run("i=0; while ((i<3)); do echo $i; ((i++)); done");

    let expected = "0\n1\n2\n";
    assert_eq!(output, expected);
}

#[test]
fn break_leaves_the_loop() {
    let (output, _) = run("for i in 1 2 3; do [ $i = 2 ] && break; echo $i; done");

    let expected = "1\n";
    assert_eq!(output, expected);
}

#[test]
fn if_with_cond_command() {
    let (output, _) = run("if [[ -d /tmp && -n x ]]; then echo yes; else echo no; fi");

    let expected = "yes\n";
    assert_eq!(output, expected);
}

#[test]
fn case_matches_a_glob_pattern() {
    let (output, _) = run("case abc.txt in *.txt) echo text;; *) echo other;; esac");

    let expected = "text\n";
    assert_eq!(output, expected);
}

#[test]
fn function_call_with_arguments_local_and_return() {
    let (output, status) = run("f() { local msg=$1; echo \"got $msg\"; return 3; }; f hello; echo $?");

    assert_eq!(status, 0);
    assert_eq!(output, "got hello\n3\n");
}

#[test]
fn local_does_not_leak_out_of_the_function() {
    let (output, _) = run("x=outer; f() { local x=inner; }; f; echo $x");

    let expected = "outer\n";
    assert_eq!(output, expected);
}

#[test]
fn glob_expands_sorted_matches() {
    let d = temp_dir("glob");
    for name in ["b.log", "a.log", "c.txt"] {
        std::fs::write(d.join(name), "").unwrap();
    }
    let (output, _) = run(&format!("cd {} && echo *.log", d.display()));

    let expected = "a.log b.log\n";
    assert_eq!(output, expected);
}

#[test]
fn glob_with_no_match_stays_literal() {
    let d = temp_dir("noglob");
    let (output, _) = run(&format!("cd {} && echo *.nope", d.display()));

    let expected = "*.nope\n";
    assert_eq!(output, expected);
}

#[test]
fn exit_status_propagates() {
    let (_, status) = run("exit 7");

    assert_eq!(status, 7);
}

#[test]
fn errexit_stops_at_the_first_failure() {
    let (output, status) = run("set -e\nfalse\necho unreachable");

    assert_eq!(status, 1);
    assert!(!output.contains("unreachable"), "output: {output:?}");
}

#[test]
fn errexit_ignores_a_tested_failure() {
    let (output, status) = run("set -e\nfalse || true\necho survived");

    assert_eq!(status, 0);
    assert!(output.contains("survived"));
}

#[test]
fn subshell_cd_does_not_leak() {
    let (output, status) = run("(cd / && pwd); echo still-here");

    assert_eq!(status, 0);
    assert!(output.starts_with("/\n"), "output: {output:?}");
    assert!(output.contains("still-here"));
}

#[test]
fn background_command_does_not_block() {
    let start = std::time::Instant::now();
    let (output, status) = run("sleep 2 & echo immediate");

    assert_eq!(status, 0);
    assert!(output.contains("immediate"));
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "background sleep blocked the walker"
    );
}

#[test]
fn last_status_is_visible_in_dollar_question() {
    let (output, _) = run("false; echo $?");

    let expected = "1\n";
    assert_eq!(output, expected);
}

#[test]
fn brace_expansion_produces_the_sequence() {
    let (output, _) = run("echo {1..3} {a,c}x");

    let expected = "1 2 3 ax cx\n";
    assert_eq!(output, expected);
}

#[test]
fn tilde_expands_to_home() {
    let (output, _) = run("echo ~");

    let expected = format!("{}\n", std::env::var("HOME").unwrap());
    assert_eq!(output, expected);
}

#[test]
fn unsupported_builtin_fails_loudly_by_name() {
    let (output, status) = run("declare -A assoc");

    assert_ne!(status, 0);
    assert!(
        output.contains("not supported by bash-walker"),
        "output: {output:?}"
    );
}

#[test]
fn parameter_suffix_strip() {
    let (output, _) = run("v=hello.txt; echo ${v%.txt}");

    let expected = "hello\n";
    assert_eq!(output, expected);
}

#[test]
fn parameter_default_applies_when_unset() {
    let (output, _) = run("echo ${bw_unset_thing:-fallback}");

    let expected = "fallback\n";
    assert_eq!(output, expected);
}

#[test]
fn unquoted_expansion_splits_quoted_does_not() {
    let (output, _) = run("x='a b'; printf '[%s]' $x; printf '[%s]' \"$x\"");

    let expected = "[a][b][a b]";
    assert_eq!(output, expected);
}

#[test]
fn pipeline_stage_is_a_subshell_like_bash() {
    // The classic: `| read` cannot set a variable in the parent shell.
    let (output, _) = run("echo hi | read y; echo ${y:-empty}");

    let expected = "empty\n";
    assert_eq!(output, expected);
}

#[test]
fn while_read_consumes_piped_lines() {
    let (output, _) = run("printf 'a\\nb\\n' | while read l; do echo got $l; done");

    let expected = "got a\ngot b\n";
    assert_eq!(output, expected);
}

#[test]
fn regex_match_fills_bash_rematch() {
    let (output, _) = run("[[ abc123 =~ ([0-9]+) ]] && echo ${BASH_REMATCH[1]}");

    let expected = "123\n";
    assert_eq!(output, expected);
}

#[test]
fn process_substitution_reads_like_a_file() {
    let (output, status) = run("cat <(echo from-procsub)");

    assert_eq!(status, 0);
    assert_eq!(output, "from-procsub\n");
}

#[test]
fn command_not_found_is_127() {
    let (output, status) = run("definitely-not-a-command-bw-2026");

    assert_eq!(status, 127);
    assert!(output.contains("command not found"));
}

#[test]
fn arithmetic_command_status_reflects_the_value() {
    let (_, zero_is_false) = run("((0))");
    let (_, nonzero_is_true) = run("((3))");

    assert_eq!(zero_is_false, 1);
    assert_eq!(nonzero_is_true, 0);
}

#[test]
fn c_style_for_loop_counts() {
    let (output, _) = run("for ((i=0; i<3; i++)); do echo $i; done");

    let expected = "0\n1\n2\n";
    assert_eq!(output, expected);
}

#[test]
fn eval_runs_its_arguments_in_the_current_shell() {
    let (output, _) = run("eval 'x=fromeval'; echo $x");

    let expected = "fromeval\n";
    assert_eq!(output, expected);
}

#[test]
fn command_v_finds_a_program_on_path() {
    let (output, status) = run("command -v ls");

    assert_eq!(status, 0);
    assert!(output.trim_end().ends_with("/ls"), "output: {output:?}");
}

#[test]
fn multibyte_utf8_survives_the_pipeline() {
    // Real corpus mismatch: ✓ and → came out as mojibake because lexer and
    // expander pushed bytes as chars.
    let (output, _) = run("echo \"✓ done → next\" && echo '✓ quoted'");

    let expected = "✓ done → next\n✓ quoted\n";
    assert_eq!(output, expected);
}

#[test]
fn failed_redirect_fails_the_command_but_not_the_script() {
    // bash prints the error, sets $? to 1, and continues; aborting the whole
    // invocation was a real divergence the differential replay caught.
    let (output, status) = run("echo x > /nonexistent-dir-bw/f; echo after $?");

    assert_eq!(status, 0);
    assert!(output.contains("No such file or directory"), "output: {output:?}");
    assert!(output.contains("after 1"), "output: {output:?}");
}

#[test]
fn printf_is_native_with_bash_number_and_padding_rules() {
    let (output, status) = run("printf '%05d|%-4s|%x|%s\\n' 42 ab 255 end");

    assert_eq!(status, 0);
    assert_eq!(output, "00042|ab  |ff|end\n");
}

#[test]
fn printf_reuses_the_format_until_arguments_are_exhausted() {
    let (output, _) = run("printf '[%s]' a b c");

    let expected = "[a][b][c]";
    assert_eq!(output, expected);
}

#[test]
fn printf_dash_prefixed_format_is_an_invalid_option() {
    // bash builtin behaviour: rc 2 and the `--` diagnostic — the external
    // BSD printf says something else entirely.
    let (output, status) = run("printf '--- a/file.txt\\n'");

    assert_eq!(status, 2);
    assert!(output.contains("invalid option"), "output: {output:?}");
}

#[test]
fn backslash_alternation_in_double_quotes_expands_and_terminates() {
    // `grep "a\|b"` hung the walker forever: the dquote expander had no arm
    // for backslash-before-ordinary-char and never advanced. Found live in
    // the first frozen-set run; bash ran the same command in 0.1s.
    let (output, status) = run("printf 'alpha\\nbeta\\n' | grep \"alpha\\|gamma\"");

    assert_eq!(status, 0);
    assert_eq!(output, "alpha\n");
}

#[test]
fn backslash_in_heredoc_body_expands_and_terminates() {
    let (output, status) = run("cat <<EOF\na\\|b\nEOF");

    assert_eq!(status, 0);
    assert_eq!(output, "a\\|b\n");
}

#[test]
fn syntax_error_reports_status_2() {
    let (output, status) = run("if true; then");

    assert_eq!(status, 2);
    assert!(output.contains("syntax error"), "output: {output:?}");
}
