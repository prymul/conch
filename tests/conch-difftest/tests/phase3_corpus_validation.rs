//! Validates the entire shipped Phase 3 corpus loads and passes
//! structural validation. This runs today, with no shell subprocess
//! involved -- see `corpus_validation.rs` (Phase 1) / `phase2_corpus_
//! validation.rs` for the pattern this mirrors.

use conch_difftest::corpus;

#[test]
fn phase3_corpus_loads_and_validates() {
    let phase3 = corpus::corpus_root().join("phase3");
    let cases = corpus::load_dir(&phase3)
        .unwrap_or_else(|err| panic!("phase 3 corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 3 corpus"
    );
    println!("loaded {} phase 3 case(s)", cases.len());
}
