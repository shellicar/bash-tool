#!/usr/bin/env node
// Find every place a script parses under bash but not under bash-parser.
//
// Walks line prefixes. A prefix bash accepts (`bash -n`) is a syntactically
// complete cut point; the smallest such prefix our parser rejects is the
// smallest region that shows the difference. After reporting one, the cut
// point advances past it so a single run finds every region in a file rather
// than stopping at the first.
//
//   node tools/reduce.mjs ~/repos/gnu/bash/tests/redir.tests
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

const BASH = process.env.BASH_ORACLE ?? `${process.env.HOME}/repos/gnu/bash/bash`;
const CHECK = new URL("../target/debug/examples/parse_check", import.meta.url).pathname;

const run = (cmd, args, input) => {
  try {
    execFileSync(cmd, args, { input, stdio: ["pipe", "pipe", "pipe"] });
    return { ok: true, err: "" };
  } catch (e) {
    return { ok: false, err: String(e.stderr ?? "").trim() };
  }
};

const bashOk = (src) => run(BASH, ["-n"], src).ok;
const ours = (src) => run(CHECK, [], src);

const path = process.argv[2];
if (!path) {
  console.error("usage: reduce.mjs SCRIPT");
  process.exit(2);
}
const lines = readFileSync(path, "utf8").split("\n");

let start = 0;
let found = 0;
for (let n = start + 1; n <= lines.length; n++) {
  const src = lines.slice(start, n).join("\n") + "\n";
  if (!bashOk(src)) continue;
  const r = ours(src);
  if (r.ok) {
    start = n;
    continue;
  }
  found++;
  console.log(`--- lines ${start + 1}-${n}: ${r.err}`);
  console.log(lines.slice(start, n).join("\n"));
  console.log("");
  start = n;
}
if (!found) console.log("no divergence");
