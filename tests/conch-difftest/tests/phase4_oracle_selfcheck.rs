//! Runs every Phase 4 corpus case's own oracle(s) against a second,
//! independent invocation of themselves (bash vs. bash, sh vs. sh) and
//! asserts they agree.
//!
//! Mirrors `oracle_selfcheck.rs` (Phase 1) / `phase3_oracle_selfcheck.rs`.
//! This is a **hard gate** despite Phase 4 execution not existing in
//! conch yet: it doesn't touch conch at all, only the corpus's own
//! assumptions about real bash/sh job-control behavior (backgrounding
//! with `&`, `wait`, `$!`, `jobs -p`, `kill -0`, and `trap`). A failure
//! here means a corpus case's script has a bug, or -- for this phase
//! specifically -- possibly that the invoking machine's shells disagree
//! with what was verified while building this corpus (see
//! `corpus/phase4/*.toml` headers for the exact cross-shell divergences
//! already found and worked around), not that conch is non-compliant.
//!
//! Job control is also the first phase whose scripts can plausibly hang
//! (a background job synchronized on a `mkfifo` gate whose writer-side
//! command never runs, if a script has a bug) rather than simply produce
//! the wrong output -- see `invoke::wait_with_timeout`'s doc comment for
//! the bounded-wall-clock-timeout mechanism that turns a hang into a
//! reported `CaseOutcome::Error` instead of blocking this test (and
//! therefore CI) indefinitely.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase4_oracles_agree_with_themselves() {
    let phase4 = corpus::corpus_root().join("phase4");
    let cases = corpus::load_dir(&phase4).expect("phase 4 corpus failed to load");

    let results = runner::run_oracle_selfcheck(&cases);
    runner::print_report("oracle self-check: phase4", &results);

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
