//! Runs every Phase 3 corpus case's own oracle(s) against a second,
//! independent invocation of themselves (bash vs. bash, sh vs. sh) and
//! asserts they agree.
//!
//! Mirrors `oracle_selfcheck.rs` (Phase 1) / `phase2_oracle_selfcheck.rs`.
//! This is a **hard gate** despite Phase 3 execution not existing in conch
//! yet: it doesn't touch conch at all, only the corpus's own assumptions
//! about real bash/sh control-flow behavior (if/elif/else, for, while,
//! until, break/continue with a numeric level, case, subshells, and brace
//! groups). A failure here means a corpus case's script has a bug -- most
//! plausibly a `break`/`continue` level miscounted into an infinite loop,
//! see `corpus/phase3/nested_loops.toml`'s header -- not that conch is
//! non-compliant.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase3_oracles_agree_with_themselves() {
    let phase3 = corpus::corpus_root().join("phase3");
    let cases = corpus::load_dir(&phase3).expect("phase 3 corpus failed to load");

    let results = runner::run_oracle_selfcheck(&cases);
    runner::print_report("oracle self-check: phase3", &results);

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
