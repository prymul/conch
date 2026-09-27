//! Runs every Phase 5 corpus case's own oracle(s) against a second,
//! independent invocation of themselves (bash vs. bash, sh vs. sh) and
//! asserts they agree.
//!
//! Mirrors `oracle_selfcheck.rs` (Phase 1) / `phase4_oracle_selfcheck.rs`.
//! This is a **hard gate** despite Phase 5 execution not existing in
//! conch yet: it doesn't touch conch at all, only the corpus's own
//! assumptions about real bash/sh builtin behavior (`read`, `getopts`,
//! `test`/`[`, `printf`, `declare`, `unset`, `alias`, `source`/`.`,
//! `eval`, `exec`, `type`, `command`, `umask`, `kill`, and `set`'s
//! option-flag half). A failure here means a corpus case's script has a
//! bug -- or, for the several cases that deliberately assert on the
//! POSIX "special built-in" abort-asymmetry between bash and dash (see
//! `known-differences.md`), that the running machine's shells disagree
//! with what was verified while building this corpus -- not that conch
//! is non-compliant.
//!
//! This is also the first phase whose corpus exercises
//! [`conch_difftest::case::Case::stdin`] (`read`'s cases): a failure here
//! could additionally mean a bug in `invoke::configure_stdin` itself
//! rather than in a corpus case's script.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase5_oracles_agree_with_themselves() {
    let phase5 = corpus::corpus_root().join("phase5");
    let cases = corpus::load_dir(&phase5).expect("phase 5 corpus failed to load");

    let results = runner::run_oracle_selfcheck(&cases);
    runner::print_report("oracle self-check: phase5", &results);

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
