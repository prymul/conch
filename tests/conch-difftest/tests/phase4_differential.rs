//! The real differential suite: conch vs. bash/sh, per the Phase 4
//! (job control) corpus.
//!
//! **Phase 4 execution semantics don't exist in conch yet** -- as of this
//! writing, job control (backgrounding with `&`, `wait`, `$!`, `jobs`,
//! `trap`, `fg`/`bg`) is still being implemented in parallel with this
//! corpus. Every case below is therefore expected to `Skip` (no conch
//! binary) or `Fail`/`Error` (a conch binary exists but doesn't implement
//! this yet) for a while, exactly the same report-only, non-blocking
//! state every earlier phase's `*_differential.rs` was in before its own
//! execution semantics landed.
//!
//! This is deliberately **decoupled from every earlier phase's strict
//! gate** (`CONCH_DIFFTEST_STRICT`, `_PHASE2`, `_PHASE3`, `_PHASE3B`):
//! this test checks its own, differently-named
//! `CONCH_DIFFTEST_STRICT_PHASE4` instead, which nothing currently sets.
//! That's the whole mechanism -- CI's `difftest` job runs every test in
//! this crate (`cargo test -p conch-difftest`), so this suite runs and
//! reports every time that job does, but flipping an earlier phase's
//! corpus to a hard gate never silently pulls this still-in-progress
//! phase's corpus along with it (and Phase 4 going green later won't
//! require touching any earlier phase's gate either). Once Phase 4
//! execution lands and this corpus is expected to be fully green, set
//! `CONCH_DIFFTEST_STRICT_PHASE4=1` (locally, or in the `difftest` job's
//! `env:`) to make it one, the same way Phase 1's README section "Wiring
//! in real execution" describes for `CONCH_DIFFTEST_STRICT`.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase4_differential_against_conch() {
    let phase4 = corpus::corpus_root().join("phase4");
    let cases = corpus::load_dir(&phase4).expect("phase 4 corpus failed to load");

    let conch_bin = conch_difftest::invoke::find_conch_binary();
    let results = runner::run_differential(&cases, conch_bin.as_deref());
    runner::print_report("differential: phase4", &results);

    let strict = std::env::var("CONCH_DIFFTEST_STRICT_PHASE4")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !strict {
        eprintln!(
            "conch-difftest: Phase 4 job-control execution doesn't exist in conch yet, so this \
             suite runs in report-only mode regardless of any earlier phase's strict gate (those \
             gate only their own corpora). Set CONCH_DIFFTEST_STRICT_PHASE4=1 once Phase 4 \
             evaluation lands and this corpus is expected to be fully green -- see \
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
        "{} case(s) did not pass under CONCH_DIFFTEST_STRICT_PHASE4=1 -- see the report printed \
         above",
        failures.len()
    );
}
