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

Six real, decided-on-purpose divergences exist so far: KD-0001 through
KD-0003 from Phase 4 (job control), KD-0004/KD-0005 from Phase 5
(builtins completeness), and KD-0006 from Phase 6 (interactive UX --
`PS1`/`PS2` prompt expansion). The two sections after this one are a different,
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
at startup -- the same general shape Shellshock's (CVE-2014-6271)
post-mortem warns about (data an attacker doesn't control gets sent
somewhere, only to have a startup path auto-execute a piece of it as a
command), even though the specific exploitability chain doesn't carry
over one-for-one: unlike Shellshock's bug (a parser that scanned function
definitions out of *every* environment variable indiscriminately, with no
intended boundary at all), a trap-text channel here would be one
specific, internally-named variable read at one known point
(`Shell::new`), the same narrow shape the (safe, already-shipped)
`__CONCH_IGNORED_SIGNALS` ignored-signal-list case already uses -- it
wouldn't inherit Shellshock's specific indiscriminate-environment-scanning
defect just by existing; getting the delimiting right for arbitrary
command text (as opposed to a simple comma-joined signal-name list) is a
solvable, ordinary engineering problem on its own, not evidence the whole
approach is unsound. And, notably, no real capability is actually
withheld from
anyone by this decision: a script can already get the identical effect
without any propagation machinery at all, simply by writing `trap 'cmd'
SIG` as the first line inside its own subshell body. So this is
ultimately a POSIX-fidelity gap, not a load-bearing security boundary --
but the general shape (arbitrary command text, sent through an
environment variable, auto-run by a freshly-started interpreter that
trusts it) is close enough to what Shellshock's category of bug looked
like that this codebase already explicitly declined the analogous
pattern once before, for propagating ordinary shell variables
(`Shell::shell_vars`) into a subshell -- see that same `exec_subshell` doc
comment -- and extending that same caution to trap command-text, rather
than reopening the question from scratch, is the more conservative
default until there's a concrete reason a script actually needs the
propagated form instead of writing the trap inside the subshell directly.
Revisiting this is realistic (unlike a hypothetical fundamentally
different in-process subshell implementation, which would sidestep the
whole question) if that reason ever surfaces.

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

### KD-0003: a background job reported once at an interactive prompt can become un-`wait`-able too soon

**What diverges:** in an *interactive* session, if a backgrounded job
finishes and gets announced by the next prompt's own notification pass
(bash's `[1]+ Done sleep 0.1`-style line) before the script/user gets
around to running `wait "$pid"` on it -- i.e. at least one full
prompt/command boundary passes in between -- a subsequent `wait "$pid"`
fails with `wait: pid N is not a child of this shell` instead of
succeeding.

**bash/sh behavior:** a completed background job stays `wait`-able for a
meaningfully longer window than "until it's been printed once" -- typing
an unrelated command at the prompt in between backgrounding a job and
later `wait`-ing on it does not cause bash to forget the job.

**conch behavior:** [`Shell::purge_finished_notified_jobs`]
(`conch-shell-core::job`) removes a job from the job table as soon as its
current state has been reported *once*, whether that report came from
the `jobs` builtin or the interactive prompt loop's own per-prompt
notification pass (`conch`'s `report_finished_jobs`). Once purged,
[`Shell::wait_for_pid`] has nothing left to look up, so `wait` on that
pid reports it as never having been this shell's child at all, even
though it genuinely was.

**Why:** this is a narrow, interactive-only gap in the *policy* of when a
finished job's table entry gets cleaned up, not a fundamental limitation
of the job-table/`wait` mechanism itself -- unlike KD-0001/KD-0002, this
one is closer to an under-tuned default than a considered design
tradeoff: the "purge after first report" rule was written to match "show
a completed job's status exactly once, then forget it" for `jobs`'
*display* purposes, without separately considering that the same purge
also needs to not race a still-pending explicit `wait` on that same job.
A more bash-like policy (e.g. only purging once a job has actually been
`wait`-ed on, or after some longer grace window, rather than immediately
after its first *display*) would close this, but wasn't implemented as
part of landing the rest of Phase 4 -- flagged and explicitly deferred
rather than fixed in the moment it was found, the same "land this phase
in a good, complete state rather than keep extending it" call already
made for KD-0002. A legitimate candidate for a fast-follow alongside (or
instead of) KD-0002's.

**Case:** not currently represented in the differential corpus and can't
be with the current corpus design -- reaching this gap requires crossing
an *interactive* prompt boundary between backgrounding a job and later
`wait`-ing on it (the corpus's own `-c`/script-mode invocations never go
through the interactive per-prompt notification pass at all, since
that's only wired up in `conch`'s own `run_interactive`, not
`run_source`), so no `-c`-based script can exercise it regardless of how
it's written. Found and confirmed via live `pty`-backed manual testing,
not the differential suite.

### KD-0004: `eval`'s syntax-error handling matches bash's leniency, not dash's abort-on-error

**What diverges:** when the string passed to `eval` fails to parse (a
syntax error), conch does not terminate the rest of the script -- the
`eval` call itself fails with a nonzero status and execution continues
normally afterward, even on the same physical source line.

**bash/sh behavior:** bash matches conch's behavior exactly here: `eval`'s
parse failure is an ordinary, non-fatal error, and the rest of the
script -- including anything joined by `;` on the same line as the
failing `eval` -- runs normally afterward. dash instead takes POSIX
2.14's permitted stricter reading for `eval` (one of POSIX's fourteen
"special built-in" utilities): a special built-in's error aborts the
entire remaining non-interactive script outright, with no exemption for
same-line-joined commands.

**conch behavior:** matches bash's leniency, not dash's abort. This was a
deliberate choice covering `eval` together with `readonly`/`unset`/`set
-u`'s equivalent special-built-in error paths (see
`known-differences.md`'s "Cross-shell quirks worth knowing" section for
the consolidated bash-vs-dash writeup this decision was made in response
to) -- `eval`'s piece of that decision is confirmed landed and pinned
here; the others are tracked separately and may pick up their own KD
entries as each lands.

**Why:** this project has an established precedent of matching bash's
behavior over strict-but-optional POSIX behavior when the two genuinely
diverge and POSIX doesn't mandate the stricter reading (dash's
special-built-in abort is *permitted*, not *required*, by POSIX 2.14).
Aborting an entire non-interactive script on an `eval` parse error is
also simply harsher than most real-world scripts expect or want --
bash's "the failed thing fails, everything else keeps going" model is
the more broadly useful default, and is what the overwhelming majority of
`bash`-targeting (and `sh`-targeting-but-bash-tested) scripts in the wild
already assume.

**Case:** `corpus/phase5/source_and_eval.toml` ->
`eval-syntax-error-matches-bashs-leniency-not-dashs-abort` (a
`known_difference` case, pinning conch's own expected `exit:2\n` stdout
and exit code `0` directly, rather than a live oracle comparison against
dash -- a live comparison for this exact construct would now fail
forever by design once this decision landed, which is precisely the
scenario `known_difference` exists for). The sibling
`eval-syntax-error-is-non-fatal-on-bash` case remains a live `oracles =
["bash"]` comparison, since conch matching bash here means that
comparison keeps being meaningful indefinitely.

### KD-0005: `alias` definitions don't propagate across any re-exec boundary

**What diverges:** an alias defined in the current shell is silently
never expanded inside a subshell, a backgrounded (`&`) list, or a command
substitution, even though all three otherwise run "in the current
shell['s] environment" for every other purpose POSIX cares about here.
No error, no warning -- the raw, unaliased command just runs instead.
Concretely: `alias rm='rm -i'; (rm somefile)` silently runs the bare `rm`
inside the subshell, quietly losing a user's own interactive safety-net
alias.

**bash/sh behavior:** aliases are a property of the shell's own parser
state at the moment a command is read, and a subshell/background job/
command substitution all read their commands using that same, still-live
alias table -- an alias defined before entering any of the three is
visible inside it.

**conch behavior:** never visible inside any of the three, for two
distinct underlying reasons that both land on the identical symptom:

- A subshell (`Parser::parse_subshell`) and a backgrounded list
  (`Parser::parse_optional_separator`'s `Separator::Async` case) both
  capture their own re-exec source text by slicing the *original,
  unmodified* input string by byte offset (`self.source[open.end..close_start]`
  for a subshell, the equivalent `self.source[item_start..amp_start]` for
  an async list) -- see [`conch_shell_parser::SubshellBody::source`]'s
  own doc comment for why the *executor* needs this raw text at all,
  rather than just walking the already-parsed body. `maybe_expand_alias`
  (see [`conch_shell_parser::parse_with_aliases`]'s own docs) only ever
  splices replacement tokens into the parser's *token stream*
  (`self.tokens`) -- it has no reason to, and doesn't, rewrite
  `self.source` itself. So the captured slice is always the literal,
  pre-expansion source text regardless of what alias expansion did to the
  token stream alongside it, and `conch-shell-core::exec`'s re-exec of
  that slice (`conch -c <source>`) runs it through plain, non-alias-aware
  `parse` in a fresh child `Shell` with an empty alias table to begin
  with.
- Command substitution has the same symptom for an even more fundamental,
  Phase-2-vintage reason: its body (`conch_shell_lexer::CommandSubstitution::body`)
  is captured as opaque, unparsed text by the *lexer*, before the parser
  -- and therefore before `maybe_expand_alias`, which only runs during
  parsing -- ever sees a single token of it.

**Why:** not fixed, and not planned as a targeted fix, for the same
category of reason KD-0001 already declined propagating `trap`'s command
text the same way: every one of these three boundaries is a genuinely
separate re-exec'd process (`conch -c <text>`), not an in-process fork of
interpreter state (see
`conch-shell-core::exec::exec_subshell`'s own doc comment for why that's
a hard requirement, not a shortcut), and an alias's replacement text is
exactly as arbitrary, attacker-shaped user data as a trap's command text
is -- propagating it through an environment variable for a freshly
started interpreter to automatically parse and act on at startup is the
same general shape Shellshock's (CVE-2014-6271) post-mortem warns about,
even though (as with KD-0001) the specific exploitability chain doesn't
carry over one-for-one. This is a different category from the
`__CONCH_IGNORED_SIGNALS`/`__CONCH_PID` propagation this same codebase
already does safely: both of those carry a closed, fixed-shape payload
(a signal name from a known enum, a decimal PID) with no room for
arbitrary content, which an alias's replacement text -- ordinary shell
source, by definition -- structurally cannot be narrowed to the same
way. A script that actually needs an alias visible inside one of these
three boundaries already has an exact, zero-propagation-machinery
workaround available: define the alias *inside* the subshell/background
list/command-substitution body directly, the same escape hatch KD-0001
notes for trap text.

**Case:** not currently represented in the differential corpus -- found
via a security-review code trace (`Parser::parse_subshell`/
`parse_optional_separator`'s source-slicing, `CommandSubstitution`'s
lexer-time capture), not a failing test. A live-oracle case is
straightforward to add (`alias rm='rm -i'; (rm --version 2>&1 | head
-1)`-shaped, comparing whether the subshell's own resolved command
reflects the alias) whenever the Phase 5 corpus is next touched.

### KD-0006: `PS1`/`PS2`'s `\s`/`\v`/`\V` escapes report conch's own identity, not bash's

**What diverges:** `\s` (shell name), `\v` (major.minor version), and `\V`
(full version) in an expanded `PS1`/`PS2` report conch's own name and
Cargo package version, not the string `"bash"` or bash's own version.

**bash/sh behavior:** `\s` expands to `"bash"` (or whatever `$0`'s
basename is, for a renamed/symlinked binary); `\v`/`\V` expand to bash's
own compiled-in version (e.g. `5.3`/`5.3.20(1)-release`).

**conch behavior:** `\s` expands to `$0`'s own basename with a leading
`-` stripped (`conch_shell_core::prompt::shell_name`) -- `"conch"` for
every ordinary invocation, since nothing in this project models a
login-shell leading-`-` convention of its own. `\v`/`\V` expand to
`env!("CARGO_PKG_VERSION_MAJOR").env!("CARGO_PKG_VERSION_MINOR")`/
`env!("CARGO_PKG_VERSION")` respectively -- conch's own crate version at
build time.

**Why:** these three escapes exist specifically to answer "which shell,
and which version of it, am I looking at" -- correctly reporting conch's
own identity here isn't a shortcut or a gap, it's the entire point of the
escape existing at all. Matching bash's literal `"bash"`/bash's own
version string would be actively wrong (indistinguishable from actually
running bash), not more "compatible."

**Case:** `corpus/phase6/prompt/prompt_expansion.toml` ->
`shell-name-escape-is-a-known-difference` (`\s`, pinned via
`known_difference` against the fixed, permanent literal `"conch"`). `\v`/
`\V` are deliberately **not** a corpus case: the expected string changes
on every version bump (this project's release process re-versions
automatically on every push to `main` -- see `CLAUDE.md`), and
`known_difference.expect` is a static, hand-pinned TOML string with no
mechanism to track that automatically -- documented here in prose
instead, the same "not every divergence needs a corpus case" precedent
KD-0002/KD-0003 already established.

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
| `. file arg1 arg2` passing extra arguments as the sourced file's own positional parameters -- also, like `trap -p` above, actually POSIX-specified rather than a bash extension, included here purely because dash's practical non-support makes every corpus case around it `oracles = ["bash"]` only | `$1`/`$2`/etc. inside the sourced file are simply empty -- dash accepts the extra arguments syntactically but never binds them to anything, confirmed empirically | `corpus/phase5/source_and_eval.toml` |
| `type -t name` (single-word machine-readable output: one of builtin/file/function/alias/keyword) | Doesn't recognize `-t` as a flag at all -- treats it as a command name to look up in its own right (`-t: not found`), printed as a spurious extra line before still answering the real query in `type`'s ordinary human-readable prose form | `corpus/phase5/type_and_command.toml` |

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

- **POSIX "special built-in" errors abort the whole script on dash, but
  are often just an ordinary non-fatal error on bash.** POSIX 2.14 lists
  fourteen "special built-ins" (`.`, `:`, `break`, `continue`, `eval`,
  `exec`, `exit`, `export`, `readonly`, `return`, `set`, `shift`, `times`,
  `trap`, `unset`) and permits (without requiring) a non-interactive
  shell to terminate immediately if one of them encounters certain kinds
  of error -- e.g. an assignment error, or, more broadly on some shells,
  any usage error at all. dash consistently takes the strict reading:
  `readonly`/`unset` reassignment-of-readonly errors, `set -u`'s
  unset-variable-reference error, and `eval`'s own parse errors on
  malformed input all abort the *entire remaining script* on dash, even
  when the failing call and the very next command are joined on the same
  physical line by `;`. bash is considerably more lenient across this
  same set: an `eval` parse error or an `unset`-on-readonly failure are
  just ordinary non-fatal errors that don't stop the script at all; a
  plain `x=6` reassignment of a readonly `x`, or a `set -u` violation,
  *do* abort -- but only the current physical source line, with
  execution resuming normally on the next one (this is the same
  same-line-vs-next-line asymmetry already documented above for
  `${var:?message}` and arithmetic division by zero -- those are both
  instances of this exact same general rule, now confirmed to extend
  well beyond parameter/arithmetic expansion errors). conch's own
  decision across this whole set is to match bash's leniency, not dash's
  strictness -- `eval`'s piece of that decision has landed and is pinned
  as **KD-0004** (see above); `readonly`/`unset`/`set -u`'s pieces are
  still tracked here as live-oracle-comparison corpus cases
  (`corpus/phase5/declare_and_readonly.toml`,
  `corpus/phase5/set_options.toml`, narrowing `compare` to `["stdout"]`
  where a same-line comparison still agrees except for the reported exit
  code, or splitting into a dedicated `oracles = ["bash"]` case) pending
  the same fix landing for each of them and picking up their own KD entry
  in turn.

- **`getopts`/`OPTIND` interaction across two independent, unrelated
  parses without an explicit `OPTIND=1` reset differs.** Reusing
  `getopts` for a second, unrelated argument list (e.g. inside a second
  call to a function that parses its own `"$@"` with `getopts`) without
  first resetting `OPTIND=1` finds nothing at all on bash (OPTIND is
  still pointing past the end of the *first* argument list), but finds
  the option again on dash regardless. The correct, portable pattern
  (`OPTIND=1` before reusing `getopts`) is unaffected and produces
  identical output on both shells -- see
  `corpus/phase5/getopts.toml`'s `getopts-optind-must-be-reset-to-reuse-
  across-two-independent-parses` for that pattern, and its bash-only
  `...-without-optind-reset-sees-nothing-on-a-second-call-bash-only` for
  the divergence itself.

- **A handful of builtin usage/lookup errors agree on producing no
  stdout, but disagree on the specific nonzero exit code.** `command -v`
  of an unresolvable name (bash: 1, dash: 127) and `readonly`/`unset`
  reassignment errors on the same physical line (bash: 1, dash: 2 --
  see the special-built-ins entry above) are both real, confirmed
  instances of this: both shells agree completely on *content* (nothing
  printed to stdout), just not on the numeric status. Every affected
  corpus case narrows `compare` to `["stdout"]` rather than dropping to a
  single-shell oracle, since the stdout-level fact being tested is
  genuinely shared.

- **Non-interactive alias expansion is enabled by default on dash, but
  requires an explicit opt-in on bash.** POSIX leaves whether a
  non-interactive shell expands aliases at all as implementation-defined,
  and the two oracle shells this project uses landed on opposite
  defaults: dash expands an alias defined on an earlier line by default,
  with nothing to configure; bash does the same only after
  `shopt -s expand_aliases` is set first (a bash-only mechanism -- there
  is no dash equivalent to point at, since dash never needed one). Both
  shells agree, with no configuration needed at all, that an alias
  defined and used on the *same* physical line never expands regardless
  (alias substitution happens while that line is still being parsed,
  before the later-in-the-same-line definition is known). See
  `corpus/phase5/alias.toml` for the full set: one shared case for the
  same-line rule, an `oracles = ["bash"]` case showing the opt-in
  requirement, and an `oracles = ["sh"]` case showing dash's default-on
  behavior for the identical script.

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
