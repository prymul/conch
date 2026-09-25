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

## There are no KD-numbered entries yet

No genuinely *deliberate, permanent* conch-vs-bash/sh divergence has been
found or decided on yet (see `README.md#status-as-of-this-writing`) --
including any output mismatch the differential suite turns up along the
way, which is a bug/regression to fix, not a candidate for this section,
until someone actually decides conch should keep behaving that way on
purpose. The two sections below are a different, complementary kind of
record: not conch decisions at all, but
divergences *between the two oracle shells themselves* (bash and dash),
found and verified while building the Phase 2 (`corpus/phase2/`)
word-expansion corpus. They exist so a case's `oracles = ["bash"]`
(rather than `["bash", "sh"]`) is never mistaken for an oversight, and so
nobody re-derives these from scratch once conch actually has to decide
which one (if either) to match.

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
