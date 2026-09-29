//! Runs every Phase 3b corpus case's own oracle(s) against a second,
//! independent invocation of themselves (bash vs. bash, sh vs. sh) and
//! asserts they agree.
//!
//! Mirrors `oracle_selfcheck.rs` (Phase 1) / `phase3_oracle_selfcheck.rs`.
//! This is a **hard gate** despite Phase 3b execution not existing in
//! conch yet: it doesn't touch conch at all, only the corpus's own
//! assumptions about real bash/sh function, `local`, `return`, and
//! positional-parameter behavior. A failure here means a corpus case's
//! script has a bug, not that conch is non-compliant.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase3b_oracles_agree_with_themselves() {
    let phase3b = corpus::corpus_root().join("phase3b");
    let cases = corpus::load_dir(&phase3b).expect("phase 3b corpus failed to load");

    let results = runner::run_oracle_selfcheck(&cases);
    runner::print_report("oracle self-check: phase3b", &results);

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
