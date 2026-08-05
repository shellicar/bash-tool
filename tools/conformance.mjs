#!/usr/bin/env node
// Runs GNU Bash's own test suite against bash-walker and reports where they
// differ.
//
//   node tools/conformance.mjs [name...] [--accept] [--walker PATH] [--image TAG]
//
// A ratchet, not a report. `conformance-baseline.json` records which
// tests are known to fail; a test outside it that fails is a regression and
// exits 1. Closing a gap makes the run say so and `--accept` records it, which
// is the only way the baseline shrinks.
//
// Bash itself is the oracle, run here, rather than the shipped .right files:
// those were captured with `> file 2>&1`, so they have bash's stdio buffering
// baked in (stdout block-buffered to a file, stderr not), which no
// reimplementation can reproduce and which is not semantics. Running both
// shells with the streams kept apart compares what each decided, and leaves
// cross-stream ordering untested, deliberately.
//
// Both shells run inside the container built by conformance.Dockerfile, which
// carries the bash under test, the test files, and one GNU userland. On the
// host they ran against whatever sed, grep and awk that host had, and BSD and
// GNU disagree about several: that moves under you, and its failure mode is a
// false match, where both shells break identically and the gate calls it a
// pass. The walker is mounted in, so it must be a Linux build:
//
//   docker run --rm -v $PWD:/src -w /src -e CARGO_TARGET_DIR=/src/target-linux \
//     rust:alpine sh -c 'apk add --no-cache musl-dev && cargo build --release -p bash-walker'
//
// Exit 0 when every test matched, 1 otherwise, 64 on a bad call.

import { readdirSync, readFileSync, mkdtempSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const ROOT = new URL("..", import.meta.url).pathname.replace(/\/$/, "");
// --image so a rebuilt world can be tried under its own tag without
// overwriting the one everything else is currently measured against.
const imageAt = process.argv.indexOf("--image");
const IMAGE = imageAt >= 0 ? process.argv[imageAt + 1] : "bash-conformance";
// Paths inside the image, fixed by the Dockerfile.
const BASH = "/src/bash";
const WALKER = "/walker";
// --walker so a session working in its own worktree can point at its own
// build; several worktrees on this repo is how the work runs in parallel.
const walkerAt = process.argv.indexOf("--walker");
const WALKER_HOST = walkerAt >= 0 ? process.argv[walkerAt + 1] : `${ROOT}/target-linux/release/bash-walker`;
const TESTS = `${process.env.HOME}/repos/gnu/bash/tests`;
const TIMEOUT = 900_000;

const only = process.argv
  .slice(2)
  .filter((a, i, all) => !a.startsWith("-") && all[i - 1] !== "--walker" && all[i - 1] !== "--image");
const verbose = process.argv.includes("--verbose");
const accept = process.argv.includes("--accept");
const BASELINE = `${ROOT}/conformance-baseline.json`;

if (spawnSync("docker", ["image", "inspect", IMAGE], { stdio: "ignore" }).status !== 0) {
  console.error(`no ${IMAGE} image`);
  console.error(`build it from ${ROOT}: docker compose build`);
  process.exit(64);
}
if (!existsSync(WALKER_HOST)) {
  console.error(`no walker at ${WALKER_HOST}`);
  console.error("it must be a Linux build; see the cross-build at the top of this file, or pass --walker PATH");
  process.exit(64);
}

const names = readdirSync(TESTS)
  .filter((f) => f.endsWith(".tests"))
  .map((f) => f.replace(/\.tests$/, ""))
  .filter((n) => only.length === 0 || only.includes(n))
  .sort();

if (names.length === 0) {
  console.error("no matching tests");
  process.exit(64);
}

// One container for the whole sweep rather than one per test: 166 starts under
// emulation costs minutes of nothing. Each test still gets its own scratch
// directory, because the tests write files and a shared one lets an earlier
// test decide a later one's result.
//
// `.` on PATH is how bash's own runners find its helpers, recho, zecho and
// printenv; the image builds them beside the tests.
const RUNNER = `
set -u
for n in $NAMES; do
  d=$(mktemp -d)
  TMPDIR=$d THIS_SH=$SH BUILD_DIR=/src PATH=.:$PATH \
    timeout ${Math.floor(TIMEOUT / 1000 / 60)}m $SH ./$n.tests >/out/$n.$TAG.out 2>/out/$n.$TAG.err
  echo $? > /out/$n.$TAG.status
done
`;

function sweep(shell, tag, out) {
  return spawnSync(
    "docker",
    [
      "run", "--rm", "--platform", "linux/amd64",
      "-v", `${WALKER_HOST}:${WALKER}:ro`,
      "-v", `${out}:/out`,
      "-e", `NAMES=${names.join(" ")}`,
      "-e", `SH=${shell}`,
      "-e", `TAG=${tag}`,
      IMAGE, "sh", "-c", RUNNER,
    ],
    { encoding: "utf8", timeout: TIMEOUT, stdio: ["ignore", "inherit", "inherit"] },
  );
}

const readOut = (out, name, tag) => {
  const at = (ext) => {
    try {
      return readFileSync(`${out}/${name}.${tag}.${ext}`, "utf8");
    } catch {
      return "";
    }
  };
  const status = Number.parseInt(at("status").trim(), 10);
  // 124 is timeout(1)'s own exit, so a hang is reported as one rather than
  // silently comparing two truncated outputs.
  return { out: at("out"), err: at("err"), status, timedOut: status === 124 };
};

// Why a test failed, judged from the walker's own diagnostics rather than
// guessed. "unimplemented" is what the walker says it does not do; anything
// else is a divergence and needs reading.
function classify(w) {
  const text = w.err + w.out;
  if (w.timedOut) return "timeout";
  // Match the refusal by its stable half. Keying on "is not supported" filed
  // every `arrays are not supported` as a divergence, which made loud refusals
  // look like silent wrong answers and put them at the top of the queue.
  if (/not yet supported by this parser|not supported by bash-walker|unsupported/i.test(text)) return "unimplemented";
  if (/bash-walker: syntax error/.test(text)) return "parse error";
  return "divergence";
}

const out = mkdtempSync(join(tmpdir(), "bashconf-"));
console.log(`${names.length} tests, both shells in ${IMAGE}`);
for (const [shell, tag] of [[BASH, "bash"], [WALKER, "walker"]]) {
  const r = sweep(shell, tag, out);
  if (r.status !== 0 && r.error) {
    console.error(`the ${tag} sweep did not finish: ${r.error.message}`);
    process.exit(1);
  }
}

const results = [];
for (const name of names) {
  const b = readOut(out, name, "bash");
  const w = readOut(out, name, "walker");
  const outSame = b.out === w.out;
  const errSame = b.err === w.err;
  const statusSame = b.status === w.status;
  const verdict = outSame && errSame && statusSame ? "match" : classify(w);
  results.push({ name, verdict, outSame, errSame, statusSame, b, w });
  console.log(`${verdict.padEnd(14)} ${name}${verdict === "match" ? "" : `  (stdout ${outSame ? "=" : "≠"}, stderr ${errSame ? "=" : "≠"}, status ${b.status} vs ${w.status})`}`);
}

const tally = new Map();
for (const r of results) tally.set(r.verdict, (tally.get(r.verdict) ?? 0) + 1);
console.log(`\n${results.length} tests`);
for (const [k, v] of [...tally].sort((a, b) => b[1] - a[1])) console.log(`  ${String(v).padStart(3)}  ${k}`);

writeFileSync(`${ROOT}/conformance-report.json`, JSON.stringify(
  results.map((r) => ({
    name: r.name,
    verdict: r.verdict,
    outSame: r.outSame,
    errSame: r.errSame,
    bashStatus: r.b.status,
    walkerStatus: r.w.status,
    walkerErrHead: r.w.err.split("\n").slice(0, 3),
    bashOutHead: r.b.out.split("\n").slice(0, 3),
    walkerOutHead: r.w.out.split("\n").slice(0, 3),
  })),
  null,
  1,
));

if (verbose) {
  for (const r of results.filter((x) => x.verdict !== "match")) {
    console.log(`\n=== ${r.name} (${r.verdict})`);
    console.log(`  walker stderr: ${JSON.stringify(r.w.err.slice(0, 300))}`);
  }
}

const failing = Object.fromEntries(
  results.filter((r) => r.verdict !== "match").map((r) => [r.name, r.verdict]),
);

if (accept) {
  writeFileSync(BASELINE, `${JSON.stringify(failing, null, 1)}\n`);
  console.log(`\nbaseline recorded: ${Object.keys(failing).length} known failures`);
  process.exit(0);
}

if (!existsSync(BASELINE)) {
  console.error("\nno baseline; record one with --accept");
  process.exit(64);
}

// A partial run says nothing about tests it did not run, so the comparison is
// scoped to what was actually run.
const baseline = JSON.parse(readFileSync(BASELINE, "utf8"));
const ran = new Set(names);
const regressed = Object.keys(failing).filter((n) => !(n in baseline));
const fixed = Object.keys(baseline).filter((n) => ran.has(n) && !(n in failing));

for (const n of fixed) console.log(`\nnow passing: ${n}`);
for (const n of regressed) console.log(`\nREGRESSED: ${n} (${failing[n]})`);

if (fixed.length > 0 && regressed.length === 0) {
  console.log(`\n${fixed.length} newly passing; record with --accept`);
}

process.exit(regressed.length > 0 ? 1 : 0);
