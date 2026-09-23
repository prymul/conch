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

## There are no entries yet

Phase 1 hasn't run its corpus against a working conch (see
`README.md#status-as-of-this-writing`), so no real divergence has been
found or decided on. This file is a template for when one is.

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
