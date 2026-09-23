//! Runs every corpus case's own oracle(s) against a second, independent
//! invocation of themselves (bash vs. bash, sh vs. sh) and asserts they
//! agree.
//!
//! This validates the harness's entire plumbing -- process spawning per
//! invocation mode, byte-safe capture, normalization (including workdir
//! substitution), and the comparison/diff logic -- using a real,
//! deterministic shell as a stand-in for conch. It requires `bash` and
//! `sh` on `PATH` (present by default on the CI runner and on any normal
//! dev machine) but does *not* require the conch binary to exist yet.
//!
//! A failure here means the harness itself has a bug, or a corpus case
//! made an incorrect assumption about bash/sh behavior -- not that conch
//! is non-compliant. See `tests/differential.rs` for the conch-vs-oracle
//! comparison.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase1_oracles_agree_with_themselves() {
    let phase1 = corpus::corpus_root().join("phase1");
    let cases = corpus::load_dir(&phase1).expect("phase 1 corpus failed to load");

    let results = runner::run_oracle_selfcheck(&cases);
    runner::print_report("oracle self-check: phase1", &results);

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
