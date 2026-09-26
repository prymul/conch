# Known differences from bash/sh

A living record of every place conch **deliberately and permanently**
diverges from bash and/or POSIX `sh` behavior, following the pattern
oils-for-unix uses in its own `doc/known-differences.md`: so a future
differential-test failure can be triaged against "known, intentional"
versus "actual regression" without re-litigating a decision that was
already made on purpose.

**This is not a list of unimplemented features.** A construct conch
hasn't built yet (most of the shell language, as of Phase 1) is not a
"known difference" -- it's just not done yet, and doesn't belong here.
Only add an entry once a divergence has been *decided*: conch will never
match bash/sh for this specific case, and here's why.

## Deliberate conch-vs-bash/sh divergences

Two real, decided-on-purpose divergences exist so far, both from Phase 4
(job control). The two sections after this one are a different,
complementary kind of record: not conch decisions at all, but
divergences *between the two oracle shells themselves* (bash and dash),
found and verified while building the Phase 2 (`corpus/phase2/`)
word-expansion corpus. They exist so a case's `oracles = ["bash"]`
(rather than `["bash", "sh"]`) is never mistaken for an oversight, and so
nobody re-derives these from scratch once conch actually has to decide
which one (if either) to match.

### KD-0001: `trap`'s command-text action doesn't propagate into a subshell

**What diverges:** a `trap 'command text' SIG` registered in a shell
before it enters a `(...)` subshell (or an async list, or command
substitution -- POSIX 2.12's "subshell environment" triggers generally)
is not visible inside it: `trap -p SIG` run inside the subshell reports
no trap registered at all, rather than the parent's.

**bash/sh behavior:** POSIX 2.9.3.1 -- and confirmed empirically against
real bash -- a subshell environment inherits the parent's traps,
including command-text ones; `trap -p SIG` inside a subshell reports the
same action the parent had.

**conch behavior:** a subshell (and every other Phase 4 re-exec site:
async lists, command substitution) starts with a completely empty trap
table for any signal that has a real `TrapAction::Command`. The one
exception: `trap '' SIG` (an *ignored* signal -- no command text at all)
*does* correctly propagate, via a dedicated, inert
`__CONCH_IGNORED_SIGNALS` environment variable that only ever carries a
comma-joined list of signal names -- POSIX mandates this one specifically,
and it carries zero code-execution risk, so it isn't part of this
divergence.

**Why:** every conch subshell/async-job/command-substitution is
implemented by re-exec'ing `conch -c <source>` as a genuinely separate
process (see `conch-shell-core::exec::exec_subshell`'s own doc comment),
not by an in-process fork of interpreter state. Propagating a `trap`'s
*command text* to that freshly-started process would require passing it
through the environment for the child to automatically parse and execute
at startup -- which is structurally the exact same shape CVE-2014-6271
(Shellshock) exploited (a command/function definition smuggled through an
environment variable and auto-run by a newly-started shell that trusted
it). This codebase already explicitly declined that same pattern for
propagating ordinary shell variables (`Shell::shell_vars`) into a
subshell, for the identical reason -- see that same `exec_subshell` doc
comment. This is a deliberate security tradeoff, not an oversight, and
not expected to change without a fundamentally different (in-process, not
re-exec-based) subshell implementation.

**Case:** `corpus/phase4/trap.toml` ->
`trap-set-in-parent-is-visible-inside-a-subshell-bash-only` (currently
still `oracles = ["bash"]`, i.e. compared live against a real bash rather
than pinned via `known_difference` -- the schema for pinning it exists
and is ready to use once someone wires it up; not done as part of this
entry to avoid touching the Phase 4 corpus, which was built in parallel
by a different contributor).

### KD-0002: a foreground multi-stage pipeline doesn't get one shared process group

**What diverges:** `cmd1 | cmd2` run in the **foreground** (no trailing
`&`) doesn't become a single tracked job with one process group the way
every other Phase 4 job-control case does -- pressing Ctrl-Z while such a
pipeline is running stops whichever individual stage is currently the
live OS process, but that stop is not reflected in `jobs`/resumable via
`fg`/`bg` as one coherent pipeline-wide job.

**bash/sh behavior:** every process in a pipeline -- foreground or
background, one stage or many -- shares one process group, and the whole
pipeline is one job as far as `jobs`/`fg`/`bg`/Ctrl-Z are concerned.

**conch behavior:** a *single* foreground external command gets its own
process group correctly (the overwhelmingly common interactive case:
`ls`, `vim file`, `sleep 30` then Ctrl-Z all work exactly like bash). Any
**backgrounded** pipeline, of any stage count, also gets this correctly
(the whole pipeline runs inside one re-exec'd wrapper process, which
itself has one process group, so every stage naturally inherits it).
Only a pipeline that is both **foreground** and has **two or more
stages** falls back to conch's pre-Phase-4 behavior: each stage still
spawns without any process-group assignment at all, connected via the
existing in-memory stdout/stdin buffering between stages (see this
module's own "Known Phase 1 simplification" docs on pipeline execution).

**Why:** closing this gap correctly needs either (a) real concurrent
OS-pipe wiring between pipeline stages (the larger, already-documented
Phase 1 architectural simplification this project has carried forward
since Phase 1, and which a prior design review for this phase explicitly
decided is out of scope to also tackle here), or (b) a smaller scheme
where each stage's spawn lazily joins a process group established by the
pipeline's first stage, layered onto the existing sequential/buffered
stage execution without requiring (a). Neither was implemented in the
Phase 4 pass that added every other piece of job control, as a deliberate
scope decision to land the rest of job control in a complete, well-tested
state rather than extend scope further -- this is the one piece of "each
pipeline/job gets its own process group" not yet delivered. Unlike
KD-0001, this is not a permanent design stance on principle -- it's a
legitimate candidate for a smaller, later fast-follow (option (b) above)
-- but it's a real, currently-true divergence in the meantime, not merely
an unimplemented feature with no decision behind it: the decision made
was to ship the rest of Phase 4 without it rather than block on it.

**Case:** not currently represented in the differential corpus -- the
underlying behavior (does Ctrl-Z during a running foreground pipeline
correctly suspend it as one resumable job) is real-time,
signal-timing-dependent interactive behavior, which this corpus's own
stdout-diffing approach doesn't attempt to capture (see `README.md`'s
notes on what Phase 4 is and isn't realistically differential-testable
this way). Verified instead via live `pty`-backed manual testing during
Phase 4 development, for the cases this gap does *not* affect (single
foreground command, any backgrounded pipeline) -- both confirmed correct
that way; the gap itself (foreground multi-stage pipeline) was reasoned
through from the implementation, not separately reproduced live.

## Bash extensions not in the POSIX baseline

Every construct below is something bash supports that POSIX `sh` (and
dash, the `sh` used as this repo's POSIX oracle) does not -- dash either
leaves the syntax as literal text or raises a parse error. Every
corresponding corpus case therefore uses `oracles = ["bash"]` only; there
is no dash behavior to agree with. This is **not** a statement that conch
won't implement these -- the Phase 2 plan explicitly includes brace
expansion, which is on this very list -- it's purely a note on why these
cases can't be (and shouldn't be made to look like they're) checked
against two oracles.

| Construct | dash's behavior instead | Corpus |
|---|---|---|
| Brace expansion: `{a,b,c}`, `{1..5}`, `{1..10..2}`, `{a..e}` | Passed through as literal text -- no expansion at all | `corpus/phase2/brace_expansion.toml` (whole file) |
| Tilde `~+` / `~-` (expand to `$PWD` / `$OLDPWD`) | Left as literal `~+`/`~-` text | `corpus/phase2/tilde_expansion.toml` |
| Case-modifying parameter expansion: `${var^}`, `${var^^}`, `${var,}`, `${var,,}` (with or without a match pattern) | `Bad substitution` error | `corpus/phase2/parameter_expansion.toml` |
| Substring parameter expansion: `${var:offset}`, `${var:offset:length}`, including negative offsets | `Bad substitution` error | `corpus/phase2/parameter_expansion.toml` |
| Pattern-substitution parameter expansion: `${var/pat/rep}`, `${var//pat/rep}`, `${var/#pat/rep}`, `${var/%pat/rep}` | `Bad substitution` error | `corpus/phase2/parameter_expansion.toml` |
| Indirect parameter expansion: `${!var}` | `Bad substitution` error | `corpus/phase2/parameter_expansion.toml` |
| Plain (non-arithmetic-context) `+=` assignment, e.g. `x+=3` as a standalone statement | `x+=3: not found` -- dash tries to run it as a command, since `+=` isn't assignment syntax to it at all | not currently in the corpus (arithmetic `$((x+=3))`, which *is* POSIX baseline, is; see `arithmetic_expansion.toml`) |
| Arithmetic `**` (exponentiation) | `expecting primary` parse error | `corpus/phase2/arithmetic_expansion.toml` |
| Arithmetic `++`/`--` (pre/post increment/decrement) | `expecting primary` parse error | `corpus/phase2/arithmetic_expansion.toml` |
| Arithmetic comma operator `(a,b,c)` | `expecting ')'` parse error | `corpus/phase2/arithmetic_expansion.toml` |
| Arithmetic: a non-numeric variable value is recursively treated as another variable's *name* (e.g. `x=abc; $((x+1))` looks up `abc`, finds it unset, uses 0) | `Illegal number: abc` -- an immediate error, no recursive lookup | `corpus/phase2/arithmetic_expansion.toml` |
| Glob bracket-expression `^` as a negation synonym for `!` (a glibc `fnmatch()` extension bash's linked libc happens to support) | `^` is an ordinary literal character inside the bracket set -- POSIX only defines `!` for negation | `corpus/phase2/globbing.toml` |
| `function fname { ...; }` (and `function fname() { ...; }`) as an alternate function-definition keyword syntax alongside POSIX's `fname() compound_command` | Treats `function`, the function name, and the literal `{` as ordinary words -- an attempt to run a command named `function` (`function: not found`) -- then runs the body's own commands as plain top-level statements with no group around them at all, and finally hits a syntax error on the orphaned closing `}` (confirmed: dash exits 2, having never reached the intended function call) | `corpus/phase3b/functions.toml` |
| `trap -p SIG` (print the current trap action for a signal without triggering it) -- note this one is *not* actually a bash-beyond-POSIX extension the way every other row here is: POSIX's own `trap` utility description specifies `-p`. It's included in this table anyway because the practical effect on corpus cases is identical (dash has no `-p` support at all to agree with, so those cases are `oracles = ["bash"]` only) | `dash: trap: Illegal option -p` -- an immediate usage error, confirmed empirically; dash's `trap` implements no introspection flag at all | `corpus/phase4/trap.toml` |

## Cross-shell quirks worth knowing (neither shell is simply "wrong")

These aren't bash extensions -- both shells implement the relevant POSIX
feature -- but their behavior at the edges (mostly: exactly what happens
when an expansion produces an error partway through a command) was found
to genuinely differ, sometimes in ways that don't even depend on which
shell it is so much as *how* the script was written. Corpus cases
affected either drop to `oracles = ["bash"]` (when the divergence is in
actual stdout content, not just exit code/stderr) or narrow `compare` to
just the target(s) that do agree.

- **`${var:?message}` on an unset variable: exit code varies by shell
  *and by bash's own invocation mode*.** bash aborts the script (nothing
  after the failing expansion runs) with exit code 127 when the script
  came in via `-c '...'`, but exit code 1 when the identical script runs
  from a file. dash aborts the same way but with exit code 2, regardless
  of invocation mode. stdout produced before the failing expansion is the
  one thing every combination agrees on. See
  `corpus/phase2/parameter_expansion.toml`'s
  `param-error-if-unset-suppresses-rest-of-script`, which is why it
  narrows `compare` to `["stdout"]`.

- **Division by zero: dash halts the script, bash may not.** Both shells
  treat `$((1/0))` as an error, but what happens to the *rest of the
  script* differs, and for bash it further depends on whether the
  remaining commands are on the same source line (joined by `;`) as the
  failing one or on a later line: bash aborts entirely for the `;`-joined
  case, but only aborts the *current* command and continues to the next
  line otherwise. dash aborts the whole script in both cases. This is a
  difference in actual stdout content, not just exit code, so
  `corpus/phase2/arithmetic_expansion.toml`'s
  `arithmetic-division-by-zero-halts-dash-but-not-bash-on-the-next-line`
  is `oracles = ["bash"]` only rather than narrowing `compare`.

- **`return` outside any function: bash reports a usage error and keeps
  going; dash treats it as an implicit `exit` for the whole invocation.**
  Both shells reject a bare `return` at the top level in some sense (it
  isn't a function call, so "return to the caller" is meaningless), but
  they disagree on what that rejection actually does. bash prints
  `` return: can only `return' from a function or sourced script `` to
  stderr, sets `$?` to 2 for that one command, and then continues
  executing the rest of the script exactly as if that line had been a
  no-op. dash instead treats the top-level script (`-c '...'` or a file,
  either way) as though it were itself a sourced script, so `return n`
  behaves like `exit n`: it terminates immediately with exit code `n`,
  and nothing after it -- even later on the same source line -- ever
  runs. This is a difference in actual stdout content (bash's remaining
  output still appears; dash's doesn't), so
  `corpus/phase3b/return_and_exit_status.toml`'s
  `return-outside-any-function-is-an-error-in-bash` is `oracles =
  ["bash"]` only rather than narrowing `compare`.

- **`jobs -p` run through a pipe or command substitution sees the job on
  bash, sees nothing at all on dash.** Both `jobs -p | wc -l` and
  `n=$(jobs -p)` put the `jobs` builtin on the producer/read side of a
  subshell environment (POSIX defines both pipeline stages and command
  substitution that way). bash's forked subshell still has visibility
  into the parent's job table at the moment it forks, so `jobs -p` there
  still reports the running background job; dash's does not, and reports
  none at all -- confirmed empirically, not a formatting difference, an
  actual presence-vs-absence difference. `corpus/phase4/jobs_status.toml`
  works around this entirely by redirecting `jobs -p`'s output straight to
  a file (an ordinary redirect creates no subshell on either shell) and
  reading the count back separately, rather than dropping to `oracles =
  ["bash"]`, since the underlying "is job control's job table observable"
  question isn't actually what those cases are testing.

- **`jobs -p`'s bookkeeping for when a job disappears after being
  explicitly `wait`-ed on differs.** After `wait "$pid"` returns, bash's
  `jobs -p` no longer lists that pid at all; dash's still does. POSIX
  doesn't pin down exactly when a completed job must be removed from the
  list, so neither behavior is non-compliant -- but it means no
  `corpus/phase4/jobs_status.toml` case asserts anything about `jobs -p`'s
  output *after* a `wait`; `kill -0 $pid` is used instead for any
  liveness check that needs to hold both before and after, since both
  shells agree on that precisely.

## Entry format

Each entry gets a stable ID (`KD-0001`, `KD-0002`, ...) referenced from
the corresponding corpus case's `known_difference.id` field:

```markdown
### KD-0001: <short title>

**What diverges:** <precise description of the differing behavior>

**bash/sh behavior:** <what a real oracle shell does>

**conch behavior:** <what conch does instead>

**Why:** <the actual reason -- a POSIX-vs-bash-mode design choice, a
deliberately simplified/stricter behavior, an extension bash doesn't
have that conch's own docs promise, etc. "Wasn't implemented yet" is
never a valid reason here.>

**Case:** `corpus/phase1/<file>.toml` -> `<case-name>`
```

## Candidate divergences to watch for (not yet decided)

Noted here as an aid for whoever eventually resolves them -- these are
open questions, not entries. Once a decision is made, either promote the
relevant one into a real numbered entry with a corpus case, or delete it
from this list if conch ends up matching bash/sh after all.

- **`echo -n`/`echo -e` flag support.** bash's builtin `echo` supports
  both; POSIX `sh`'s `echo` behavior here is famously
  implementation-defined and disagrees with bash's even before conch
  enters the picture. conch's own flag behavior isn't decided yet.
