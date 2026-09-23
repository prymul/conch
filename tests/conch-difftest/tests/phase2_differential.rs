//! The real differential suite: conch vs. bash/sh, per the Phase 2
//! (word expansion) corpus.
//!
//! **Phase 2 execution semantics don't exist in conch yet** -- as of this
//! writing, `crates/conch-parser`'s expansion-operator/arithmetic parsing
//! and `crates/conch-core`'s evaluator wiring are both still in progress
//! (see the Phase 2 plan). Every case below is therefore expected to
//! `Skip` (no conch binary) or `Fail`/`Error` (a conch binary exists but
//! doesn't implement this yet) for a while, exactly the same
//! report-only, non-blocking state `differential.rs` was in before Phase
//! 1 execution landed.
//!
//! This is deliberately **decoupled from `CONCH_DIFFTEST_STRICT`**,
//! Phase 1's hard gate (see `differential.rs` and
//! `.github/workflows/ci.yml`'s `difftest` job, which sets it
//! unconditionally): this test checks its own, differently-named
//! `CONCH_DIFFTEST_STRICT_PHASE2` instead, which nothing currently sets.
//! That's the whole mechanism -- CI's `difftest` job runs every test in
//! this crate (`cargo test -p conch-difftest`), so this suite runs and
//! reports every time that job does, but flipping Phase 1's corpus to a
//! hard gate never silently pulls Phase 2's still-failing corpus along
//! with it. Once Phase 2 evaluation lands and this corpus is expected to
//! be fully green, set `CONCH_DIFFTEST_STRICT_PHASE2=1` (locally, or in
//! the `difftest` job's `env:`) to make it one, the same way Phase 1's
//! README section "Wiring in real execution" describes for
//! `CONCH_DIFFTEST_STRICT`.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase2_differential_against_conch() {
    let phase2 = corpus::corpus_root().join("phase2");
    let cases = corpus::load_dir(&phase2).expect("phase 2 corpus failed to load");

    let conch_bin = conch_difftest::invoke::find_conch_binary();
    let results = runner::run_differential(&cases, conch_bin.as_deref());
    runner::print_report("differential: phase2", &results);

    let strict = std::env::var("CONCH_DIFFTEST_STRICT_PHASE2")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !strict {
        eprintln!(
            "conch-difftest: Phase 2 word-expansion execution doesn't exist in conch yet, so \
             this suite runs in report-only mode regardless of CONCH_DIFFTEST_STRICT (Phase 1's \
             gate covers only the phase1/ corpus). Set CONCH_DIFFTEST_STRICT_PHASE2=1 once Phase \
             2 evaluation lands and this corpus is expected to be fully green -- see \
             tests/conch-difftest/README.md."
        );
        return;
    }

    let failures: Vec<_> = results
        .iter()
        .filter(|r| !matches!(r.outcome, CaseOutcome::Passed))
        .collect();
    assert!(
        failures.is_empty(),
        "{} case(s) did not pass under CONCH_DIFFTEST_STRICT_PHASE2=1 -- see the report printed \
         above",
        failures.len()
    );
}
