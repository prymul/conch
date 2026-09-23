//! Validates the entire shipped corpus loads and passes structural
//! validation. This runs today, with no shell subprocess involved, and
//! catches corpus-authoring mistakes (duplicate names, empty scripts,
//! malformed TOML, ...) independently of whether bash/sh/conch are even
//! available in the environment.

use conch_difftest::corpus;

#[test]
fn phase1_corpus_loads_and_validates() {
    let phase1 = corpus::corpus_root().join("phase1");
    let cases = corpus::load_dir(&phase1)
        .unwrap_or_else(|err| panic!("phase 1 corpus failed to load: {err}"));
    assert!(
        !cases.is_empty(),
        "expected at least one case in the phase 1 corpus"
    );
    println!("loaded {} phase 1 case(s)", cases.len());
}
