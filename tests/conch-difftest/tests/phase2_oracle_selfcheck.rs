//! Runs every Phase 2 corpus case's own oracle(s) against a second,
//! independent invocation of themselves (bash vs. bash, sh vs. sh) and
//! asserts they agree.
//!
//! Mirrors `oracle_selfcheck.rs` (Phase 1). This is a **hard gate**
//! despite Phase 2 execution not existing in conch yet: it doesn't touch
//! conch at all, only the corpus's own assumptions about real bash/sh
//! behavior (including every documented bash-only vs. bash-and-sh oracle
//! split -- see each corpus file's header and `../known-differences.md`).
//! A failure here means a corpus case's script or oracle list has a bug,
//! not that conch is non-compliant.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase2_oracles_agree_with_themselves() {
    let phase2 = corpus::corpus_root().join("phase2");
    let cases = corpus::load_dir(&phase2).expect("phase 2 corpus failed to load");

    let results = runner::run_oracle_selfcheck(&cases);
    runner::print_report("oracle self-check: phase2", &results);

    let failures: Vec<_> = results
        .iter()
        .filter(|r| !matches!(r.outcome, CaseOutcome::Passed))
        .collect();

    assert!(
        failures.is_empty(),
        "{} case(s) failed the oracle self-check -- see the report printed above \
         (run with `cargo test -- --nocapture` to see it locally)",
        failures.len()
    );
}
