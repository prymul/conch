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

**Phase 1** (simple commands, pipelines, redirection, quoting, lists,
basic `$VAR` expansion, invocation modes) is done: the `phase1/` corpus is
wired as a hard CI gate (`CONCH_DIFFTEST_STRICT=1`, see "Wiring in real
execution" below) and was last confirmed 100/100 against real bash/sh.

**Phase 2** (full POSIX word expansion: brace expansion, tilde expansion,
parameter-expansion operators, command substitution, arithmetic
expansion, globbing, IFS word splitting) is in progress in
`crates/conch-parser`/`crates/conch-core` at the time the `phase2/`
corpus was written. That corpus (120 cases across 7 files) is fully
built, self-validated, and oracle-verified against real bash/dash today,
but deliberately runs in **report-only mode** against conch regardless of
`CONCH_DIFFTEST_STRICT` -- see "Phase-specific strict gates" below for
exactly how, and why that separation matters.

**Phase 3** (control flow: `if`/`elif`/`else`, `for`, `while`/`until`
with `break`/`continue` including the numeric level argument, `case`,
subshells, and brace groups) is being implemented concurrently in
`crates/conch-parser`/`crates/conch-core` at the time the `phase3/`
corpus was written -- as of this writing conch's executor still only
handles `Command::Simple` (Phase 1) and panics on everything else, so
every Phase 3 case is expected to fail against conch for now. That
corpus (51 cases across 6 files) is, like Phase 2's, fully built,
self-validated, and oracle-verified against real bash/dash today, and
also runs in **report-only mode** via its own `CONCH_DIFFTEST_STRICT_PHASE3`
gate -- see "Phase-specific strict gates" below.

Every phase's three-test pattern is the same (`{phase}_corpus_validation.rs`,
`{phase}_oracle_selfcheck.rs`, `{phase}_differential.rs` -- Phase 1's
happen to be un-prefixed since they were written first):

- `corpus_validation.rs` -- loads and structurally validates every corpus
  file. No shell subprocess involved.
- `oracle_selfcheck.rs` -- runs every case's own oracle(s) against a
  *second, independent invocation of themselves* (bash vs. bash, sh vs.
  sh) and asserts they agree. This exercises the entire harness (process
  spawning, byte-safe capture, normalization, comparison) using a real,
  deterministic shell as a stand-in for conch, so it's meaningful even
  for a phase whose conch-side execution isn't ready yet. This is a hard
  gate for every phase, unconditionally -- it never depends on conch.
- `differential.rs` -- the real conch-vs-oracle suite: finds the conch
  binary (if any), runs every case, prints a full categorized
  pass/fail/skip report. Whether it fails the build depends on that
  phase's own strict-mode env var; see the next section.

Run `cargo test -p conch-difftest -- --nocapture` from the workspace root
to see all of this today, including every phase's live report.

### Phase-specific strict gates

Each phase's `{phase}_differential.rs` checks its **own**, separately
named env var rather than a single shared one -- `differential.rs` (Phase
1) checks `CONCH_DIFFTEST_STRICT`; `phase2_differential.rs` checks
`CONCH_DIFFTEST_STRICT_PHASE2`; `phase3_differential.rs` checks
`CONCH_DIFFTEST_STRICT_PHASE3`; a hypothetical Phase 4 would check
`CONCH_DIFFTEST_STRICT_PHASE4`; and so on. This is deliberate and is the
whole mechanism that keeps phases independent: CI's `difftest` job runs
`cargo test -p conch-difftest`, which builds and runs *every* test binary
in this crate regardless of which corpora are actually finished, so
turning one phase's corpus into a hard gate must never silently pull a
still-in-progress later phase's corpus along with it (and, symmetrically,
a later phase going green shouldn't require touching the earlier phase's
gate). This is exactly why the `phase3/` corpus in this repo could be
built and merged concurrently with the Phase 3 grammar/executor work
itself without any risk to Phase 1's `CONCH_DIFFTEST_STRICT=1` hard gate
or Phase 2's `CONCH_DIFFTEST_STRICT_PHASE2` state -- `phase3_differential.rs`
runs and reports every time `cargo test -p conch-difftest` does, but
can't fail the build unless someone deliberately opts it in. Flip a
phase to strict by setting its own var to `1` -- locally, or by adding it
to the `difftest` job's `env:` in `.github/workflows/ci.yml` -- once that
phase's execution has landed and its corpus is expected to be fully
green. Don't reuse an earlier phase's flag (e.g. `CONCH_DIFFTEST_STRICT`
or `CONCH_DIFFTEST_STRICT_PHASE2`) for a later phase; a shared flag would
mean the day an earlier phase legitimately earns a hard gate is the same
day every not-yet-implemented later phase starts failing the build too.

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
│   ├── phase1/                      <- one *.toml file per category, several
│   │   ├── simple_commands.toml        cases per file, oils-spec-test style
│   │   ├── pipelines.toml
│   │   ├── lists.toml
│   │   ├── redirection.toml
│   │   ├── quoting.toml
│   │   ├── expansion.toml
│   │   ├── builtins.toml
│   │   └── invocation_modes.toml
│   ├── phase2/                      <- same style, one file per expansion kind
│   │   ├── brace_expansion.toml
│   │   ├── tilde_expansion.toml
│   │   ├── parameter_expansion.toml
│   │   ├── command_substitution.toml
│   │   ├── arithmetic_expansion.toml
│   │   ├── globbing.toml
│   │   └── word_splitting.toml
│   └── phase3/                      <- same style, one file per control-flow construct
│       ├── conditionals.toml
│       ├── for_loops.toml
│       ├── while_until_loops.toml
│       ├── nested_loops.toml            (break N / continue N -- see its own header)
│       ├── case_statements.toml
│       └── subshells_and_groups.toml
└── tests/
    ├── corpus_validation.rs
    ├── oracle_selfcheck.rs
    ├── differential.rs
    ├── phase2_corpus_validation.rs
    ├── phase2_oracle_selfcheck.rs
    ├── phase2_differential.rs
    ├── phase3_corpus_validation.rs
    ├── phase3_oracle_selfcheck.rs
    └── phase3_differential.rs
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
**There are no `known_difference` cases yet** -- no genuinely deliberate,
permanent conch-vs-bash/sh divergence has actually been decided on. A
conch output that merely doesn't match bash/sh yet (an unimplemented
feature, or a regression) is not a known difference and does not belong
here -- see `known-differences.md`'s own header for the distinction.
`known-differences.md` does separately track divergences *between bash
and POSIX sh themselves* (found while building the Phase 2 corpus) --
that's a different, non-`known_difference`-schema section of the same
file, for context rather than for a specific case's pinned expectation.

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

Phase 2's corpus (`corpus/phase2/`, `tests/phase2_*.rs`) and Phase 3's
corpus (`corpus/phase3/`, `tests/phase3_*.rs`) both follow exactly this
pattern -- see "Directory layout" and "Phase-specific strict gates"
above. If you add a Phase 4 (or later) corpus, do the same: a sibling
`corpus/phase4/` directory, `tests/phase4_{corpus_validation,
oracle_selfcheck,differential}.rs` following the same `corpus::load_dir`
/ `runner::run_*` pattern, and its own `CONCH_DIFFTEST_STRICT_PHASE4` --
nothing about the harness itself is phase-specific. Phase 4 (job control)
will also be the first phase that needs a new `normalize.rs` rule
(`$$`/`$!` PID substitution) before its corpus can even pass its own
oracle self-check -- see "Non-determinism and normalization" above.

### Phase 2 scoping notes

`corpus/phase2/` covers the full Phase 2 plan bullet (brace expansion,
tilde expansion, parameter-expansion operators, command substitution,
arithmetic expansion, globbing, IFS word splitting) evaluated in POSIX's
specified order. Two things worth knowing about how it's organized:

- Every file is split (where applicable) into a POSIX-baseline half using
  `oracles = ["bash", "sh"]` and a bash-only-extension half using
  `oracles = ["bash"]` -- see each file's own header comment for exactly
  which constructs landed in which half, and `known-differences.md`
  ("Bash extensions not in the POSIX baseline") for the consolidated
  list plus two further cross-shell quirks that aren't extensions at all,
  just edge-case disagreements, found while verifying every case against
  real bash and dash directly (not only through this harness's own `sh`,
  which resolves to a non-dash POSIX-compatible shell in some dev
  environments -- see that doc's cross-shell-quirks section).
- Arrays are out of scope for this corpus (not part of the Phase 2 plan
  bullet), so no case here depends on `${arr[@]}`-style expansion, even
  though brace expansion's "empty alternative disappears via ordinary
  unquoted-word elision" behavior is most easily demonstrated with one.

### Phase 3 scoping notes

`corpus/phase3/` covers the Phase 3 plan bullet's control-flow
constructs: `if`/`elif`/`else`/`fi`, `for`, `while`/`until`,
`break`/`continue` (with and without the numeric level argument),
`case`/`esac`, subshells, and brace groups. All of it is POSIX baseline
-- there is no bash-only control-flow extension in scope here (bash's
`;;&`/`;&` `case` fallthrough terminators and `for ((...))` C-style loop
are both bash extensions and are deliberately left out, same rationale as
Phase 2's bash-extension carve-outs) -- so every case in this corpus runs
against both `oracles = ["bash", "sh"]` with no split needed, unlike
`corpus/phase2/`.

`corpus/phase3/nested_loops.toml` is the file most worth reading before
adding to: POSIX's `break n` / `continue n` numeric level argument is a
genuinely easy thing to get backwards when hand-writing a script (an
`until`/`while` loop where the increment happens *after* the point a
`continue 2` fires never reaches its own increment, which is an infinite
loop, not a test failure) -- its header spells out the exact contrasts
each case is meant to demonstrate, and every script in that file was run
against real bash and dash directly, under a hard wall-clock timeout
guard, while building this corpus.

Every `pwd`/`cd`-observing case in `corpus/phase3/subshells_and_groups.toml`
needs `normalize = ["workdir"]` for the same reason Phase 1's
`cd-then-pwd-reflects-new-directory` does -- see "Non-determinism and
normalization" above.

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

The same steps apply verbatim to Phase 2 once its execution semantics
land in `crates/conch-core`, substituting `phase2_differential` for
`differential` and `CONCH_DIFFTEST_STRICT_PHASE2` for
`CONCH_DIFFTEST_STRICT` (see "Phase-specific strict gates" above for why
they're separate flags):

```sh
# Phase 2 full report, report-only (today's state):
cargo test -p conch-difftest --test phase2_differential -- --nocapture

# Phase 2 hard gate, once warranted:
CONCH_DIFFTEST_STRICT_PHASE2=1 cargo test -p conch-difftest --test phase2_differential -- --nocapture
```

Likewise for Phase 3 once its execution semantics land in
`crates/conch-core`, substituting `phase3_differential` and
`CONCH_DIFFTEST_STRICT_PHASE3`. As of this writing, conch's executor only
handles `Command::Simple` (Phase 1) and panics (exit code 101) on every
`Command` variant Phase 3 introduces, so `phase3_differential` reports
0/102 against a built conch binary today -- that's the expected,
correct-for-the-wrong-reason-not-being-a-corpus-bug state described in
"Status as of this writing" above, confirmed by running the panic
directly (`conch -c 'if true; then echo hi; fi'` → `internal error:
entered unreachable code: conch-shell-parser only produces Command::Simple
in Phase 1`), not a mismatch traceable to any corpus case's own script:

```sh
# Phase 3 full report, report-only (today's state):
cargo test -p conch-difftest --test phase3_differential -- --nocapture

# Phase 3 hard gate, once warranted:
CONCH_DIFFTEST_STRICT_PHASE3=1 cargo test -p conch-difftest --test phase3_differential -- --nocapture
```
