# conch-difftest

The differential test harness: runs the same shell script through **conch**
and a real **oracle shell** (`bash`, and where relevant POSIX `sh`) and
compares the result. This is conch's primary correctness signal for
bash/POSIX compatibility -- see the root plan's Phase 1 "Test harness" bullet
and `CLAUDE.md`.

Methodology follows two established prior-art projects, cited throughout
this crate's doc comments:

- **brush** (`github.com/reubeno/brush`) validates itself against ~1700
  bash compatibility tests plus a 300+-case integration suite that diffs
  stdout/exit codes against an oracle shell, and separately runs bash's own
  upstream `run-*` test scripts under a fixed 24x80 terminal for
  byte-exact, surrogateescape-equivalent comparison.
- **oils-for-unix**'s "spec test" methodology (`doc/known-differences.md`
  in their repo) groups many small, focused `####`-delimited cases per
  file and runs them against multiple real shells simultaneously, with
  explicit, documented divergences rather than treating every mismatch as
  a bug.

The community "Open POSIX Test Suite" is deliberately **not** a source for
new cases here -- it's been effectively dormant since ~2004. Bash's own
upstream test scripts and hand-written cases (as in this corpus) are a
higher-value investment; see the grounding notes in this agent's brief for
the full rationale.

## Status as of this writing

The `conch` binary exists and compiles (`target/debug/conch`) but is a
Phase 1-in-progress stub -- it doesn't yet parse or execute scripts. This
crate is fully wired up and its own tests pass today:

- `corpus_validation.rs` -- loads and structurally validates every corpus
  file. No shell subprocess involved.
- `oracle_selfcheck.rs` -- runs every case's own oracle(s) against a
  *second, independent invocation of themselves* (bash vs. bash, sh vs.
  sh) and asserts they agree. This exercises the entire harness (process
  spawning, byte-safe capture, normalization, comparison) using a real,
  deterministic shell as a stand-in for conch, so it's meaningful *today*
  even though conch itself isn't ready.
- `differential.rs` -- the real conch-vs-oracle suite. It runs today too,
  but in **report-only mode**: it finds the conch binary (if any), runs
  every case, prints a full categorized pass/fail/skip report, and then
  returns success regardless of the result, so CI doesn't stay red for
  functionality that's legitimately still under construction. See
  "Wiring in real execution" below for how this becomes a hard gate with
  zero code changes.

Run `cargo test -p conch-difftest -- --nocapture` from the workspace root
to see all of this today, including the live report.

## Directory layout

```
tests/conch-difftest/
├── Cargo.toml
├── README.md                  <- this file
├── known-differences.md       <- living doc of deliberate conch/bash divergences
├── src/
│   ├── lib.rs
│   ├── case.rs                <- the TOML case schema (source of truth for field docs)
│   ├── corpus.rs               <- loads + validates *.toml files under corpus/
│   ├── invoke.rs                <- spawns a shell, captures output as raw bytes
│   ├── normalize.rs             <- per-case byte-level normalization rules
│   ├── compare.rs                <- diffs a candidate run against an oracle run
│   └── runner.rs                  <- orchestration + trackable pass/fail/skip summary
├── corpus/
│   └── phase1/                     <- one *.toml file per category, several
│       ├── simple_commands.toml       cases per file, oils-spec-test style
│       ├── pipelines.toml
│       ├── lists.toml
│       ├── redirection.toml
│       ├── quoting.toml
│       ├── expansion.toml
│       ├── builtins.toml
│       └── invocation_modes.toml
└── tests/
    ├── corpus_validation.rs
    ├── oracle_selfcheck.rs
    └── differential.rs
```

Why a workspace member under `tests/`, not a bare `tests/*.rs` at the
workspace root: the root `Cargo.toml` is a virtual manifest (`[workspace]`
only, no `[package]`), so there's no root package for Cargo to attach a
root-level `tests/` directory to. A dedicated crate is the standard way to
get cross-crate integration tests (plus a non-trivial corpus and harness
library) in a Cargo workspace.

## The case format

Each corpus file is TOML, an array of `[[case]]` tables. Example (see
`corpus/phase1/*.toml` for the real, currently-shipping set):

```toml
[[case]]
name = "echo-basic"                     # unique, kebab-case; used as a report identifier
description = "echo prints its arguments separated by single spaces"
tags = ["builtins", "echo"]              # optional, free-form
oracles = ["bash", "sh"]                 # default: ["bash"]; each compared independently
invocation = "dash-c"                    # "dash-c" (default) | "script-file" | "stdin-pipe"
compare = ["stdout", "exit-code"]        # default; "stderr" is opt-in, see below
normalize = []                            # see normalize.rs; e.g. ["workdir"]
script = '''
echo hello world
'''
```

Always use TOML **literal** multi-line strings (`'''...'''`) for `script`,
never basic strings (`"""..."""`) -- shell scripts are full of backslashes
and quotes that a basic string would try to escape-interpret.

Full field reference lives as doc comments on `case::Case` and its enums
(`Oracle`, `Invocation`, `CompareTarget`, `NormalizeRule`,
`KnownDifference`) -- that's the source of truth; this README summarizes.

### Why stderr isn't compared by default

Shell error-message wording is notoriously shell- and version-specific.
`corpus/phase1/simple_commands.toml`'s `command-not-found-exit-code` case
is a concrete, verified example: bash says `command not found`, dash says
`not found` -- for the exact same construct, both exiting 127. Comparing
stderr by default would make that a permanent, uninteresting failure.
Opt in per case with `compare = [..., "stderr"]` when a case is
specifically about stderr content.

### Known-difference cases

A case can set `known_difference` instead of relying on a live oracle, for
behavior conch deliberately and permanently diverges on:

```toml
[[case]]
name = "some-deliberate-divergence"
description = "..."
known_difference = { id = "KD-0001", expect_stdout = "...", expect_exit_code = 0 }
script = '''
...
'''
```

When set, the case is checked against conch's own pinned expectation
instead of a live oracle run, and `oracles` may be omitted. Every such case
must have a matching entry in `known-differences.md` explaining *why*.
**There are no real entries yet** -- Phase 1 hasn't run against a working
conch, so no divergence has actually been found or decided on. Don't
pre-emptively invent one; add an entry (and a case) only once a real,
deliberate divergence is decided.

## How the harness works mechanically

For each `(case, oracle)` pair (or `(case, known_difference)`):

1. A **fresh temp directory** (`tempfile::tempdir()`) is created per
   invocation -- the candidate (conch) and the oracle each get their own,
   never a shared one. This matters for anything touching the filesystem
   (`cd`, `pwd`, redirection): two independent shells racing over the same
   directory would be both wrong and flaky.
2. The script is fed to the shell per `invocation`:
   - `dash-c`: `<shell> -c '<script>'`
   - `script-file`: `<shell> <path>`, where `<path>` is a `NamedTempFile`
     written inside that run's own workdir
   - `stdin-pipe`: the script bytes are written to the child's stdin and
     the handle is dropped (closing the pipe) so the child sees EOF
3. stdout, stderr, and the process exit code are captured as raw bytes /
   `Option<i32>` -- see "Byte-safety", next.
4. Each side is normalized independently (see "Non-determinism and
   normalization") using **that run's own** workdir, then compared per
   `compare`.
5. Every case produces a `CaseResult` (`Passed` / `Failed` / `Skipped` /
   `Error`), never silently omitted, and `runner::print_report` renders a
   summary plus per-failure detail -- a trackable pass/fail/skip count,
   not a single binary verdict, matching how brush and oils-for-unix both
   report differential results as an ongoing metric.

### Byte-safety

Comparisons run on raw `Vec<u8>`, never on a lossily-decoded `String`.
`String::from_utf8_lossy` collapses *every* invalid byte sequence into the
same U+FFFD replacement character, which would make two genuinely
different malformed outputs compare as equal -- exactly backwards for a
harness whose job is catching exactly that kind of mismatch.
`report::render_bytes` is the only place bytes get turned into a `String`
(for a failure message), and it renders each invalid byte as its own
`\xHH` escape (a surrogateescape-equivalent, reversible representation,
the same principle brush documents for its bash-upstream-script e2e
suite) so two different invalid outputs stay visibly distinct in a
report. See `report.rs`'s tests for the exact pitfall this avoids.

### Non-determinism and normalization

Phase 1's corpus only needs two rules (`normalize.rs`):

- `trailing-newline`: strip exactly one trailing `\n` before comparing.
- `workdir`: substitute *that run's own* temp directory (both its literal
  and canonicalized form -- needed because macOS's `/tmp` ->
  `/private/tmp` symlink means a real shell's `pwd` reports a different
  string than what was passed to `Command::current_dir`, confirmed while
  building this harness) with a stable `<CWD>` placeholder. Any case whose
  output embeds an absolute path (`pwd`, `cd` error messages, ...) needs
  this, since candidate and oracle never share a literal path even when
  they're semantically identical.

Later phases will need more, and the mechanism is designed to grow rather
than be reinvented:

- **Phase 4 (job control)**: `$$`/`$!` PIDs are exactly the workdir
  problem's shape -- a per-run, harness-known value substituted with a
  placeholder on both sides.
- **`$RANDOM`, hostnames**: not literal-string-known in advance the way a
  workdir is; add a masking rule backed by a small, explicit pattern
  match for the one or two fixed shapes actually needed, rather than
  pulling in a full regex engine speculatively.
- **TTY-dependent formatting** (column widths, `\r` handling, prompts):
  relevant once Phase 6 (interactive UX) lands. brush's approach --
  running under a fixed 24x80 controlling terminal via a PTY crate (e.g.
  `portable-pty`) -- is the concrete pattern to follow when that's
  tackled; see "Interactive mode scoping" below for why it's not done yet.

Keep normalization **explicit and opt-in per case** (never applied
globally) so a case's `normalize` list documents exactly which kind of
non-determinism it's accounting for.

### Interactive mode scoping

Phase 1 nominally includes an interactive REPL. This harness's Phase 1
corpus deliberately restricts itself to the two invocation modes that are
tractable for exact, deterministic stdout/exit-code comparison: `-c` and
script-file. `stdin-pipe` is included as a **non-PTY proxy** for
interactive-style input (commands arriving over stdin rather than as an
argument), which is enough to exercise conch's non-interactive input-loop
handling, but it is *not* a substitute for real interactive testing:
prompt rendering (`PS1`/`PS2`), line editing, and TTY-dependent output
formatting all require an actual pseudo-terminal to observe. That's
explicitly Phase 6 (interactive UX polish) territory in the roadmap; when
that work lands, add a `pty` invocation mode backed by a PTY crate, using
a fixed terminal size the same way brush does for its bash-upstream e2e
suite, so output is reproducible enough to diff.

### Scoping notes

Every corpus case in `phase1/` was picked to stay strictly inside the
Phase 1 plan's stated scope, and a few plausible-looking cases were
deliberately **left out** because they'd exercise behavior that isn't
promised yet:

- **Unquoted-variable word splitting on IFS** (e.g. `x="a b"; echo $x`
  producing two fields) is explicitly Phase 2 scope ("word splitting via
  IFS"). Every Phase 1 expansion case either double-quotes the expansion
  or expands a single-word value, so none of them depend on whether IFS
  splitting exists yet.
- **`NAME=value command` leading assignment prefixes** are ambiguous
  Phase 1 scope (arguably part of the POSIX "simple command" grammar, but
  not explicitly promised) -- left out rather than guessed at.
- **`echo -n`/`echo -e` flags** aren't promised by the Phase 1 plan, and
  bash/dash/POSIX `echo` already disagree with each other on them, so
  conch's behavior here is an open design question, not yet a regression
  target. Worth a `known-differences.md` entry once conch's echo flag
  behavior is actually decided.

If you add a Phase 2 (or later) corpus, put it in a sibling
`corpus/phase2/` directory and a corresponding `tests/phase2_*.rs` file
following the same `corpus::load_dir` / `runner::run_*` pattern -- nothing
about the harness itself is phase-specific.

## Wiring in real execution

This is the part whoever picks this crate up once Phase 1 execution
semantics exist in `crates/conch` needs. **The harness needs no code
changes** -- it's already fully wired:

1. `invoke::find_conch_binary()` already locates `target/{release,debug}/conch`
   relative to the workspace root (see its doc comment for why this is
   plain path discovery rather than Cargo's usual `CARGO_BIN_EXE_<name>`
   trick: `conch-shell` is bin-only with no `[lib]` target, so a normal
   path dependency on it is silently dropped by Cargo rather than wired
   up, and the real fix for that shape of problem -- artifact
   dependencies / `-Z bindeps` -- is still nightly-only, confirmed
   against this repo's pinned stable toolchain). Once `conch -c "..."`
   and `conch script.sh` actually execute scripts instead of printing a
   stub message, `differential.rs` will start finding and running the
   real binary with zero further changes, as soon as it's been built at
   least once (`cargo build` or `cargo test`, which the CI job already
   does).
2. By default, `differential.rs` runs every case, prints the full report,
   and **does not fail the build** even if every case fails -- that's
   deliberate, so this crate's own CI job doesn't go permanently red the
   moment the binary becomes discoverable but Phase 1 execution is still
   incomplete. Once you're confident the Phase 1 corpus should be fully
   green (or want to track it as a hard gate), set
   `CONCH_DIFFTEST_STRICT=1` in the environment -- either locally, or by
   adding it to the `difftest` job's `env:` in `.github/workflows/ci.yml`.
3. To test against a specific binary manually (a release build, an
   installed `conch`, a non-default `--target-dir`, ...), set
   `CONCH_BIN=/path/to/conch` -- it takes priority over the automatic
   `target/` search.
4. Watch `oracle_selfcheck.rs` stay green throughout -- if it ever starts
   failing, the bug is in the harness or a corpus case's assumption about
   bash/sh, not in conch.

Useful commands once wired:

```sh
# Full report, human-readable, without asserting on the result:
cargo test -p conch-difftest -- --nocapture

# Hard gate (fails the build on any non-pass):
CONCH_DIFFTEST_STRICT=1 cargo test -p conch-difftest --test differential -- --nocapture

# Against a specific binary:
CONCH_BIN=/path/to/conch cargo test -p conch-difftest --test differential -- --nocapture
```
