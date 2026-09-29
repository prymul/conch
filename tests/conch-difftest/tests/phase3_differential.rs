//! The real differential suite: conch vs. bash/sh, per the Phase 3
//! (control flow) corpus.
//!
//! **Phase 3 execution semantics don't exist in conch yet** -- as of this
//! writing, `crates/conch-parser`'s control-flow grammar (if/for/while/
//! until/case/subshells/brace groups, plus `break`/`continue` with a
//! numeric level) is still in progress in parallel with this corpus. Every
//! case below is therefore expected to `Skip` (no conch binary) or
//! `Fail`/`Error` (a conch binary exists but doesn't implement this yet)
//! for a while, exactly the same report-only, non-blocking state
//! `differential.rs` and `phase2_differential.rs` were in before their
//! respective execution semantics landed.
//!
//! This is deliberately **decoupled from `CONCH_DIFFTEST_STRICT`** and
//! `CONCH_DIFFTEST_STRICT_PHASE2` (Phase 1's and Phase 2's hard gates):
//! this test checks its own, differently-named `CONCH_DIFFTEST_STRICT_PHASE3`
//! instead, which nothing currently sets. That's the whole mechanism --
//! CI's `difftest` job runs every test in this crate (`cargo test -p
//! conch-difftest`), so this suite runs and reports every time that job
//! does, but flipping an earlier phase's corpus to a hard gate never
//! silently pulls this still-in-progress phase's corpus along with it (and
//! Phase 3 going green later won't require touching Phase 1's or Phase
//! 2's gates either). Once Phase 3 execution lands and this corpus is
//! expected to be fully green, set `CONCH_DIFFTEST_STRICT_PHASE3=1`
//! (locally, or in the `difftest` job's `env:`) to make it one, the same
//! way Phase 1's README section "Wiring in real execution" describes for
//! `CONCH_DIFFTEST_STRICT`.

use conch_difftest::corpus;
use conch_difftest::runner::{self, CaseOutcome};

#[test]
fn phase3_differential_against_conch() {
    let phase3 = corpus::corpus_root().join("phase3");
    let cases = corpus::load_dir(&phase3).expect("phase 3 corpus failed to load");

    let conch_bin = conch_difftest::invoke::find_conch_binary();
    let results = runner::run_differential(&cases, conch_bin.as_deref());
    runner::print_report("differential: phase3", &results);

    let strict = std::env::var("CONCH_DIFFTEST_STRICT_PHASE3")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !strict {
        eprintln!(
            "conch-difftest: Phase 3 control-flow execution doesn't exist in conch yet, so this \
             suite runs in report-only mode regardless of CONCH_DIFFTEST_STRICT or \
             CONCH_DIFFTEST_STRICT_PHASE2 (those gate only the phase1/ and phase2/ corpora). Set \
             CONCH_DIFFTEST_STRICT_PHASE3=1 once Phase 3 evaluation lands and this corpus is \
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
        "{} case(s) did not pass under CONCH_DIFFTEST_STRICT_PHASE3=1 -- see the report printed \
         above",
        failures.len()
    );
}
