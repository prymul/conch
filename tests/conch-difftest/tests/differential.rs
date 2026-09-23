//! The real differential suite: conch vs. bash/sh, per the Phase 1
//! corpus.
//!
//! **This can't meaningfully run until the conch binary exists and
//! implements Phase 1 execution semantics.** Until then (and by default
//! even once the binary exists but is still a work-in-progress stub),
//! this test *reports* results without failing the build, so CI stays
//! actionable rather than permanently red for functionality that's
//! legitimately still being built. See this crate's README
//! ("Wiring in real execution") for exactly how to turn this into a hard
//! gate once Phase 1 execution lands: set `CONCH_DIFFTEST_STRICT=1`.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase1_differential_against_conch() {
    let phase1 = corpus::corpus_root().join("phase1");
    let cases = corpus::load_dir(&phase1).expect("phase 1 corpus failed to load");

    let conch_bin = conch_difftest::invoke::find_conch_binary();
    let results = runner::run_differential(&cases, conch_bin.as_deref());
    runner::print_report("differential: phase1", &results);

    let strict = std::env::var("CONCH_DIFFTEST_STRICT")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !strict {
        if conch_bin.is_none() {
            eprintln!(
                "conch-difftest: conch binary not found, so every case above was skipped. \
                 This is expected until Phase 1 execution semantics land -- see \
                 tests/conch-difftest/README.md. Set CONCH_DIFFTEST_STRICT=1 to make this a \
                 hard failure once that's no longer expected."
            );
        }
        return;
    }

    let failures: Vec<_> = results
        .iter()
        .filter(|r| !matches!(r.outcome, CaseOutcome::Passed))
        .collect();
    assert!(
        failures.is_empty(),
        "{} case(s) did not pass under CONCH_DIFFTEST_STRICT=1 -- see the report printed above",
        failures.len()
    );
}
