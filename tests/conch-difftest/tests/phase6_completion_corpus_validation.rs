//! Validates the Phase 6 command-position tab-completion corpus loads and
//! passes structural validation. Runs today, with no shell subprocess
//! involved -- see `tests/corpus_validation.rs` for the Phase 1 sibling
//! this mirrors, and `src/completion_case.rs` for why this is a separate
//! schema/loader from `Case`/`PromptCase`.

use conch_difftest::corpus;

#[test]
fn phase6_completion_corpus_loads_and_validates() {
    let phase6 = corpus::corpus_root().join("phase6").join("completion");
    let cases = corpus::load_completion_dir(&phase6)
        .unwrap_or_else(|err| panic!("phase 6 completion corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 6 completion corpus"
    );
    println!("loaded {} phase 6 completion case(s)", cases.len());
}
