//! End-to-end behaviour: real scripts through parse → walk → real
//! subprocesses, asserting on the combined output and status — never on how
//! the walker got there.
//!
//! Cwd and env are shell STATE, not process state, so tests run in
//! parallel with no mutex and no cwd restoration — that this file needs
//! neither is itself evidence the process-global edges are gone.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use bash_walker::clock::{Clock, CpuTimes, Entropy};
use bash_walker::ShellState;

fn init() {
    static ONCE: Once = Once::new();
    // Process substitution re-execs the walker binary; under cargo test the
    // current exe is the harness, so name the real binary explicitly.
    // SAFETY: guarded by Once, before any test spawns children.
    ONCE.call_once(|| unsafe {
        std::env::set_var("BASH_WALKER_SELF", env!("CARGO_BIN_EXE_bash-walker"))
    });
}

/// Run one script in a fresh state.
fn run(src: &str) -> (String, i32) {
    init();
    let mut state = ShellState::default();
    bash_walker::run(src, &mut state)
}

/// A fake clock the test scripts by hand — `time` reads it instead of the
/// real clock, so timing output is asserted exactly, never by luck.
struct ScriptedClock {
    wall: Mutex<VecDeque<Duration>>,
    cpu: Mutex<VecDeque<CpuTimes>>,
}

impl Clock for ScriptedClock {
    fn now_monotonic(&self) -> Duration {
        self.wall.lock().unwrap().pop_front().expect("scripted wall reading")
    }
    fn cpu_times(&self) -> CpuTimes {
        self.cpu.lock().unwrap().pop_front().expect("scripted cpu reading")
    }
}

fn run_with_scripted_clock(
    src: &str,
    wall: Vec<Duration>,
    cpu: Vec<CpuTimes>,
) -> (String, i32) {
    init();
    let clock = Arc::new(ScriptedClock {
        wall: Mutex::new(wall.into()),
        cpu: Mutex::new(cpu.into()),
    });
    let mut state = ShellState::default();
    bash_walker::run_with_clock(src, &mut state, clock)
}

/// A scripted `$RANDOM` source.
struct ScriptedEntropy(Mutex<VecDeque<u16>>);

impl Entropy for ScriptedEntropy {
    fn next_random(&self) -> u16 {
        self.0.lock().unwrap().pop_front().expect("scripted random value")
    }
}

fn run_with_scripted_entropy(src: &str, values: Vec<u16>) -> (String, i32) {
    init();
    let entropy = Arc::new(ScriptedEntropy(Mutex::new(values.into())));
    let mut state = ShellState::default();
    bash_walker::run_with(
        src,
        &mut state,
        Arc::new(bash_walker::clock::RealClock::default()),
        entropy,
    )
}

/// Run scripts sequentially against ONE state — the persistence story.
fn run_session(scripts: &[&str]) -> Vec<(String, i32)> {
    init();
    let mut state = ShellState::default();
    scripts
        .iter()
        .map(|s| bash_walker::run(s, &mut state))
        .collect()
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
    // Logical cwd, like bash: `cd /tmp; pwd` prints /tmp, not the
    // symlink-resolved /private/tmp a physical chdir would report.
    let expected = "/tmp";
    let actual = results[1].0.trim().to_string();
    assert_eq!(actual, expected);
}

#[test]
fn cd_is_shell_state_and_never_touches_the_process_cwd() {
    let before = std::env::current_dir().unwrap();

    let (output, status) = run("cd / && pwd");

    assert_eq!(status, 0);
    assert_eq!(output, "/\n");
    let after = std::env::current_dir().unwrap();
    assert_eq!(after, before);
}

#[test]
fn cd_normalizes_dot_dot_logically() {
    let (output, _) = run("cd /usr/bin && cd ../lib && pwd");

    let expected = "/usr/lib\n";
    assert_eq!(output, expected);
}

#[test]
fn random_comes_from_the_scripted_entropy_source() {
    let expected = ("12345 671\n".to_string(), 0);

    let actual = run_with_scripted_entropy("echo $RANDOM $RANDOM", vec![12345, 671]);

    assert_eq!(actual, expected);
}

#[test]
fn unset_environment_variable_does_not_reach_a_child() {
    let (output, status) = run("export BW_UNSET_CHECK=here; unset BW_UNSET_CHECK; printenv BW_UNSET_CHECK; echo rc=$?");

    assert_eq!(status, 0);
    assert_eq!(output, "rc=1\n");
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
fn time_reports_scripted_wall_and_cpu_intervals() {
    let wall = vec![Duration::ZERO, Duration::from_millis(1234)];
    let cpu = vec![
        CpuTimes::default(),
        CpuTimes { user: Duration::from_millis(500), sys: Duration::from_millis(250) },
    ];

    let expected = ("\nreal\t0m1.234s\nuser\t0m0.500s\nsys\t0m0.250s\n".to_string(), 0);

    let actual = run_with_scripted_clock("time true", wall, cpu);

    assert_eq!(actual, expected);
}

#[test]
fn time_reports_minutes_past_sixty_seconds() {
    let wall = vec![Duration::ZERO, Duration::from_millis(75_500)];
    let cpu = vec![CpuTimes::default(), CpuTimes::default()];

    let (output, _) = run_with_scripted_clock("time true", wall, cpu);

    let expected = "\nreal\t1m15.500s\nuser\t0m0.000s\nsys\t0m0.000s\n";
    assert_eq!(output, expected);
}

#[test]
fn exec_redirect_rewires_the_shell_for_later_commands() {
    let (output, status) = run("exec 2>/dev/null\nls /definitely-does-not-exist-bw\necho after");

    assert_eq!(status, 0);
    assert_eq!(output, "after\n");
}

#[test]
fn exec_opens_fd_three_and_a_later_dup_writes_through_it() {
    let (output, status) = run("exec 3>&1 && echo through-three >&3");

    assert_eq!(status, 0);
    assert_eq!(output, "through-three\n");
}

#[test]
fn exec_in_a_subshell_does_not_leak_out() {
    let (output, status) = run("(exec 1>/dev/null; echo hidden); echo visible");

    assert_eq!(status, 0);
    assert_eq!(output, "visible\n");
}

#[test]
fn exec_with_a_command_replaces_the_shell() {
    let (output, status) = run("exec echo replaced\necho never-runs");

    assert_eq!(status, 0);
    assert_eq!(output, "replaced\n");
}

#[test]
fn exec_command_not_found_exits_127() {
    let (_, status) = run("exec definitely-not-a-command-bw-2026");

    assert_eq!(status, 127);
}

#[test]
fn fd_three_redirect_reaches_a_child_process() {
    let d = temp_dir("fd3");
    let f = d.join("three.txt");
    let (_, status) = run(&format!("sh -c 'echo from-child >&3' 3>{}", f.display()));

    assert_eq!(status, 0);
    let expected = "from-child\n";
    let actual = std::fs::read_to_string(&f).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn dup_of_an_unopened_fd_fails_the_command_not_the_script() {
    let (output, status) = run("echo x >&7; echo after $?");

    assert_eq!(status, 0);
    assert!(output.contains("Bad file descriptor"), "output: {output:?}");
    assert!(output.contains("after 1"), "output: {output:?}");
}

#[test]
fn process_substitution_streams_to_an_early_exiting_consumer() {
    // A temp-file implementation would try to materialise a billion lines;
    // a real FIFO lets head take one line and the producer die on SIGPIPE.
    let (output, status) = run("head -1 <(seq 1 1000000000)");

    assert_eq!(status, 0);
    assert_eq!(output, "1\n");
}

#[test]
fn process_substitution_sees_the_parent_shell_variables() {
    let (output, status) = run("x=carried-in; cat <(echo $x)");

    assert_eq!(status, 0);
    assert_eq!(output, "carried-in\n");
}

#[test]
fn background_compound_runs_detached_and_wait_collects_it() {
    let d = temp_dir("bg-compound");
    let (_, status) = run(&format!(
        "(cd {} && echo from-bg > out.txt) & wait",
        d.display()
    ));

    assert_eq!(status, 0);
    let expected = "from-bg\n";
    let actual = std::fs::read_to_string(d.join("out.txt")).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn background_compound_survives_the_invocation() {
    let d = temp_dir("bg-survive");
    let f = d.join("late.txt");
    let start = std::time::Instant::now();

    let (_, status) = run(&format!("(sleep 0.4 && echo late > {}) &", f.display()));

    assert_eq!(status, 0);
    assert!(
        start.elapsed() < Duration::from_millis(300),
        "background job blocked the invocation"
    );
    assert!(!f.exists(), "job finished before the invocation returned");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !f.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let expected = "late\n";
    let actual = std::fs::read_to_string(&f).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn background_compound_sets_a_real_pid_in_bang() {
    let (output, status) = run("(true) & echo $!");

    assert_eq!(status, 0);
    let pid: u32 = output.trim().parse().expect("$! is a number");
    assert!(pid > 0);
}

#[test]
fn background_function_call_runs_in_the_job() {
    let d = temp_dir("bg-fn");
    let (_, status) = run(&format!(
        "f() {{ echo fn-bg > {}/fn.txt; }}; f & wait",
        d.display()
    ));

    assert_eq!(status, 0);
    let expected = "fn-bg\n";
    let actual = std::fs::read_to_string(d.join("fn.txt")).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn infinite_internal_producer_dies_when_the_consumer_leaves() {
    // The temp-file pipeline would materialise this forever; real pipes
    // mean head takes two lines and the loop dies of a broken pipe.
    let start = std::time::Instant::now();
    let (output, status) = run("while true; do echo y; done | head -2");

    assert_eq!(status, 0);
    assert_eq!(output, "y\ny\n");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "producer did not stop after the consumer left"
    );
}

#[test]
fn compound_stage_pipes_into_an_external_consumer() {
    let (output, status) = run("(echo one; echo two) | wc -l");

    assert_eq!(status, 0);
    assert_eq!(output.trim(), "2");
}

#[test]
fn pipeline_waits_for_every_stage_like_bash() {
    // The consumer leaves after one line, but bash still waits for the
    // whole pipeline — sleep is not killed; only the NEXT write dies.
    let start = std::time::Instant::now();
    let (output, status) = run("(echo first; sleep 1; echo second) | head -1");

    assert_eq!(status, 0);
    assert_eq!(output, "first\n");
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "pipeline returned before its slowest stage"
    );
}

#[test]
fn empty_backtick_substitution_expands_to_nothing() {
    // Found by sequence replay: docs text like (`attrs`) produced empty
    // backtick pairs that bash runs as an empty program; the walker fed ""
    // to the parser and died with unexpected-end-of-input.
    let expected = ("xy\n".to_string(), 0);

    let actual = run("echo x``y");

    assert_eq!(actual, expected);
}

#[test]
fn empty_dollar_substitution_expands_to_nothing() {
    let expected = ("ab\n".to_string(), 0);

    let actual = run("echo a$()b");

    assert_eq!(actual, expected);
}

#[test]
fn stderr_merged_into_a_pipe_reaches_the_consumer() {
    // The dominant 10% sequence-replay divergence: `cmd 2>&1 | grep` must
    // merge stderr into the PIPE (bash wires the pipe before redirects);
    // applying redirects first sent stderr around the filter.
    let (output, status) = run("ls /definitely-does-not-exist-bw 2>&1 | grep -c 'No such'");

    assert_eq!(status, 0);
    assert_eq!(output.trim(), "1");
}

#[test]
fn command_not_found_respects_a_stderr_redirect() {
    let (output, status) = run("definitely-not-a-command-bw 2>/dev/null; echo rc=$?");

    assert_eq!(status, 0);
    assert_eq!(output, "rc=127\n");
}

#[test]
fn missing_path_says_no_such_file_like_bash() {
    let (output, status) = run("./no/such/binary-bw");

    assert_eq!(status, 127);
    assert!(
        output.contains("No such file or directory"),
        "output: {output:?}"
    );
}

#[test]
fn signal_death_prints_bash_epitaph_and_status() {
    let (output, status) = run("sh -c 'kill -ABRT $$'; echo rc=$?");

    assert_eq!(status, 0);
    assert!(output.contains("Aborted"), "output: {output:?}");
    assert!(output.contains("rc=134"), "output: {output:?}");
}

#[test]
fn double_star_without_globstar_behaves_like_a_single_star() {
    // Found by sequence replay: `*.py **/*.py` matched a top-level file
    // TWICE (walker treated ** as recursive; bash without `shopt -s
    // globstar` treats ** as an ordinary single-segment *).
    let d = temp_dir("doublestar");
    std::fs::write(d.join("a.py"), "").unwrap();
    let (output, _) = run(&format!("echo {}/*.py {}/**/*.py", d.display(), d.display()));

    let expected = format!("{}/a.py {}/**/*.py\n", d.display(), d.display());
    assert_eq!(output, expected);
}

#[test]
fn umask_changes_the_mode_of_a_walker_created_file() {
    let d = temp_dir("umask-native");
    let (_, status) = run(&format!("umask 077 && echo hi > {}/f.txt", d.display()));

    assert_eq!(status, 0);
    let mode = std::fs::metadata(d.join("f.txt")).unwrap().permissions().mode() & 0o777;
    let expected = 0o600;
    assert_eq!(mode, expected);
}

#[test]
fn umask_reaches_a_spawned_child() {
    let d = temp_dir("umask-spawn");
    let (_, status) = run(&format!("umask 077 && touch {}/f.txt", d.display()));

    assert_eq!(status, 0);
    let mode = std::fs::metadata(d.join("f.txt")).unwrap().permissions().mode() & 0o777;
    let expected = 0o600;
    assert_eq!(mode, expected);
}

#[test]
fn umask_with_no_argument_prints_the_current_mask() {
    let expected = ("0027\n".to_string(), 0);

    let actual = run("umask 027 && umask");

    assert_eq!(actual, expected);
}

#[test]
fn umask_persists_across_invocations() {
    let results = run_session(&["umask 027", "umask"]);

    let expected = "0027\n";
    assert_eq!(results[1].0, expected);
}

#[test]
fn glob_with_a_leading_dotslash_keeps_it_in_the_result() {
    // Found by sequence replay: `./hugolib/*.go` matched, but the walker's
    // glob result dropped the `./` bash keeps (the glob crate resolves `.`
    // as CurDir and drops it while walking).
    let d = temp_dir("dotslash");
    std::fs::write(d.join("a.go"), "").unwrap();
    let (output, _) = run(&format!("cd {} && echo ./*.go", d.display()));

    let expected = "./a.go\n";
    assert_eq!(output, expected);
}

#[test]
fn syntax_error_reports_status_2() {
    let (output, status) = run("if true; then");

    assert_eq!(status, 2);
    assert!(output.contains("syntax error"), "output: {output:?}");
}
