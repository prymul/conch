//! Validates the entire shipped Phase 4 corpus loads and passes
//! structural validation. This runs today, with no shell subprocess
//! involved -- see `corpus_validation.rs` (Phase 1) for the pattern this
//! mirrors.

use conch_difftest::corpus;

#[test]
fn phase4_corpus_loads_and_validates() {
    let phase4 = corpus::corpus_root().join("phase4");
    let cases = corpus::load_dir(&phase4)
        .unwrap_or_else(|err| panic!("phase 4 corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 4 corpus"
    );
    println!("loaded {} phase 4 case(s)", cases.len());
}
