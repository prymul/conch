//! Validates the entire shipped Phase 2 corpus loads and passes
//! structural validation. This runs today, with no shell subprocess
//! involved -- see `corpus_validation.rs` for the Phase 1 equivalent this
//! mirrors.

use conch_difftest::corpus;

#[test]
fn phase2_corpus_loads_and_validates() {
    let phase2 = corpus::corpus_root().join("phase2");
    let cases = corpus::load_dir(&phase2)
        .unwrap_or_else(|err| panic!("phase 2 corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 2 corpus"
    );
    println!("loaded {} phase 2 case(s)", cases.len());
}
