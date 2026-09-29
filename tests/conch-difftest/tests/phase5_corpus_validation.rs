//! Validates the entire shipped Phase 5 corpus loads and passes
//! structural validation. This runs today, with no shell subprocess
//! involved -- see `corpus_validation.rs` (Phase 1) for the pattern this
//! mirrors.

use conch_difftest::corpus;

#[test]
fn phase5_corpus_loads_and_validates() {
    let phase5 = corpus::corpus_root().join("phase5");
    let cases = corpus::load_dir(&phase5)
        .unwrap_or_else(|err| panic!("phase 5 corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 5 corpus"
    );
    println!("loaded {} phase 5 case(s)", cases.len());
}
