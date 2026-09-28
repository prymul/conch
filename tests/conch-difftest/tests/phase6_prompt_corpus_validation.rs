//! Validates the Phase 6 `PS1`/`PS2` prompt-expansion corpus loads and
//! passes structural validation. Runs today, with no shell subprocess
//! involved -- see `tests/corpus_validation.rs` for the Phase 1 sibling
//! this mirrors, and `src/prompt_case.rs` for why this is a separate
//! schema/loader rather than reusing `Case`/`load_dir`.

use conch_difftest::corpus;

#[test]
fn phase6_prompt_corpus_loads_and_validates() {
    let phase6 = corpus::corpus_root().join("phase6");
    let cases = corpus::load_prompt_dir(&phase6)
        .unwrap_or_else(|err| panic!("phase 6 prompt corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 6 prompt corpus"
    );
    println!("loaded {} phase 6 prompt case(s)", cases.len());
}
