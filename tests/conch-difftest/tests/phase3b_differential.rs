//! The real differential suite: conch vs. bash/sh, per the Phase 3b
//! (functions, `local`, `return`, positional parameters) corpus.
//!
//! **Phase 3b execution semantics don't exist in conch yet** -- as of this
//! writing, `crates/conch-parser` has no `function_definition` grammar at
//! all (`ast::Command` is `#[non_exhaustive]` specifically pending it --
//! see that enum's own doc comment) and `crates/conch-core` has no
//! positional-parameter shell state (`$1`/`$2`/`$#`/`$@`/`$*` all expand
//! as if permanently unset -- see `expand.rs`'s own comments on this), so
//! every case in this corpus is expected to fail against a real conch
//! binary today, for one of two confirmed reasons rather than a corpus
//! bug:
//!
//! - any script defining a function fails to *parse* at all, e.g.
//!   `conch -c 'f() { echo hi; }; f'` produces `conch: unexpected operator
//!   '(' at byte 1, expected a separator (';', '&', or newline) or end of
//!   input` (exit code 2) -- confirmed by running it directly.
//! - `local`, `return`, `set`, and `shift` aren't registered builtins yet
//!   (`crates/conch-builtins` only has `cd`/`exit`/`export`/`echo`/`pwd`/
//!   `break`/`continue`), so a script that calls any of them tries to
//!   `exec` a same-named external command instead, e.g.
//!   `conch -c 'return 3'` produces `conch: return: No such file or
//!   directory (os error 2)` (exit code 127) -- also confirmed directly.
//!   Positional-parameter reads (`$1`, `$#`, ...) don't error at all; they
//!   just silently expand to empty/zero, which is why cases that only
//!   *read* positional parameters (no function definition, no `set`/
//!   `shift`/`local`/`return`) fail on a stdout mismatch instead of a
//!   parse error or a 127 exit.
//!
//! This is deliberately **decoupled from `CONCH_DIFFTEST_STRICT`**,
//! `CONCH_DIFFTEST_STRICT_PHASE2`, and `CONCH_DIFFTEST_STRICT_PHASE3`
//! (the earlier phases' hard gates): this test checks its own,
//! differently-named `CONCH_DIFFTEST_STRICT_PHASE3B` instead, which
//! nothing currently sets. That's the whole mechanism -- CI's `difftest`
//! job runs every test in this crate (`cargo test -p conch-difftest`), so
//! this suite runs and reports every time that job does, but flipping an
//! earlier phase's corpus to a hard gate never silently pulls this
//! still-in-progress phase's corpus along with it (and Phase 3b going
//! green later won't require touching any earlier phase's gate either).
//! Once function/`local`/`return`/positional-parameter execution lands
//! and this corpus is expected to be fully green, set
//! `CONCH_DIFFTEST_STRICT_PHASE3B=1` (locally, or in the `difftest` job's
//! `env:`) to make it one, the same way Phase 1's README section "Wiring
//! in real execution" describes for `CONCH_DIFFTEST_STRICT`.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase3b_differential_against_conch() {
    let phase3b = corpus::corpus_root().join("phase3b");
    let cases = corpus::load_dir(&phase3b).expect("phase 3b corpus failed to load");

    let conch_bin = conch_difftest::invoke::find_conch_binary();
    let results = runner::run_differential(&cases, conch_bin.as_deref());
    runner::print_report("differential: phase3b", &results);

    let strict = std::env::var("CONCH_DIFFTEST_STRICT_PHASE3B")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !strict {
        eprintln!(
            "conch-difftest: Phase 3b function/local/return/positional-parameter execution \
             doesn't exist in conch yet, so this suite runs in report-only mode regardless of \
             CONCH_DIFFTEST_STRICT, CONCH_DIFFTEST_STRICT_PHASE2, or CONCH_DIFFTEST_STRICT_PHASE3 \
             (those gate only the phase1/, phase2/, and phase3/ corpora). Set \
             CONCH_DIFFTEST_STRICT_PHASE3B=1 once this evaluation lands and this corpus is \
             expected to be fully green -- see tests/conch-difftest/README.md."
        );
        return;
    }

    let failures: Vec<_> = results
        .iter()
        .filter(|r| !matches!(r.outcome, CaseOutcome::Passed))
        .collect();
    assert!(
        failures.is_empty(),
        "{} case(s) did not pass under CONCH_DIFFTEST_STRICT_PHASE3B=1 -- see the report printed \
         above",
        failures.len()
    );
}
