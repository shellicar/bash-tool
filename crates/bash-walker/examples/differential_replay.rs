//! Differential validation of the walker, two modes:
//!
//! RECORDED (`--recorded triples.json`): replay against the container's real
//! recorded stdout+exit code. Only commands that are provably independent of
//! the container's environment qualify — after harness-noise removal that
//! subset turns out to be tiny (the corpus is filesystem-bound), so this
//! mode is a spot-check, not the main event.
//!
//! BASH (`--against-bash <bash> commands.json`): run each command through
//! BOTH the walker and a real bash on THIS machine — same filesystem, same
//! binaries, same env — and diff combined output + exit code. Environment
//! dependence stops mattering (both sides see the same one); the filter only
//! has to guarantee SAFETY (read-only programs, writes confined to /dev/null
//! or the per-command scratch cwd) and determinism (no clocks, pids,
//! backgrounding). This is the real fidelity instrument.
//!
//!   cargo run --release --example differential_replay -p bash-walker -- \
//!       --against-bash ~/repos/gnu/bash/bash /tmp/corpus_bash_all.json

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};

use bash_parser::{Command, CondExpr, Connection, Redirect, RedirectOp, SimpleCommand, Word};
use serde::Deserialize;

#[derive(Deserialize)]
struct Triple {
    c: String,
    r: i32,
    o: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Ground truth is a container recording: nothing environmental allowed.
    Recorded,
    /// Ground truth is local bash: only safety and determinism required.
    LocalBash,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--recorded") => run_recorded(&args[1]),
        Some("--against-bash") => {
            let cap = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(usize::MAX);
            run_against_bash(Path::new(&args[1]), &args[2], cap);
        }
        _ => {
            eprintln!("usage: --recorded <triples.json> | --against-bash <bash-path> <commands.json>");
            std::process::exit(2);
        }
    }
}

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!("bw-replay-{}", std::process::id()))
}

fn reset_scratch(scratch: &Path) {
    let _ = std::fs::remove_dir_all(scratch);
    std::fs::create_dir_all(scratch).unwrap();
}

fn run_recorded(path: &str) {
    let raw = std::fs::read_to_string(path).unwrap();
    let triples: Vec<Triple> = serde_json::from_str(&raw).unwrap();
    // SAFETY: single-threaded, before any spawns.
    unsafe { std::env::set_var("LC_ALL", "C") };
    let scratch = scratch_dir();
    reset_scratch(&scratch);
    std::env::set_current_dir(&scratch).unwrap();

    let mut selected = 0;
    let mut matched = 0;
    let mut mismatches = Vec::new();
    for t in &triples {
        if !select(&t.c, Mode::Recorded) {
            continue;
        }
        selected += 1;
        let mut state = bash_walker::ShellState::default();
        let (output, rc) = bash_walker::run(&t.c, &mut state);
        let _ = std::env::set_current_dir(&scratch);
        if output == t.o && rc == t.r {
            matched += 1;
        } else if mismatches.len() < 20 {
            mismatches.push((t.c.clone(), format!("rc={} {:?}", t.r, trunc(&t.o)), format!("rc={rc} {:?}", trunc(&output))));
        }
    }
    println!("triples: {}   selected: {selected}   matched: {matched}   mismatched: {}", triples.len(), selected - matched);
    print_mismatches(&mismatches);
    let _ = std::env::set_current_dir(std::env::temp_dir());
    let _ = std::fs::remove_dir_all(&scratch);
}

#[derive(Default)]
struct Tally {
    matched: usize,
    normalized_matched: usize,
    walker_unsupported: usize,
    timeouts: usize,
    mismatches: Vec<(String, String, String)>,
    /// Never leave this bucket unexamined again: the walker's `grep "a\|b"`
    /// infinite loop hid in it as "slow commands" for a whole run.
    timeout_samples: Vec<String>,
}

fn run_against_bash(bash: &Path, commands_path: &str, cap: usize) {
    let raw = std::fs::read_to_string(commands_path).unwrap();
    let commands: Vec<String> = serde_json::from_str(&raw).unwrap();
    // The corpus repeats the same command text heavily; each unique text is
    // one observation.
    let mut seen = HashSet::new();
    let commands: Vec<&String> = commands.iter().filter(|c| seen.insert(c.as_str())).collect();
    // SAFETY: single-threaded here, before any worker spawns.
    unsafe { std::env::set_var("LC_ALL", "C") };
    let walker_bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("bash-walker");
    assert!(walker_bin.exists(), "build the bash-walker binary first: {walker_bin:?}");

    let selected: Vec<&&String> = commands
        .iter()
        .filter(|c| select(c, Mode::LocalBash))
        .take(cap)
        .collect();
    eprintln!("selected {} of {} unique commands", selected.len(), commands.len());

    // Tunable for slow environments (qemu-emulated containers).
    let workers: usize = std::env::var("BW_REPLAY_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let chunk = selected.len().div_ceil(workers);
    let tally: Tally = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for (w, work) in selected.chunks(chunk.max(1)).enumerate() {
            let walker_bin = &walker_bin;
            let done = &done;
            handles.push(s.spawn(move || {
                let scratch = scratch_dir().join(format!("w{w}"));
                let mut t = Tally::default();
                for c in work {
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if n % 1000 == 0 {
                        eprintln!("... {n} compared");
                    }
                    compare_one(bash, walker_bin, c, &scratch, &mut t);
                }
                let _ = std::fs::remove_dir_all(&scratch);
                t
            }));
        }
        let mut total = Tally::default();
        for h in handles {
            let t = h.join().unwrap();
            total.matched += t.matched;
            total.normalized_matched += t.normalized_matched;
            total.walker_unsupported += t.walker_unsupported;
            total.timeouts += t.timeouts;
            total.mismatches.extend(t.mismatches);
            total.timeout_samples.extend(t.timeout_samples);
        }
        total
    });

    let compared = selected.len() - tally.walker_unsupported - tally.timeouts;
    println!("commands: {}", commands.len());
    println!("selected as safe+deterministic: {}", selected.len());
    println!(
        "walker named-unsupported: {}   timeouts/spawn-fail: {}",
        tally.walker_unsupported, tally.timeouts
    );
    println!("compared: {compared}");
    println!(
        "exact match: {} ({:.2}%)   +message-shape-only: {} (cumulative {:.2}%)",
        tally.matched,
        100.0 * tally.matched as f64 / compared.max(1) as f64,
        tally.normalized_matched,
        100.0 * (tally.matched + tally.normalized_matched) as f64 / compared.max(1) as f64,
    );
    println!(
        "real mismatches: {}",
        compared - tally.matched - tally.normalized_matched
    );
    print_mismatches(&tally.mismatches[..tally.mismatches.len().min(40)]);
    if !tally.timeout_samples.is_empty() {
        println!("timeout samples (side that timed out):");
        for s in tally.timeout_samples.iter().take(10) {
            println!("  {s}");
        }
    }
}

fn compare_one(bash: &Path, walker_bin: &Path, c: &str, scratch: &Path, t: &mut Tally) {
    reset_scratch(scratch);
    let Some((bash_out, bash_rc)) = run_one(bash, &["--norc", "-c", c], scratch, &[]) else {
        t.timeouts += 1;
        if t.timeout_samples.len() < 10 {
            t.timeout_samples.push(format!("[bash side] {}", trunc(c)));
        }
        return;
    };
    reset_scratch(scratch);
    let state_file = scratch.join(".bw-state.json");
    let Some((walker_out, walker_rc)) = run_one(
        walker_bin,
        &["-c", c],
        scratch,
        &[("BASH_WALKER_STATE", state_file.to_str().unwrap())],
    ) else {
        t.timeouts += 1;
        if t.timeout_samples.len() < 10 {
            t.timeout_samples.push(format!("[walker side] {}", trunc(c)));
        }
        return;
    };
    if walker_out.contains("not supported by bash-walker") {
        t.walker_unsupported += 1;
        return;
    }
    if bash_out == walker_out && bash_rc == walker_rc {
        t.matched += 1;
    } else if normalize(&bash_out) == normalize(&walker_out) && bash_rc == walker_rc {
        t.normalized_matched += 1;
    } else if t.mismatches.len() < 10 {
        t.mismatches.push((
            c.to_string(),
            format!("rc={bash_rc} {:?}", trunc(&bash_out)),
            format!("rc={walker_rc} {:?}", trunc(&walker_out)),
        ));
    }
}

/// Spawn with combined stdout+stderr into one file, 10s timeout.
fn run_one(bin: &Path, args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Option<(String, i32)> {
    let out_path = cwd.join(".combined-out");
    let f = std::fs::File::create(&out_path).ok()?;
    let mut cmd = Proc::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(f.try_clone().ok()?)
        .stderr(f);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    let timeout_secs: u64 = std::env::var("BW_REPLAY_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait().ok()? {
            Some(st) => break st,
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    };
    // Cap the read: a whitelisted generator (`seq 1 1000000000`) can emit
    // gigabytes; both sides get the same cap so the diff stays fair.
    let mut buf = Vec::new();
    let mut fh = std::fs::File::open(&out_path).ok()?;
    let _ = fh.seek(SeekFrom::Start(0));
    let _ = std::io::Read::take(&mut fh, 2 * 1024 * 1024).read_to_end(&mut buf);
    let out = String::from_utf8_lossy(&buf).into_owned();
    let _ = std::fs::remove_file(&out_path);
    Some((out, status.code().unwrap_or(128)))
}

/// Error-message lines differ in prefix between the two shells
/// ("bash: line 1: x: ..." vs "bash-walker: x: ..."); strip the prefixes so
/// only the substance compares.
fn normalize(s: &str) -> String {
    s.lines()
        .map(|l| {
            let mut l = l.trim_start();
            if let Some(rest) = l.strip_prefix("bash-walker: ") {
                l = rest;
            } else if let Some(pos) = l.find("bash: ") {
                // covers both "bash: ..." and "/path/to/bash: ..."
                if pos < 80 {
                    l = &l[pos + "bash: ".len()..];
                }
            }
            if let Some(rest) = l.strip_prefix("line ") {
                l = rest
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .trim_start_matches(": ");
            }
            l.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn trunc(s: &str) -> String {
    s.chars().take(160).collect()
}

fn print_mismatches(m: &[(String, String, String)]) {
    for (cmd, expected, actual) in m {
        println!("---\nCMD: {}", trunc(cmd));
        println!("  bash:   {expected}");
        println!("  walker: {actual}");
    }
}

// ---------------------------------------------------------------- filtering

fn select(src: &str, mode: Mode) -> bool {
    let Ok(ast) = bash_parser::parse(src) else {
        return false;
    };
    let mut assigned = HashSet::new();
    collect_assigned(&ast, &mut assigned);
    is_pure(&ast, &assigned, mode)
}

/// Read-only, deterministic external programs. In LocalBash mode both sides
/// run the same local binary, so version quirks cancel out; in Recorded mode
/// the list narrows further via `word`/`env` rules.
fn program_allowed(name: &str, mode: Mode) -> bool {
    const COMMON: &[&str] = &[
        "echo", "printf", "true", "false", "seq", "expr", "basename", "dirname", "tr", "cut",
        "sort", "uniq", "head", "tail", "rev", "grep", "cat", "test", "[", ":", "let",
    ];
    const LOCAL_EXTRA: &[&str] = &[
        "wc", "ls", "stat", "file", "diff", "cmp", "nl", "paste", "od", "readlink", "sed",
        "which", "printenv", "pwd", "cd", "export", "unset", "set", "shift", "read", "exit",
        "return", "break", "continue", "command", "xargs", "fold", "strings", "column", "env",
    ];
    COMMON.contains(&name) || (mode == Mode::LocalBash && LOCAL_EXTRA.contains(&name))
}

const FILE_TEST_FLAGS: &[&str] = &[
    "-e", "-f", "-d", "-s", "-r", "-w", "-x", "-L", "-h", "-p", "-S", "-b", "-c", "-g", "-k",
    "-u", "-O", "-G", "-t", "-N", "-nt", "-ot", "-ef", "-a",
];

/// Variables whose value differs run-to-run even on the same machine.
const NONDET_VARS: &[&str] = &[
    "RANDOM", "SRANDOM", "SECONDS", "EPOCHSECONDS", "EPOCHREALTIME", "BASHPID", "PPID",
    "LINENO", "BASH_COMMAND", "FUNCNAME",
];

fn collect_assigned(cmd: &Command, out: &mut HashSet<String>) {
    match cmd {
        Command::Simple(s) => {
            for (k, _) in &s.assignments {
                out.insert(k.clone());
            }
        }
        Command::Connection(c) => {
            collect_assigned(&c.left, out);
            collect_assigned(&c.right, out);
        }
        Command::Invert(i) | Command::Time(i) | Command::Background(i) | Command::Subshell(i)
        | Command::Group(i) => collect_assigned(i, out),
        Command::Redirected { command, .. } => collect_assigned(command, out),
        Command::For(f) => {
            out.insert(f.var.clone());
            collect_assigned(&f.body, out);
        }
        Command::ArithFor { expr, body } => {
            collect_arith_assigned(expr, out);
            collect_assigned(body, out);
        }
        Command::Arith { expr } => collect_arith_assigned(expr, out),
        Command::If(i) => {
            for (c, b) in &i.branches {
                collect_assigned(c, out);
                collect_assigned(b, out);
            }
            if let Some(e) = &i.else_branch {
                collect_assigned(e, out);
            }
        }
        Command::Case(c) => {
            for arm in &c.arms {
                if let Some(b) = &arm.body {
                    collect_assigned(b, out);
                }
            }
        }
        Command::While { cond, body } | Command::Until { cond, body } => {
            collect_assigned(cond, out);
            collect_assigned(body, out);
        }
        Command::FunctionDef { body, .. } => collect_assigned(body, out),
        Command::Cond(_) => {}
    }
}

fn collect_arith_assigned(expr: &str, out: &mut HashSet<String>) {
    let b = expr.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let rest = &expr[i..];
            if (rest.starts_with('=') && !rest.starts_with("=="))
                || rest.starts_with("++")
                || rest.starts_with("--")
                || rest.starts_with("+=")
                || rest.starts_with("-=")
            {
                out.insert(expr[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
}

fn is_pure(cmd: &Command, assigned: &HashSet<String>, mode: Mode) -> bool {
    match cmd {
        Command::Simple(s) => simple_is_pure(s, assigned, mode),
        Command::Connection(Connection { left, right, .. }) => {
            is_pure(left, assigned, mode) && is_pure(right, assigned, mode)
        }
        Command::Invert(i) | Command::Subshell(i) | Command::Group(i) => is_pure(i, assigned, mode),
        Command::Time(_) | Command::Background(_) => false,
        Command::Redirected { command, redirects } => {
            redirects.iter().all(|r| redirect_is_pure(r, assigned, mode))
                && is_pure(command, assigned, mode)
        }
        Command::For(f) => {
            f.words.iter().all(|w| word_is_pure(w, assigned, mode, false))
                && is_pure(&f.body, assigned, mode)
        }
        Command::ArithFor { expr, body } => {
            arith_text_pure(expr, assigned, mode) && is_pure(body, assigned, mode)
        }
        Command::Arith { expr } => arith_text_pure(expr, assigned, mode),
        Command::If(i) => {
            i.branches
                .iter()
                .all(|(c, b)| is_pure(c, assigned, mode) && is_pure(b, assigned, mode))
                && i.else_branch.as_ref().is_none_or(|e| is_pure(e, assigned, mode))
        }
        Command::Case(c) => {
            word_is_pure(&c.word, assigned, mode, false)
                && c.arms.iter().all(|arm| {
                    arm.patterns.iter().all(|p| word_is_pure(p, assigned, mode, true))
                        && arm.body.as_ref().is_none_or(|b| is_pure(b, assigned, mode))
                })
        }
        Command::While { cond, body } | Command::Until { cond, body } => {
            is_pure(cond, assigned, mode) && is_pure(body, assigned, mode)
        }
        Command::Cond(e) => cond_is_pure(e, assigned, mode),
        Command::FunctionDef { body, .. } => is_pure(body, assigned, mode),
    }
}

fn arith_text_pure(expr: &str, assigned: &HashSet<String>, mode: Mode) -> bool {
    match mode {
        Mode::Recorded => !expr.contains('$') && !expr.contains('`'),
        Mode::LocalBash => text_scan_pure(expr, assigned, mode, true, true),
    }
}

fn cond_is_pure(e: &CondExpr, assigned: &HashSet<String>, mode: Mode) -> bool {
    match e {
        CondExpr::Or(l, r) | CondExpr::And(l, r) => {
            cond_is_pure(l, assigned, mode) && cond_is_pure(r, assigned, mode)
        }
        CondExpr::Not(i) | CondExpr::Group(i) => cond_is_pure(i, assigned, mode),
        CondExpr::Unary { op, operand } => {
            let op_ok = match mode {
                Mode::Recorded => matches!(op.as_str(), "-z" | "-n"),
                Mode::LocalBash => true, // same fs on both sides
            };
            op_ok && word_is_pure(operand, assigned, mode, false)
        }
        CondExpr::Binary { op, left, right } => {
            let op_ok = mode == Mode::LocalBash || !matches!(op.as_str(), "-nt" | "-ot" | "-ef");
            op_ok && word_is_pure(left, assigned, mode, false)
                && word_is_pure(right, assigned, mode, true)
        }
        CondExpr::Term(w) => word_is_pure(w, assigned, mode, false),
    }
}

fn redirect_is_pure(r: &Redirect, assigned: &HashSet<String>, mode: Mode) -> bool {
    match r.op {
        RedirectOp::Heredoc | RedirectOp::HeredocStrip => {
            r.target.quoted
                || r.heredoc_body
                    .as_deref()
                    .is_none_or(|b| text_scan_pure(b, assigned, mode, true, true))
        }
        RedirectOp::HereString => word_is_pure(&r.target, assigned, mode, false),
        RedirectOp::In => mode == Mode::LocalBash && word_is_pure(&r.target, assigned, mode, false),
        RedirectOp::DupOut | RedirectOp::DupIn => mode == Mode::LocalBash,
        RedirectOp::Out | RedirectOp::Append | RedirectOp::OutErr | RedirectOp::AppendOutErr => {
            // Writes only where they cannot touch anything real: /dev/null,
            // or a bare relative name inside the per-command scratch cwd.
            mode == Mode::LocalBash
                && word_is_pure(&r.target, assigned, mode, false)
                && {
                    let t = r.target.text.trim_matches(|c| c == '"' || c == '\'');
                    t == "/dev/null" || (!t.contains('/') && !t.starts_with('-') && !t.is_empty())
                }
        }
    }
}

fn simple_is_pure(s: &SimpleCommand, assigned: &HashSet<String>, mode: Mode) -> bool {
    for (_, v) in &s.assignments {
        if !word_is_pure(v, assigned, mode, false) {
            return false;
        }
    }
    if !s.redirects.iter().all(|r| redirect_is_pure(r, assigned, mode)) {
        return false;
    }
    let Some(program) = &s.program else {
        return true;
    };
    let name = program.text.trim_matches(|c| c == '"' || c == '\'').to_string();
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '[' | ':' | '_' | '-' | '.'))
    {
        return false;
    }
    if !program_allowed(&name, mode) {
        return false;
    }
    let mut grep_positionals = 0;
    for (idx, a) in s.args.iter().enumerate() {
        if !word_is_pure(a, assigned, mode, name == "grep" || name == "find") {
            return false;
        }
        let bare = a.text.trim_matches(|c| c == '"' || c == '\'').to_string();
        // destructive flags on otherwise read-only tools
        if name == "sed" && (bare == "-i" || bare.starts_with("-i.") || bare.starts_with("--in-place")) {
            return false;
        }
        if name == "sort" && (bare == "-o" || bare.starts_with("--output")) {
            return false;
        }
        if name == "xargs" {
            // the program xargs runs must itself be allowed and harmless
            if !bare.starts_with('-') && idx == first_positional_index(&s.args) {
                if !program_allowed(&bare, mode) || bare == "xargs" {
                    return false;
                }
            }
        }
        if mode == Mode::Recorded {
            if (name == "test" || name == "[") && FILE_TEST_FLAGS.contains(&bare.as_str()) {
                return false;
            }
            if name == "grep" && (bare == "-f" || bare.starts_with("--file")) {
                return false;
            }
            if bare == "]" || bare.starts_with('-') {
                continue;
            }
            match name.as_str() {
                "echo" | "printf" | "seq" | "expr" | "basename" | "dirname" | "tr" | "test"
                | "[" | ":" | "let" | "true" | "false" => {}
                "grep" => {
                    grep_positionals += 1;
                    if grep_positionals > 1 {
                        return false;
                    }
                }
                _ => {
                    let numeric = bare.chars().all(|c| c.is_ascii_digit());
                    let tiny = bare.chars().count() <= 3 && !bare.contains('/');
                    if name == "cat" || !(numeric || tiny) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn first_positional_index(args: &[Word]) -> usize {
    args.iter()
        .position(|a| {
            !a.text
                .trim_matches(|c| c == '"' || c == '\'')
                .starts_with('-')
        })
        .unwrap_or(usize::MAX)
}

fn word_is_pure(w: &Word, assigned: &HashSet<String>, mode: Mode, allow_glob: bool) -> bool {
    if mode == Mode::Recorded && w.text.starts_with('~') {
        return false;
    }
    text_scan_pure(&w.text, assigned, mode, allow_glob, false)
}

fn text_scan_pure(
    raw: &str,
    assigned: &HashSet<String>,
    mode: Mode,
    allow_glob: bool,
    heredoc_mode: bool,
) -> bool {
    let b = raw.as_bytes();
    let mut i = 0;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < b.len() {
        let c = b[i];
        if in_squote {
            if c == b'\'' {
                in_squote = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\\' => i += 2,
            b'\'' if !in_dquote && !heredoc_mode => {
                in_squote = true;
                i += 1;
            }
            b'"' if !heredoc_mode => {
                in_dquote = !in_dquote;
                i += 1;
            }
            b'`' => match mode {
                Mode::Recorded => return false,
                Mode::LocalBash => {
                    let mut j = i + 1;
                    while j < b.len() && b[j] != b'`' {
                        if b[j] == b'\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    if j >= b.len() {
                        return false;
                    }
                    if !subcommand_pure(&raw[i + 1..j], mode) {
                        return false;
                    }
                    i = j + 1;
                }
            },
            b'$' => {
                if i + 1 >= b.len() {
                    i += 1;
                    continue;
                }
                match b[i + 1] {
                    b'(' if i + 2 < b.len() && b[i + 2] == b'(' => {
                        let Some(end) = find_matched(b, i + 1, b'(', b')') else {
                            return false;
                        };
                        if !arith_text_pure(&raw[i + 3..end.saturating_sub(1)], assigned, mode) {
                            return false;
                        }
                        i = end + 1;
                    }
                    b'(' => {
                        if mode == Mode::Recorded {
                            return false;
                        }
                        let Some(end) = find_matched(b, i + 1, b'(', b')') else {
                            return false;
                        };
                        if !subcommand_pure(&raw[i + 2..end], mode) {
                            return false;
                        }
                        i = end + 1;
                    }
                    b'{' => {
                        let Some(end) = find_matched(b, i + 1, b'{', b'}') else {
                            return false;
                        };
                        let inner = &raw[i + 2..end];
                        if !braced_param_pure(inner, assigned, mode) {
                            return false;
                        }
                        i = end + 1;
                    }
                    c2 if c2.is_ascii_alphabetic() || c2 == b'_' => {
                        let mut j = i + 1;
                        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                            j += 1;
                        }
                        if !name_pure(&raw[i + 1..j], assigned, mode) {
                            return false;
                        }
                        i = j;
                    }
                    b'?' | b'#' | b'@' | b'*' if mode == Mode::LocalBash => i += 2,
                    _ => return false, // $$, $!, digits, and everything else
                }
            }
            b'*' | b'?' if !in_dquote && !allow_glob && mode == Mode::Recorded => return false,
            b'<' | b'>' if !in_dquote && i + 1 < b.len() && b[i + 1] == b'(' => {
                if mode == Mode::Recorded {
                    return false;
                }
                let Some(end) = find_matched(b, i + 1, b'(', b')') else {
                    return false;
                };
                if b[i] == b'>' || !subcommand_pure(&raw[i + 2..end], mode) {
                    return false;
                }
                i = end + 1;
            }
            _ => i += 1,
        }
    }
    !in_squote && !in_dquote
}

fn name_pure(name: &str, assigned: &HashSet<String>, mode: Mode) -> bool {
    match mode {
        Mode::Recorded => assigned.contains(name),
        Mode::LocalBash => !NONDET_VARS.contains(&name),
    }
}

fn braced_param_pure(inner: &str, assigned: &HashSet<String>, mode: Mode) -> bool {
    let name_part = inner.trim_start_matches(['#', '!']);
    if inner.starts_with('!') {
        return false; // indirect: value unknowable statically
    }
    let name: String = name_part
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() || !name_pure(&name, assigned, mode) {
        return false;
    }
    let rest = &name_part[name.len()..];
    rest.is_empty()
        || text_scan_pure(rest, assigned, mode, true, false)
}

/// A `$(...)`/backtick interior: parse it and hold it to the same standard.
fn subcommand_pure(src: &str, mode: Mode) -> bool {
    let Ok(ast) = bash_parser::parse(src) else {
        return false;
    };
    let mut assigned = HashSet::new();
    collect_assigned(&ast, &mut assigned);
    is_pure(&ast, &assigned, mode)
}

fn find_matched(b: &[u8], open: usize, oc: u8, cc: u8) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
            }
            _ if b[i] == oc => depth += 1,
            _ if b[i] == cc => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}
