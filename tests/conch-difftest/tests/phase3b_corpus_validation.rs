//! Validates the entire shipped Phase 3b corpus loads and passes
//! structural validation. This runs today, with no shell subprocess
//! involved -- see `corpus_validation.rs` (Phase 1) / `phase3_corpus_
//! validation.rs` for the pattern this mirrors.

use conch_difftest::corpus;

#[test]
fn phase3b_corpus_loads_and_validates() {
    let phase3b = corpus::corpus_root().join("phase3b");
    let cases = corpus::load_dir(&phase3b)
        .unwrap_or_else(|err| panic!("phase 3b corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 3b corpus"
    );
    println!("loaded {} phase 3b case(s)", cases.len());
}
