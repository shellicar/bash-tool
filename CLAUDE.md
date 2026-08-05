# bash-tool

A bash implementation in Rust, built so that a command can be inspected and
approved before it runs, and so that the thing approved is the thing that runs.
Two crates:

- `bash-parser` — text in, tree out. It answers one question: is this bash
  syntax. It has no opinion about what can be executed.
- `bash-walker` — executes that tree. Expansion, redirection, pipelines on real
  OS pipes, job control, builtins, arithmetic, `set -x`, and shell state.

It exists to replace unconstrained `bash -c` as the tool a fleet Claude runs
commands through. A raw string can only be guessed at before it executes; a
tree can be read, gated by policy, and then executed as-is.

## Building and testing

```sh
cargo test --workspace          # the unit and behavioural suites
docker compose build            # the conformance world, once
node tools/conformance.mjs      # GNU Bash's own 83 test files, as a ratchet
```

The conformance run happens inside a container so both shells share one GNU
userland; `compose.yaml` builds it. It needs GNU Bash's source at
`~/repos/gnu/bash`, or `BASH_SRC` pointing elsewhere, and builds bash, its three
test helpers and every locale the tests ask for itself. It runs as a normal user
because several of bash's own files refuse to run as root.

The replay in the consuming rig mounts a Linux binary, cross-built in a
container so no toolchain is needed on the host:

```sh
docker run --rm -v $PWD:/src -w /src -e CARGO_TARGET_DIR=/src/target-linux \
  rust:alpine sh -c 'apk add --no-cache musl-dev && cargo build --release -p bash-walker'
```

Work happens in parallel across several worktrees of this repo, so
`tools/conformance.mjs --walker PATH` points the gate at a specific build.
`conformance-baseline.json` conflicts on every merge and is derived state:
regenerate it with `--accept` after merging rather than resolving it.

## Correctness is bash

Bash is the specification. If bash does it, this does it. Nobody working here
decides what should be supported — that is the SC's, and a session was ended in
July for deciding it and calling it design.

The oracle is `~/repos/gnu/bash/bash`, a real 5.3 build. Never `/bin/bash` on a
Mac, which is 3.2.57 from 2007 and wrong about almost everything modern.

Three ways to ask bash a question, in order of how often they settle it:

- **Run it.** Any single question, in seconds.
- **`set -x`** — bash prints every decision it makes, post-expansion. The
  walker's trace matches it byte for byte, including PS4's odd rule of
  repeating only its first character per nesting level.
- **`declare -f`** — bash prints its own parse tree back. Define the script as
  a function body and read what bash says it is. This is how parse questions
  get answered without reading `parse.y`.

Reading the source is the weakest of the three for behaviour. The `&`
associativity bug survived a careful reading of the grammar because the answer
is yacc's default shift on an ambiguous rule and appears nowhere in the text.

## The conformance suite is the gate

GNU Bash ships 83 test files. `tools/conformance.mjs` runs each under bash and
under the walker and compares the two streams separately, rather than against
the shipped `.right` files: those were captured with `> file 2>&1`, so bash's
stdio buffering is baked into them, which is not semantics. Cross-stream
ordering is therefore not tested.

`conformance-baseline.json` records the known failures; a test outside it that
fails is a regression. It drives the walker by script path, which is why it
cannot see the invalid-byte gap below.

That suite is the only instrument here that sees ordinary scripting. Do not
rank gaps by the rig's replay corpus: it is commands an agent wrote fixing
Python bugs in containers, so it proves what breaks that genre and is silent
about everything else. Arrays appear zero times in it, and arrays are exactly
what a careful script uses.

## Unsupported constructs are refused by static inspection, before anything runs

Decided 2026-08-05.

What the walker supports is a separate question from what parses, answered by a
static pass over the tree that runs before execution begins. If it finds
anything unimplemented, nothing runs at all.

The tool exists so that what is approved is what happens. A refusal raised
mid-walk breaks that: everything before the offending line has already run and
had its side effects, so the caller approved one tree and got part of one with
no way to tell which part. Refusing up front leaves no partial state to reason
about.

A construct on a branch that would not have been taken is refused too, and that
is right rather than a compromise. A script containing `set -o posix` is a
script written for a shell that has posix mode; whether this run reaches the
line is luck, not a property of the script.

What the pass cannot see keeps its runtime refusal, so the two are an addition
and not a replacement. The cases that are only knowable once running:

- text that is computed — `set $opt`, `$(get_flags) --x`, `"${args[@]}"`
- `eval`, `source`, `bash -c "$x"`, where the thing to inspect does not exist
  until the moment before it runs
- aliases, because the alias table is shell state

Both refusals must share one list, or they will drift and disagree about what
is supported.

What the output owes its reader, who is a Claude and cannot ask a follow-up:

- **Every finding, never the first.** The reader redesigns rather than retries,
  and a redesign made against half the constraint has to be made again.
- **Where**: line and column, compiler-style. `shellcheck --format=gcc` is the
  reference shape; the exact format is not settled.
- **What**: the construct named precisely. "Not supported" alone gives the
  reader nothing to act on.
- **What instead**: a suggestion where a correct one exists, and where none
  exists, saying so. Silence reads as "you missed an option". A wrong
  suggestion is worse than none, because it will be followed.
- **That nothing ran**, in words. Absence of output otherwise reads as success.

## Declined, for now

Recorded so nobody infers the boundary from what happens to fail.

**`set -o posix` (2026-08-05).** Bash consults `posixly_correct` in 186 places
in hand-written source: 17 in the grammar, and the rest across expansion,
execution, variables, jobs and 20 builtins. It is a whole-shell mode, not a
parser flag, so implementing only the piece one test needs would make the
walker claim posix mode and silently not be in it everywhere else. It gates
around twenty of bash's own test files. Not decided against, not scheduled.

(Count `posixly_correct` in the bash source excluding `y.tab.c`, which is
generated from `parse.y` and double-counts the grammar's 17.)

**Byte fidelity for invalid UTF-8 (2026-07-28).** Bash treats shell text as
opaque bytes; this is built on Rust `String`. Fixing it means a byte-oriented
word type through lexer, parser, expander and every builtin that touches text,
then re-validating everything built on top. Worth doing eventually, not now.

There is a severe unfixed manifestation, and it depends on how the walker was
invoked, which is why it is easy to "disprove" by testing the wrong mode:

```
-c or JSON, external process emits 0xFF   0 bytes. the whole invocation blanked, exit 0
script path, same script                  byte-identical to bash
-c, the walker's own printf '\377'        prints U+00FF. lossy, not blanked
```

So it is capture mode plus output from an external process. The capture reads
the buffer into a Rust `String`, that fails, and the error is discarded; script
path streams to inherited fds and never converts. A `$(...)` around it becomes
empty for the same reason.

```sh
bash-walker -c "echo before; /usr/bin/printf '\377'; echo after"
```

Use `printf`, not `head -c 8 /bin/echo`: Mach-O magic is non-ASCII but an ELF
header is not, so that reproduces on a Mac and nothing at all in a container.

The consequence that matters: the conformance suite drives the walker by script
path, so that gate structurally cannot see this. The two modes it does not test
are the two the tool is actually used through.

It is also what blocks `nquote4`, and that is where it will surface next.
`\x{...}` inside `$'...'` yields a BYTE rather than a codepoint: bash masks the
accumulated value with 0xFF (lib/sh/strtrans.c), so `$'\x{01234567}'` is `g`
and `$'\x{cd}'` is the single byte 0xCD. Six of that test's eighteen lines
produce bytes above 0x7F, which a `String` holds as two bytes of UTF-8 against
bash's one. The decoding itself is right as of e895394 (the braced form, `\x`
keeping its backslash when no digits follow, and a NUL ending the segment while
the word carries on); only the representation is left, so the test cannot close
until this does.

**Collating symbols and equivalence classes in patterns (2026-08-05).**
`[[.a.]]` and `[[=b=]]` inside a bracket expression are locale-collation
features, so there is no translation to a plain range: the answer depends on the
locale's collation table. Supporting them means replacing the matcher, and the
matcher is used by `case`, `[[ == ]]`, `${x#pat}`, `${x%pat}`, `${x/a/b}` and
pathname expansion, so every one of those is in scope for regression. Declined
for constructs almost nobody writes. `posixpat` needs them, so it stays failed.

POSIX character classes are the other half of that test and are NOT declined.
They are a small, safe fix that has not been done yet. The `glob` crate handles
bracket expressions correctly but does not know the twelve class names, and it
does not error on them, it silently matches nothing: `[[:xdigit:]]` against `e`
returns false where `[0-9a-fA-F]` returns true. So `case e in [[:xdigit:]])`
quietly takes the wrong branch today, which is the silent-wrong class this
project exists to remove. The fix is a translation table over the twelve names,
applied to the pattern before it reaches the matcher. It is low risk because the
only patterns whose behaviour changes are ones that match nothing today. It is
ASCII and the C locale only, which is a limitation to write down rather than a
reason not to do it.

**Extglob outside `[[ ]]`, deferred not declined (2026-08-05).** `shopt -s
extglob` on one line changes how a later line parses, because bash parses and
executes a script one command at a time. We parse the whole script up front, so
the `shopt` cannot reach the lexer, and `?(` `*(` `+(` `@(` `!(` are never word
syntax outside `[[ ]]`, where they are unconditional because bash makes them so.

Two of bash's tests, `extglob` and `printf`, need it. Note that `bash -n`
rejects both files too, for the same reason, so this is not the walker being
worse than bash at reading a file: it is the difference between parsing a script
and running one.

Closing it means always treating those five as word syntax, which accepts
slightly more than bash-with-extglob-off does. That is a small lexer change and
a change to what we support, so it needs the SC. Deferred on 2026-08-05 as not
understood well enough to decide, not declined.

**Brace expansion's remaining cases (2026-08-05).** Bash does not re-scan
expansion output for substitutions but does quote-remove it, so `{a..A}` yields
a literal backtick the walker reads as a substitution opener and a literal
backslash bash's quote removal eats. Matching it means the expander telling text
that came out of an expansion apart from text the parser saw, which is a
provenance change wanting a design rather than a fix at the edges. Parked. The
same defect is behind `"${a+'$('\'}"`.

**Arrays.** Unimplemented, and half-silent: subscripts refuse loudly, but
`arr=(a b c)` is accepted and stores the literal text, so `$arr` gives
`(a b c)` where bash gives `a`, and `files=(*.log)` does not glob.

## Traps

A fix in one place is usually not the fix. `$'...'` was wrong in three separate
scanners — the lexer's bracket matcher, the `[[ ]]` chunker, and the expander's
own paren matcher — and only the first had a test pointing at it. The parser
leaves substitution interiors opaque by design, so the walker re-scans them,
which is why the same question has more than one implementation.

Test a construct beside a neighbour, not alone. Five passing tests for `&`, all
with a single command, could not catch that `&` was backgrounding everything
before it: with nothing to bind wrongly to, the bug had nowhere to show.
