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

**Phase 3b** (shell functions -- both `fname() { ...; }` and
`fname() ( ...; )` bodies, plus bash's `function fname { ...; }`
extension -- the `local`/`return`/`set`/`shift` builtins, and real
positional-parameter state: `$1`.../`$#`/`$@`/`$*`/`$0`) is being
implemented concurrently in `crates/conch-parser`/`crates/conch-core` at
the time the `phase3b/` corpus was written. As of this writing
`crates/conch-parser` has no `function_definition` grammar at all, so
every function-defining case fails to *parse* against conch (confirmed:
`conch -c 'f() { echo hi; }; f'` -> `conch: unexpected operator '('
at byte 1, expected a separator (';', '&', or newline) or end of input`,
exit 2); `local`/`return`/`set`/`shift` aren't registered builtins yet, so
calling any of them tries to exec a same-named external command instead
(confirmed: `conch -c 'return 3'` -> `conch: return: No such file or
directory (os error 2)`, exit 127); and every positional-parameter read
silently expands to empty/zero rather than erroring, since no
positional-parameter shell state exists yet. That corpus (27 cases
across 6 files) is, like Phase 2's and Phase 3's, fully built,
self-validated, and oracle-verified against real bash/dash today, and
also runs in **report-only mode** via its own
`CONCH_DIFFTEST_STRICT_PHASE3B` gate -- see "Phase-specific strict gates"
below.

**Phase 4** (job control: backgrounding with `&`, `wait`, `$!`, `jobs`,
and `trap`) is being implemented concurrently in
`crates/conch-parser`/`crates/conch-core` at the time the `phase4/`
corpus was written. As of this writing `&` is parsed but always runs its
pipeline synchronously in the foreground rather than actually
backgrounding it (per `conch-shell-core::exec`'s own doc comment), and
`wait`/`trap` aren't registered builtins at all yet. This matters more
than the equivalent gap did for earlier phases: several Phase 4 cases
synchronize a background job with a `mkfifo` gate (a blocking `read` that
only unblocks once another command later in the same script writes to
it) specifically to make "is this job still running right now" a
deterministic fact instead of a sleep-based guess -- but that pattern
*depends on `&` actually backgrounding*. Run today against a
pre-Phase-4 conch binary, the gating command runs synchronously instead,
blocks forever waiting for a writer that never gets a turn to run, and
the harness's own [`invoke::wait_with_timeout`] wall-clock bound (added
alongside this corpus -- see "Non-determinism and normalization" below)
is what turns that into a clean, reported timeout `Error` instead of
hanging the whole differential suite (confirmed live: exactly this
happened against the pre-Phase-4 binary while building this corpus).
That corpus (22 cases across 3 files) is, like every earlier phase's,
fully built, self-validated, and oracle-verified against real bash/dash
today, and also runs in **report-only mode** via its own
`CONCH_DIFFTEST_STRICT_PHASE4` gate -- see "Phase-specific strict gates"
below.

Job control is also the first phase whose corpus needed a scoping
decision from outside this crate: real interactive job control (`fg`/`bg`
reclaiming the controlling terminal, Ctrl-Z-driven `SIGTSTP`
suspend/resume) is restricted by POSIX to *interactive* shells, and this
harness's `-c`/script-file/stdin-pipe invocation modes have no
controlling terminal at all -- there is nothing for that slice of job
control to attach to, structurally, not just something hard to make
deterministic. `corpus/phase4/` therefore covers only the
non-terminal-dependent slice (backgrounding, `wait`, `$!`, `trap`, and
`jobs`/`kill -0` status checks for jobs observed via `wait`/exit status
rather than by racing on real-time terminal output); the terminal-owning
half is intentionally out of this corpus's scope and is expected to be
verified manually once that implementation exists, the same way this
project's other genuinely-interactive surfaces are (see "Interactive mode
scoping" below).

**Phase 5** (builtins completeness: `read`, `getopts`, `test`/`[`,
`printf`, `declare`, `unset`, `alias`, `source`/`.`, `eval`, `exec`,
`type`, `command`, `umask`, `kill`, and completing `set`'s option-flag
half) is being implemented concurrently in
`crates/conch-parser`/`crates/conch-core` at the time the `phase5/`
corpus was written. This is the first phase whose corpus needed a harness
addition rather than only new cases: `read` fundamentally needs
stdin-driven input, which nothing before this phase required (Phase 1-4
are all argv/script-text driven), so [`case::Case::stdin`] and
`invoke::configure_stdin` were added -- see "The case format" below for
the field itself and "Wiring in real execution" for how `Invocation::
StdinPipe` (which delivers the *script* over stdin) stays independent of
it. As of this writing conch's implementation of this builtin set is
partial rather than all-or-nothing -- run against the binary at the time
this was written, `test`/`[`, `printf`, `type`/`command`, `umask`, and
most of `unset`/`source`/`eval` already pass, while `read`, `getopts`,
`declare`, `exec`, and `set`'s option flags don't yet (confirmed:
`read x` silently leaves `x` empty rather than actually reading stdin,
and a `getopts` loop produces no output at all). That corpus (97 cases
across 13 files) is, like every earlier phase's, fully built,
self-validated, and oracle-verified against real bash/dash today, and
also runs in **report-only mode** via its own
`CONCH_DIFFTEST_STRICT_PHASE5` gate -- see "Phase-specific strict gates"
below.

**Phase 6** (interactive UX: persistent history, tab completion, syntax
highlighting, `PS1`/`PS2` prompt expansion, startup file sourcing) is
being implemented concurrently in `crates/conch`/`crates/conch-core` at
the time this section was written, and structurally breaks the pattern
every earlier phase's corpus followed -- see "Phase 6: what's differential
and what isn't" below for the full writeup, but the short version: this is
the first phase where the *candidate* side of a comparison can't be "spawn
the compiled `conch` binary via `-c`/script-file/stdin and diff its
stdout" at all for most of its features, because prompt rendering, tab
completion, and syntax highlighting only happen inside conch's
interactive, rustyline-backed readline loop, which needs a real
controlling terminal this harness deliberately never allocates (see
"Interactive mode scoping" below -- unchanged from every earlier phase's
version of this same constraint, just newly load-bearing here). Only two
of Phase 6's five features turn out to be genuinely comparable against
real bash at all (`PS1`/`PS2` expansion, via a bash mechanism that doesn't
need a pty -- see below; tab completion candidate generation, likewise);
the other three (persistent history, syntax highlighting, startup file
sourcing) have no live-bash-oracle shape to compare against and are
ordinary Rust unit/integration test territory in `crates/conch-core`/
`crates/conch` instead, not this crate's corpus. As of this writing:
`corpus/phase6/prompt_expansion.toml` (14 cases) plus its own
`phase6_prompt_corpus_validation.rs`/`phase6_prompt_oracle_selfcheck.rs`
are built, self-validated, and oracle-verified against real bash today
(both are unconditional hard gates already, like every earlier phase's
`corpus_validation`/`oracle_selfcheck` pair); `src/completion_oracle.rs`
(a `compgen`-driving oracle mechanism, fully unit-tested against real
bash) exists but has no corpus yet, pending a design decision on tab
completion's exact scope (see below); and `phase6_prompt_differential.rs`
/`phase6_completion_differential.rs` -- the actual conch-vs-oracle
comparisons -- don't exist yet at all, deliberately: both need a Cargo
dev-dependency on `conch-shell-core` plus a call to a specific,
not-yet-landed pure-core function (`expand_prompt`/`complete_word` or
whatever they end up being named), and adding that dependency before the
function exists would break compilation of this entire crate for every
contributor, not just report a failing test -- a materially different
risk from every earlier phase's corpus, which could always be built and
merged concurrently with its own execution-semantics work because a
compiled `conch` binary (even one that doesn't implement the feature yet)
is all `invoke::run(&ShellUnderTest::Conch(path), ...)` ever needed to
exist. See "Phase 6: what's differential and what isn't" for exactly what
unblocks each piece and how to wire it up once it does.

Every phase's three-test pattern is the same (`{phase}_corpus_validation.rs`,
`{phase}_oracle_selfcheck.rs`, `{phase}_differential.rs` -- Phase 1's
happen to be un-prefixed since they were written first). Phase 6's
prompt-expansion pair follows the same two-thirds-of-the-pattern shape
(`phase6_prompt_corpus_validation.rs`, `phase6_prompt_oracle_selfcheck.rs`)
against its own separate schema/loader (`prompt_case.rs`/
`corpus::load_prompt_dir`, not `case.rs`/`corpus::load_dir` -- see
`prompt_case.rs`'s module doc comment for why), with its `_differential.rs`
third deliberately not yet added (see the Phase 6 status paragraph above).
What each file in the pattern is for, generically:

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
`CONCH_DIFFTEST_STRICT_PHASE3`; `phase3b_differential.rs` checks
`CONCH_DIFFTEST_STRICT_PHASE3B`; `phase4_differential.rs` checks
`CONCH_DIFFTEST_STRICT_PHASE4`; `phase5_differential.rs` checks
`CONCH_DIFFTEST_STRICT_PHASE5`; a hypothetical Phase 6 would check
`CONCH_DIFFTEST_STRICT_PHASE6`; and so on. This is deliberate and is the
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
│   ├── runner.rs                  <- orchestration + trackable pass/fail/skip summary
│   ├── prompt_case.rs              <- Phase 6 PS1/PS2 case schema (separate from case.rs)
│   ├── prompt_oracle.rs             <- drives bash's `${VAR@P}` prompt-expansion transform
│   └── completion_oracle.rs          <- drives bash's `compgen` builtin (no corpus/schema yet)
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
│   ├── phase3/                      <- same style, one file per control-flow construct
│   │   ├── conditionals.toml
│   │   ├── for_loops.toml
│   │   ├── while_until_loops.toml
│   │   ├── nested_loops.toml            (break N / continue N -- see its own header)
│   │   ├── case_statements.toml
│   │   └── subshells_and_groups.toml
│   ├── phase3b/                     <- same style, functions/local/return/positional params
│   │   ├── functions.toml
│   │   ├── return_and_exit_status.toml
│   │   ├── local_scoping.toml
│   │   ├── recursion.toml
│   │   ├── positional_parameters.toml
│   │   └── at_star_field_splitting.toml (the quoted-vs-unquoted $@/$* contrast)
│   ├── phase4/                      <- same style, job control (non-terminal-dependent slice)
│   │   ├── background_and_wait.toml
│   │   ├── jobs_status.toml             (kept minimal -- see its own header)
│   │   └── trap.toml
│   ├── phase5/                      <- same style, builtins completeness
│   │   ├── read.toml                    (first user of Case::stdin)
│   │   ├── getopts.toml
│   │   ├── test_and_bracket.toml        (POSIX's 0/1/2/3/4-argument disambiguation rules)
│   │   ├── printf.toml
│   │   ├── declare_and_readonly.toml    (declare is bash-only; no array support at all)
│   │   ├── unset.toml
│   │   ├── alias.toml
│   │   ├── source_and_eval.toml
│   │   ├── exec.toml
│   │   ├── type_and_command.toml
│   │   ├── umask.toml
│   │   ├── kill.toml
│   │   └── set_options.toml             (set's option-flag half; set -- is in phase3b)
│   └── phase6/                      <- separate PromptCase schema, not case.rs's Case -- see below
│       └── prompt_expansion.toml        (no completion-candidate corpus yet -- see below)
└── tests/
    ├── corpus_validation.rs
    ├── oracle_selfcheck.rs
    ├── differential.rs
    ├── phase2_corpus_validation.rs
    ├── phase2_oracle_selfcheck.rs
    ├── phase2_differential.rs
    ├── phase3_corpus_validation.rs
    ├── phase3_oracle_selfcheck.rs
    ├── phase3_differential.rs
    ├── phase3b_corpus_validation.rs
    ├── phase3b_oracle_selfcheck.rs
    ├── phase3b_differential.rs
    ├── phase4_corpus_validation.rs
    ├── phase4_oracle_selfcheck.rs
    ├── phase4_differential.rs
    ├── phase5_corpus_validation.rs
    ├── phase5_oracle_selfcheck.rs
    ├── phase5_differential.rs
    ├── phase6_prompt_corpus_validation.rs
    └── phase6_prompt_oracle_selfcheck.rs
    # phase6_prompt_differential.rs and phase6_completion_*.rs don't exist
    # yet -- see "Phase 6: what's differential and what isn't" below.
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
stdin = "some input\n"                   # optional; see below -- not the same as invocation = "stdin-pipe"
script = '''
echo hello world
'''
```

Always use TOML **literal** multi-line strings (`'''...'''`) for `script`,
never basic strings (`"""..."""`) -- shell scripts are full of backslashes
and quotes that a basic string would try to escape-interpret. `stdin` is
the opposite case: it's literal *input data* for a stdin-driven builtin
like `read` (Phase 5+), not shell source, so an ordinary TOML basic
string with explicit `\n` escapes is usually the clearest way to spell
out exactly what's on each line, including whether there's a trailing
newline at all -- see `corpus/phase5/read.toml` for the real, shipping
examples.

`stdin` is independent of `invocation`, and deliberately cannot be
combined with `invocation = "stdin-pipe"` (a validation error --
`stdin-pipe` already uses the child's one stdin stream to deliver the
*script itself*, so there's no way to unambiguously also deliver separate
`read`-input data over that same stream). For every other invocation
mode, `stdin` is the *only* thing written to the child's stdin, which
otherwise defaults to closed -- see `case::Case::stdin`'s doc comment and
`invoke::configure_stdin`.

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
oracles = []
known_difference = { id = "KD-0001", expect_stdout = "...", expect_exit_code = 0 }
script = '''
...
'''
```

When set, the case is checked against conch's own pinned expectation
instead of a live oracle run. `Case::validate` doesn't actually *require*
`oracles` to be empty when `known_difference` is set (only that it isn't
empty when `known_difference` is *not* set), but write `oracles = []`
explicitly anyway rather than omitting the field: omitting it lets serde's
ordinary default silently fill in `["bash"]` (the same default a case
with no `known_difference` at all gets), which -- confirmed while adding
this crate's first real `known_difference` case, `corpus/phase5/
source_and_eval.toml`'s `eval-syntax-error-matches-bashs-leniency-not-
dashs-abort` -- means `oracle_selfcheck.rs` would still run that oracle
(trivially, bash vs. itself, so it can't ever actually fail) purely
because the field was left absent rather than because it means anything.
Every `known_difference` case must have a matching entry in
`known-differences.md` explaining *why*. A conch output that merely
doesn't match bash/sh yet (an unimplemented feature, or a regression) is
not a known difference and does not belong here -- see
`known-differences.md`'s own header for the distinction. As of this
writing there are four real `known_difference`-backed entries (KD-0001
through KD-0004, three from Phase 4's job control and one -- `eval`'s
syntax-error handling -- from Phase 5), though only KD-0004 currently has
a corresponding `known_difference`-schema corpus case pinning it directly
(`corpus/phase5/source_and_eval.toml`); KD-0001 through KD-0003 are still
tracked as prose-only entries with their corpus case (where one exists at
all) left as an ordinary, currently-failing live oracle comparison --
converting one to `known_difference` is a deliberate follow-up step, not
automatic the moment an entry is written. `known-differences.md` does
separately track divergences *between bash and POSIX sh themselves*
(found while building the Phase 2 corpus) -- that's a different,
non-`known_difference`-schema section of the same file, for context
rather than for a specific case's pinned expectation.

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

`$$`/`$!` PIDs were flagged here, before Phase 4's corpus existed, as
the next obvious candidate for a normalization rule -- a per-run,
harness-known value substituted with a placeholder on both sides, the
same shape as the workdir problem. In practice, `corpus/phase4/` needed
no such rule at all: every case that needs a background job's PID
captures `$!` into a shell variable and uses it functionally (as an
argument to `wait`/`kill -0`) rather than ever printing the literal value,
which sidesteps the non-determinism entirely instead of normalizing it
away after the fact. Left here as a note in case a future case genuinely
needs to assert on a PID's literal text -- the mechanism above is still
the right shape for that if it comes up.

### Wall-clock timeouts (hang prevention)

A different kind of non-determinism, orthogonal to output normalization:
**can a case hang instead of producing wrong output at all.** Every
phase through 3b makes this impossible by construction -- nothing before
Phase 4 (job control) ever runs more than one process at a time, so
there's nothing for a script to block *waiting on*. Phase 4 changes that.
Several `corpus/phase4/` cases synchronize with a background job via a
`mkfifo` gate (a blocking `read` that only unblocks once a later command
in the same script writes to it) specifically to turn "is this job still
running" into a deterministic fact instead of a `sleep`-based guess (see
`corpus/phase4/jobs_status.toml`'s header) -- but that pattern's
correctness *depends on backgrounding actually working*. Confirmed live
while building that corpus: run against a conch binary from before Phase
4 landed (where `&` is parsed but always runs synchronously in the
foreground), the gating command never actually backgrounds, so the
`read` blocks forever waiting for a writer that never gets a turn to run.

`invoke::wait_with_timeout` (used by every invocation, every phase, not
just Phase 4's) bounds this: a shell invocation that hasn't exited within
`invoke::INVOCATION_TIMEOUT` (10 seconds -- far more than any legitimate
case should ever need, including on a slow/loaded CI runner) is killed
and reported as a normal `CaseOutcome::Error`, the same as any other
harness-level failure to run a shell at all, rather than blocking the
rest of the suite (and therefore CI) indefinitely. It's implemented as a
dedicated waiter thread doing a blocking `wait_with_output()` and
reporting back over a channel with `recv_timeout`, deliberately *not* as
a `try_wait`-then-`sleep` polling loop -- an earlier version of this
function used exactly that approach, and even a short poll interval taxes
*every* invocation (not just hung ones) with up to one interval's worth
of pure latency, which measurably slowed down this crate's entire test
suite across every phase before being replaced with the channel-based
version (see `invoke.rs`'s doc comment on `wait_with_timeout` for the
specifics). This is unconditional harness behavior, not a per-case
`normalize` opt-in -- there's no scenario where a script legitimately
*wants* to hang forever.

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

### Phase 3b scoping notes

`corpus/phase3b/` covers shell function definition and calling (both
`fname() { ...; }` and `fname() ( ...; )` bodies, plus bash's
`function fname { ...; }` extension), the `return` and `local` builtins,
and real positional-parameter state (`set --`, `shift [n]`, `$1`.../`$#`/
`$@`/`$*`). Almost all of it is POSIX baseline -- `local`, while not
actually specified by POSIX at all, is a de facto standard every shell
relevant to this project (including dash) implements the same way, so it
runs against both oracles like the rest of this corpus -- with two
bash-only exceptions, each `oracles = ["bash"]` only and documented in
`known-differences.md`'s bash-extensions table:
`bash-function-keyword-also-defines-a-function` in `functions.toml`
(dash doesn't accept the `function` keyword at all), and
`return-outside-any-function-is-an-error-in-bash` in
`return_and_exit_status.toml` (dash treats a top-level `return` as an
implicit `exit` instead of a builtin usage error -- a stdout-content
divergence, not just wording, so `oracles = ["bash", "sh"]` genuinely
can't be used here; see that file's own header and
`known-differences.md`'s "Cross-shell quirks worth knowing" section).

`corpus/phase3b/at_star_field_splitting.toml` is the file most worth
reading before adding to: it's the file this whole corpus's hardest
semantic (quoted vs. unquoted `$@`/`$*`, plus the N=0 zero-fields edge
case) gets dedicated coverage in, and its
`printf-with-zero-positional-at-is-not-a-valid-zero-fields-demonstration`
case is a documented **negative** result kept deliberately -- `printf`
turned out not to be a valid way to demonstrate "$@" contributing zero
fields (it applies its format string at least once regardless, treating
a missing operand as an empty string, so it produces the same one-line
output whether given zero args from `"$@"` or truly no operands at all).
That was discovered by actually running it against real bash and dash
while building this corpus, not assumed -- see the file's header for the
full writeup and which two cases (a `for` loop and a function-argument
count) are the correct way to prove the zero-fields property instead.

`corpus/phase3b/recursion.toml`'s
`local-variables-nest-correctly-across-recursive-calls` case exists
specifically so `local` scoping is proven to nest across more than one
call-stack frame, not just work once -- see that file's header.

### Phase 4 scoping notes

`corpus/phase4/` covers the non-terminal-dependent slice of job control:
backgrounding a command list with `&`, `wait` (bare, and targeting a
specific `$!`-captured pid), and `trap` (catching a signal, ignoring one,
resetting to default disposition, and the `EXIT` pseudo-signal), plus
minimal `jobs`/`kill -0` status checks. All of it is POSIX baseline except
two `trap -p`-based introspection cases (see below), so almost every case
runs against both `oracles = ["bash", "sh"]`.

**What's deliberately not here, and why:** real interactive job control --
`fg`/`bg` reclaiming the controlling terminal, `Ctrl-Z`-driven `SIGTSTP`
suspend/resume -- is restricted by POSIX to interactive shells, and none
of this harness's invocation modes (`-c`, script-file, stdin-pipe) have a
controlling terminal at all. This isn't "hard to make deterministic", the
usual bar for leaving something out of this corpus -- there is
structurally nothing for that slice of job control to attach to under any
of these invocation modes. It's expected to be verified manually once
implemented, the same way this project already treats other genuinely
interactive surfaces (see "Interactive mode scoping" above).

Three techniques recur across this corpus, worth knowing before adding to
it:

- **Never print a literal `$!`/`$$` value.** Every case that needs a
  background job's PID captures it into a variable and uses it
  functionally (as a `wait`/`kill -0` argument) instead. This is what lets
  this corpus need no new PID-normalization rule in `normalize.rs` at all
  -- see "Non-determinism and normalization" above.
- **Route concurrent jobs' output to separate files, not shared stdout.**
  `wait` makes "has this job finished yet" deterministic, but it does
  *not* make "which of two genuinely concurrent jobs' direct stdout
  writes lands first" deterministic -- that's a real race between two
  live processes. `background_and_wait.toml`'s multi-job case writes each
  job's output to its own file and reads them back afterward in a fixed,
  script-chosen order instead.
- **Use a `mkfifo` gate, not a `sleep` guess, for "is this still
  running".** `jobs_status.toml`'s cases need to assert something about a
  job's state *while it's still running*, which a `sleep` can only ever
  approximate (and which the project's testability guardrails
  specifically warn against relying on). A background job that blocks
  reading from a FIFO until a later command explicitly writes to it makes
  "is it still running at this exact point" a fact guaranteed by
  construction. See "Wall-clock timeouts (hang prevention)" above for the
  harness-level safety net this pattern's correctness actually depends on.

`corpus/phase4/jobs_status.toml`'s header documents two genuine bash-vs-
dash divergences in `jobs -p` found while building this corpus (pipe/
command-substitution job-table visibility, and post-`wait` bookkeeping
timing) -- neither is a `known_difference` (that schema is for a
deliberate *conch* decision), so both are recorded in
`known-differences.md`'s "Cross-shell quirks worth knowing" section
instead, the same way Phase 2's and Phase 3b's oracle-vs-oracle quirks
are.

`corpus/phase4/trap.toml`'s two subshell-inheritance cases are
`oracles = ["bash"]` only, using `trap -p` to introspect trap state
directly rather than relying on a live signal: `kill -SIG $$` run from
inside a subshell doesn't actually target the subshell itself, since
POSIX subshells keep the top-level shell's original `$$` value unchanged
(bash's non-POSIX `$BASHPID` exists specifically to work around this).
dash's `trap` has no `-p` support at all (confirmed:
`dash: trap: Illegal option -p`), which is why these two can't be shared-
oracle cases -- see that file's header and `known-differences.md`'s
bash-extensions table for the exact wording (this one is technically a
dash gap against a POSIX-specified flag, not bash extending past POSIX,
but the practical `oracles = ["bash"]`-only effect on the corpus is the
same).

### Phase 5 scoping notes

`corpus/phase5/` covers the Phase 5 plan bullet's builtin set: `read`,
`getopts`, `test`/`[`, `printf`, `declare`, `unset`, `alias`, `source`/
`.`, `eval`, `exec`, `type`, `command`, `umask`, `kill`, and completing
`set`'s option-flag half (`set --`/positional-parameter manipulation
itself is already `corpus/phase3b/positional_parameters.toml`'s
territory). Almost all of it is POSIX baseline; `declare` is the one
whole-file exception (bash-only, see below).

**The harness addition this phase needed:** `read` fundamentally needs
data arriving on stdin, and nothing before Phase 5 required that --
Phase 1-4 are entirely argv/script-text driven. `Invocation::StdinPipe`
already existed, but it delivers the *script itself* over stdin, which
is a different thing entirely and can't also carry separate `read`-input
data over that same one stream. `case::Case::stdin` (an optional field,
literal input bytes, validated to conflict with `stdin-pipe`) plus
`invoke::configure_stdin` close that gap -- see "The case format" above
for the field and `corpus/phase5/read.toml` for the cases that actually
exercise it. This is the same shape of harness-before-corpus need Phase 4
had for `invoke::wait_with_timeout`: a genuinely new *kind* of
non-determinism/capability gap, not just more cases in the existing
mold, gets a harness change, not a workaround inside a case's script.

**`declare` and arrays:** `declare` has no dash equivalent at all
(confirmed: `dash: declare: not found`), so every `declare` case is
`oracles = ["bash"]` only, in `declare_and_readonly.toml`. Separately,
and unconditionally regardless of which shell: conch has no array
support at all, project-wide, not just as a Phase 5 scoping choice -- so
nothing here exercises `declare -a`/`declare -A`, only scalar-variable
attributes (`-r`, `-i`, `-x`, `-f`).

**`test`/`[`'s argument-count disambiguation rules:** deliberately given
real, dedicated coverage in `test_and_bracket.toml` rather than just the
comparison-operator cases that are easy to remember to test anyway --
POSIX 2.9.4.5 specifies distinct 0/1/2/3/4-argument forms, and the
classic real-world `[ $x = y ]` bug (an unquoted, empty `$x` vanishing as
a word entirely and silently shifting a 3-argument comparison into a
malformed 2-argument one) gets its own case rather than being left as
lore.

**A recurring pattern found while building this corpus is big enough to
warrant its own consolidated writeup rather than one-off notes per
case:** POSIX's "special built-in" utilities (`.`, `:`, `break`,
`continue`, `eval`, `exec`, `exit`, `export`, `readonly`, `return`,
`set`, `shift`, `times`, `trap`, `unset`) are permitted to abort a
non-interactive shell entirely on certain errors, and dash consistently
takes that permission where bash is far more lenient -- `readonly`/
`unset` violations, `set -u`'s unset-variable error, and `eval` syntax
errors all abort the whole rest of the script on dash (even mid-`;`-
joined-line), while bash either treats the same error as ordinary and
non-fatal, or aborts only the current physical source line and resumes
on the next one. This turns out to be the exact same rule already
documented for `${var:?message}` and arithmetic division by zero in
Phase 2's corpus, just not previously named as a general pattern -- see
`known-differences.md`'s cross-shell-quirks section for the full
writeup, and `declare_and_readonly.toml`/`source_and_eval.toml`/
`set_options.toml` for the corpus cases.

## Phase 6: what's differential and what isn't

Phase 6 (interactive UX) is a genuine break from the pattern every earlier
phase's corpus followed, and is written up here in more depth than a
typical "scoping notes" section because the *shape* of the problem
changed, not just the content. Read this before adding to
`corpus/phase6/` or wiring up either `_differential.rs`.

### Why this phase is structurally different

Every Phase 1-5 corpus case works the same way regardless of whether
conch's execution semantics exist yet: `invoke::run` spawns the compiled
`conch` binary via `-c`/script-file/stdin and diffs its stdout/stderr/exit
code against a real oracle shell spawned the identical way. That
mechanism has **zero Rust-level coupling** to conch's internals -- a
freshly-`cargo build`'d `conch` binary that panics or prints nothing for
an unimplemented feature is still something `invoke::run` can spawn and
capture output from; the corpus just reports it as failing until the
feature lands. This is exactly what let `corpus/phase2/` through
`corpus/phase5/` be built and merged *concurrently* with their own
execution-semantics work with zero risk to the harness's own build.

Prompt rendering (`PS1`/`PS2`), tab completion, and syntax highlighting
don't have that property: they only happen inside conch's interactive,
rustyline-backed readline loop (see `crates/conch/src/main.rs`'s
`run_interactive`), which needs a real controlling terminal this harness
deliberately never allocates (see "Interactive mode scoping" above --
unchanged as a constraint, just newly load-bearing here, the same way
Phase 4's job-control corpus had to carve out the terminal-owning half of
`fg`/`bg`/Ctrl-Z as out of scope for the identical structural reason). The
fix the project settled on (see the team's own Phase 6 kickoff notes) is
to require each Phase 6 feature to be split into a pure, fully-testable
core function plus a thin, not-independently-testable rustyline-trait
adapter -- e.g. an `expand_prompt(shell: &Shell, template: &str) -> String`
that the interactive loop calls, but that a test can also call directly,
in-process, with no pty involved at all.

That fix changes *this crate's* risk profile, though: comparing a
directly-called Rust function against a live bash oracle means
`conch-difftest` needs an actual Cargo dependency on `conch-shell-core`
(and a call matching that function's real signature) for the differential
test to even compile -- unlike every earlier phase, **the corpus's
differential test can't be written and merged before the target function
exists.** Getting this wrong doesn't just leave a test failing or
skipped; it breaks `cargo test -p conch-difftest` (and therefore
`cargo test --all-features`, the general `test` CI job) for every
contributor, including whoever is mid-flight on an unrelated part of this
same crate. So: the oracle-driving mechanism, its own unit tests, and (for
the piece with a stable enough schema already) the corpus are all built
and merged *now*, entirely bash-only and with no dependency on
`conch-shell-core` at all; the `_differential.rs` files that actually call
into conch's pure-core functions are deliberately held back until those
functions exist, at which point wiring them up is the mechanical last
step described under each feature below.

### `PS1`/`PS2` prompt expansion -- genuinely differential, oracle ready

**Status: corpus + oracle self-check built and green; differential test
blocked on `expand_prompt`/`expand_prompt_for_ps2`-shaped functions
landing in `conch-shell-core`.**

The key discovery this crate made while scoping Phase 6 (see
`prompt_oracle.rs`'s module doc comment for the full writeup): bash's
backslash-escape prompt expansion (`\u`, `\h`, `\w`, `\$`, ...) plus its
`promptvars` parameter/command-substitution pass normally only run when
bash actually draws an interactive prompt -- `bash -c 'PS1="\u@\h "; echo
"$PS1"'` prints the *raw*, unexpanded template, confirmed directly, since
that code path never fires for a non-interactive `-c` invocation with no
prompt ever drawn. But bash >= 4.4's `${parameter@P}` transform operator
("the expansion is a string that is the result of expanding the value of
parameter as if it were `PS1`," per the bash manual) runs *exactly* that
same pipeline on demand, no pty required. `prompt_oracle.rs`'s
`expand_ps1_via_bash`/`expand_ps2_via_bash` drive this directly, and their
own test suite confirms empirically (against real bash 5.3) that
backslash escapes, `promptvars` variable/command-substitution expansion,
and a forced `$?` value are all reproduced faithfully this way. ubuntu-
latest's bash (CI's oracle host) is well above the 4.4 floor.

`corpus/phase6/prompt_expansion.toml` (14 cases, in `prompt_case.rs`'s own
schema -- deliberately **not** `case.rs`'s `Case`, since there's no
`script`/`invocation`/`oracles` here at all; see that module's doc comment
for the full rationale) sticks to escapes and mechanisms confirmed
deterministic for a live comparison: literal text, `\$`, `\\`, `\n`,
`\w`/`\W` (both normalized via the existing `NormalizeRule::Workdir`, or
via a `cwd_subdir` for a `\W`-only case), `$?` forced via `last_status`,
`promptvars` variable and command-substitution expansion, and `PS2`
through the identical pipeline. Deliberately excluded (see
`prompt_oracle.rs`'s "Non-determinism this module's callers must avoid"
for the full reasoning): `\d`/`\t`/`\T`/`\@`/`\A`/`\D{fmt}` (wall-clock
date/time -- the candidate and oracle run microseconds apart, enough to
occasionally disagree at a second/minute boundary; worth a plain unit
test with a fixed/injected clock on the conch side instead, if the
implementation supports one), `\j`/`\!`/`\#` (interpreter-internal
counters a fresh oracle invocation and conch's own `Shell` have no reason
to agree on numerically even if both implement the escape correctly), and
`\s` (shell name -- expected to permanently, deliberately read `"conch"`
rather than bash's `"bash"`, a `known_difference` case once that's
actually decided rather than a live comparison; `prompt_case.rs`'s own
`PromptKnownDifference` schema exists and is ready for exactly this).

**A real risk worth flagging explicitly, caught mid-implementation while
writing this section (this crate and Phase 6's own implementation are
being built concurrently, in the same working tree -- see the top-level
Phase 6 status paragraph):** `crates/conch/Cargo.toml` has already picked
up a direct `conch-shell-lexer` dependency with a comment naming
`src/prompt.rs` as its consumer, which reads as the actual `PS1`/`PS2`
expansion logic landing **inside the bin-only `conch-shell` package**
itself, not `conch-shell-core`. If that's where it stays, this crate
genuinely **cannot** call it in-process the same way it does
`conch-shell-core` -- `conch-shell` has no `[lib]` target, the exact same
bindeps limitation `invoke::find_conch_binary`'s own doc comment already
explains for why *that* function has to locate the compiled binary on
disk instead of linking against it. Two ways this resolves, both worth
checking for before assuming the plan below just works:

- **Best case:** the pure expansion function itself ends up in
  `conch-shell-core` (alongside `hostname`/`is_effective_root`/
  `list_path_executables`, which have already landed there for exactly
  this kind of Phase 6 primitive), with `crates/conch/src/prompt.rs` as
  only the thin rustyline-facing adapter around it. The plan below applies
  verbatim.
- **If it stays in `crates/conch/src/prompt.rs` instead:** it's still
  perfectly unit-testable in place (a binary crate's own `#[cfg(test)]`
  modules run fine via `cargo test -p conch-shell`, no `[lib]` target
  needed for *that*) -- but a cross-crate differential comparison from
  *this* crate needs one more step first: add a `[lib]` target to
  `crates/conch/Cargo.toml` alongside its existing `[[bin]]` (a package
  can have both; `src/main.rs` would then reach its own crate's `prompt`
  module via `use conch_shell::prompt` the same way any other consumer
  would) so there's an actual `.rlib` for this crate to add a normal path
  dependency on. This is a small, well-worn Cargo pattern, not a
  workaround -- just a step nobody has needed yet in this repo because
  every prior phase's testable logic already lived in a proper lib crate
  from the start.

**To wire up the real differential test once `expand_prompt` (or whatever
it ends up being named) lands, assuming the best case above (a
`conch-shell-core`-reachable function):**

1. Add `conch-shell-core = { workspace = true }` (or, per the fallback
   above, whatever crate/path it actually landed in) to this crate's
   `[dev-dependencies]` in `Cargo.toml`.
2. Add `tests/phase6_prompt_differential.rs`: for each non-`known_difference`
   case, construct a `conch_shell_core::Shell` (via `Shell::new()`, then
   override `cwd`/`env_vars`/`last_status` to match the case -- see
   `phase6_prompt_oracle_selfcheck.rs`'s `resolve_workdir`/`expand` helpers
   for the equivalent oracle-side setup to mirror), call
   `expand_prompt(&shell, &case.template)` directly (no subprocess, no
   timeout needed -- it's a plain in-process function call), and compare
   the returned `String`'s bytes against `prompt_oracle::expand_ps1_via_bash`
   (or `_ps2_`)'s output, both normalized per `case.normalize` against
   their own respective `cwd`. For a `known_difference` case, compare
   directly against `PromptKnownDifference::expect` instead, with no live
   oracle run at all -- same shape as `compare::compare_known_difference`,
   just against a single `String` rather than a `RunOutcome` triple.
3. Add `CONCH_DIFFTEST_STRICT_PHASE6` handling to that new file (report-only
   by default, matching every earlier phase's own strict-gate convention --
   see "Phase-specific strict gates" above), and, once warranted, to
   `.github/workflows/ci.yml`'s `difftest` job `env:` block.
4. `phase6_prompt_corpus_validation.rs`/`phase6_prompt_oracle_selfcheck.rs`
   need no changes at all -- they were deliberately written not to depend
   on `expand_prompt` existing, and stay exactly as valuable afterward.

### Tab completion -- differential in principle, blocked on a scope decision too

**Status: oracle-driving mechanism (`completion_oracle.rs`) built, unit-
tested against real bash, and unused by any corpus yet -- deliberately,
pending a design decision on what `complete_word` is actually scoped to
cover.**

bash ships `compgen [options] [word]` specifically to query its own
completion logic without a terminal (confirmed empirically -- see
`completion_oracle.rs`'s module doc comment and test suite): `compgen -c`
lists command-name candidates (builtins, functions, aliases, keywords, and
every executable on `$PATH`), `compgen -f`/`-d` list filenames/directories
relative to the current directory, `compgen -A function`/`-A alias`/
`-A variable` list exactly those categories. `compgen_via_bash` drives
this with `$PATH` fully replaced (not merely prepended) by a caller-
supplied, controlled directory list, so results are exactly as
deterministic as this crate's other oracle mechanisms rather than
depending on whatever happens to be installed on the machine running the
suite.

What's still an open question, and why no corpus exists yet: the team's
proposed signature, `fn complete_word(shell: &Shell, word: &str) ->
Vec<String>`, takes no argument-position information (is `word` the first
word of the command line, or a later one?) -- real bash's own *default*
completion (no `bash-completion` package, no `complete -F` registered)
dispatches on exactly that distinction: command-name completion
(`compgen -c`-equivalent) at the first word, filename completion
(`compgen -f`-equivalent) everywhere else. A single-argument
`complete_word` most plausibly means one of: (a) it's always called with
enough context already resolved that position doesn't need to be a
parameter, (b) it dispatches on `word`'s own shape instead (e.g. "contains
a `/`" => path completion, else => command completion -- a common
simplified heuristic for a first completion implementation), or (c) it
deliberately only covers one category for now (commands only, or files
only) with the rest left for later. Building `corpus/phase6/
completion_candidates.toml` against a guess here risks encoding the wrong
one and having to redo it; whoever lands `complete_word` should confirm
which of these (or something else) it actually is, at which point:

1. Add the corpus (`PromptCase`-style schema likely isn't the right fit
   here -- a completion case needs a `$PATH` directory list, defined
   functions/aliases, a prefix word, and (if (a) or a hybrid of the above)
   a position/context marker; a fresh `CompletionCase` schema, sibling to
   `prompt_case.rs`, is the likely shape) with cases exercising each
   `CompletionKind` `completion_oracle.rs` already supports
   (`Command`/`Filename`/`Directory`/`Function`/`Alias`/`Variable`).
2. Add the `conch-shell-core` dev-dependency (if not already added for
   the prompt-expansion wiring above) and
   `tests/phase6_completion_differential.rs`, calling `complete_word`
   directly and comparing its result (sorted + deduplicated, matching
   `compgen_via_bash`'s own return convention -- see that module's doc
   comment for why raw `compgen` ordering isn't part of the contract
   worth matching) against the corresponding `compgen_via_bash` call.
3. Same `CONCH_DIFFTEST_STRICT_PHASE6` convention as the prompt-expansion
   differential (a single shared Phase 6 strict-mode flag covering both
   `_differential.rs` files is fine -- they're two halves of the same
   phase, unlike two genuinely different phases -- but keep both files
   themselves separate, mirroring `prompt_oracle`/`completion_oracle`
   already being separate modules).

### Syntax highlighting -- not differential

bash has no built-in syntax highlighting to compare against at all (it's
a rustyline/readline-side feature some shells add, not a POSIX/bash
behavior), so a `classify(line: &str) -> Vec<Span>`-shaped function has no
live-oracle shape to test against here. This belongs as a plain, ordinary
Rust unit test suite in whichever crate `classify` lives in, asserting
expected span classifications for representative input (keywords,
strings, comments, ...) -- not this crate's corpus, and not a live
comparison against anything.

### Persistent history -- not differential

The eager-append-to-a-history-file wiring is mostly glue around
rustyline's own history mechanism, with very little pure "core" logic to
differentially test even in principle (bash's own history file format and
exact append timing are also implementation details, not a specified
behavior worth chasing byte-for-byte). A plain integration test -- run a
few commands through conch's history-recording path, then assert on the
resulting history file's contents -- is the right bar here, in
`crates/conch` or `crates/conch-core`'s own test suite, not this crate.

### Startup file sourcing -- not differential, but not novel either

Once a `source_startup_files`-shaped entry point exists, unit-testing it
is straightforward and needs no new harness machinery: build a temp rc
file, point a test `Shell` at it, run the sourcing function, and assert
on the resulting `env_vars`/`aliases`/`functions` -- ordinary state
assertions on a `Shell` the same way `conch-shell-core`'s own existing
`#[cfg(test)]` modules already do (see e.g. `lib.rs`'s
`register_and_look_up_builtin`/`env_var_takes_precedence_over_shell_var`).
This isn't meaningfully comparable against real bash's own
`~/.bashrc`-sourcing behavior for the same file content *as a live
oracle* -- the interesting assertion is "did conch's own internal state
end up right," not "does conch's stdout match bash's," and sourcing
itself is just an ordinary `.`/`source` execution over a fixed path, which
already has full differential coverage for its actual execution semantics
in `corpus/phase5/source_and_eval.toml`. Belongs in `crates/conch-core`/
`crates/conch`'s own test suite, not this crate's corpus.

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

Likewise for Phase 3b once function/`local`/`return`/positional-parameter
execution lands in `crates/conch-parser`/`crates/conch-core`,
substituting `phase3b_differential` and `CONCH_DIFFTEST_STRICT_PHASE3B`.
As of this writing, conch's parser has no `function_definition` grammar
at all and its executor has no positional-parameter shell state, so
`phase3b_differential` reports 2/52 against a built conch binary today.
Both passes are `printf-with-zero-positional-at-is-not-a-valid-zero-
fields-demonstration` (see that case's own doc note in
`at_star_field_splitting.toml`) -- a coincidence, not evidence `$@` is
implemented: conch's placeholder "every positional parameter is
permanently unset" behavior happens to produce the exact same one blank
line real bash/dash produce for a script that legitimately has zero
positional parameters, purely because that specific case can't
distinguish "always unset" from "genuinely empty" in its output. Every
other case fails for one of two confirmed reasons (not a corpus bug):
defining a function fails to *parse* (`conch -c 'f() { echo hi; }; f'` →
`conch: unexpected operator '(' at byte 1, expected a separator (';',
'&', or newline) or end of input`, exit 2), or calling `local`/`return`/
`set`/`shift` tries to exec a same-named external command (`conch -c
'return 3'` → `conch: return: No such file or directory (os error 2)`,
exit 127):

```sh
# Phase 3b full report, report-only (today's state):
cargo test -p conch-difftest --test phase3b_differential -- --nocapture

# Phase 3b hard gate, once warranted:
CONCH_DIFFTEST_STRICT_PHASE3B=1 cargo test -p conch-difftest --test phase3b_differential -- --nocapture
```

Likewise for Phase 4 once job-control execution lands in
`crates/conch-parser`/`crates/conch-core`, substituting `phase4_differential`
and `CONCH_DIFFTEST_STRICT_PHASE4`. As of this writing job control is
being actively implemented concurrently with this corpus -- literally
mid-edit: `cargo build --release` against the working tree at the time
this note was written fails to compile (`crates/conch-core`) -- so unlike
every earlier phase's entry above, there is no single stable pass/fail
count to report here yet. Running against the last binary that *did*
build successfully (a pre-Phase-4 snapshot, where `&` is parsed but
always runs synchronously and `wait`/`trap` aren't registered builtins)
showed three patterns worth knowing about rather than one exact number:

- A meaningful minority of cases **coincidentally pass** -- not because
  job control is implemented, but because every case in this corpus was
  deliberately written to have one, single, deterministic output
  ordering (that's the whole point of leaning on `wait`; see "Phase 4
  scoping notes" above), and running everything synchronously in program
  order happens to reproduce that exact same ordering for a script that
  never actually needed concurrency to begin with. This is the same
  phenomenon Phase 3b's entry above documents for
  `printf-with-zero-positional-at-is-not-a-valid-zero-fields-demonstration`
  -- coincidental output equivalence, not a signal that the feature works.
- Every `trap`-based case reliably fails or errors, confirming `trap`
  isn't a registered builtin yet (consistent with `local`/`return`/`set`/
  `shift` all resolving to "tries to exec a same-named external command"
  at the equivalent point in Phase 3b).
- Both FIFO-gated cases in `jobs_status.toml` hit the harness's own
  `INVOCATION_TIMEOUT` and were reported as clean `Error`s rather than
  hanging the test run -- direct, live confirmation of exactly the
  scenario "Wall-clock timeouts (hang prevention)" above describes,
  observed while building this corpus rather than only reasoned about in
  the abstract.

```sh
# Phase 4 full report, report-only (today's state):
cargo test -p conch-difftest --test phase4_differential -- --nocapture

# Phase 4 hard gate, once warranted:
CONCH_DIFFTEST_STRICT_PHASE4=1 cargo test -p conch-difftest --test phase4_differential -- --nocapture
```

Likewise for Phase 5 once its builtins land in
`crates/conch-parser`/`crates/conch-core`/`crates/conch-builtins`,
substituting `phase5_differential` and `CONCH_DIFFTEST_STRICT_PHASE5`. As
of this writing, unlike Phase 4's entry above, the workspace *does* build
successfully -- Phase 5's builtin set is landing incrementally rather
than all at once, and running against that binary showed 109/175 passing
(`test`/`[`, `printf`, `type`/`command`, `umask`, and most of `unset`/
`source`/`eval` already agree with bash/dash; `read`, `getopts`,
`declare`, `exec`, and `set`'s option flags don't yet -- confirmed by
hand, e.g. `read x` against that binary silently leaves `x` empty rather
than actually consuming stdin). Zero cases hit the harness's own
`INVOCATION_TIMEOUT`, including the `mkfifo`-free stdin-driven `read`
cases and Phase 5's own `kill`-based cases -- worth calling out
specifically because Phase 4's equivalent entry above is the reason that
mechanism exists at all, and this is it staying quiet (as it should)
against a binary that's actually making incremental progress rather than
being completely unimplemented or actively mid-deadlock. Take the exact
109/175 split as a snapshot, not a target to chase here -- this crate's
job is the corpus and harness, not tracking a specific implementation's
day-to-day progress.

```sh
# Phase 5 full report, report-only (today's state):
cargo test -p conch-difftest --test phase5_differential -- --nocapture

# Phase 5 hard gate, once warranted:
CONCH_DIFFTEST_STRICT_PHASE5=1 cargo test -p conch-difftest --test phase5_differential -- --nocapture
```
