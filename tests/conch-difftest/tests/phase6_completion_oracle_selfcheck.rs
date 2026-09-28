//! Runs every Phase 6 completion-case's bash-oracle (`compgen -c`)
//! candidate list *against a second, independent invocation of itself*
//! and asserts they agree -- the [`completion_case`]/[`completion_oracle`]
//! sibling of `tests/oracle_selfcheck.rs`/`phase6_prompt_oracle_selfcheck.rs`.
//!
//! Validates the completion-oracle mechanism itself (a fully replaced,
//! controlled `$PATH`; function/alias definition; `compgen -c` querying)
//! using real bash on both sides -- meaningful even before/independently
//! of whether conch's own `command_candidates` agrees with it. Requires
//! `bash` on `PATH` but not the conch binary.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use conch_difftest::completion_case::CompletionCase;
use conch_difftest::completion_oracle::{CompletionKind, compgen_via_bash};
use conch_difftest::corpus;

/// See `completion_oracle.rs`'s own test helper of the same shape.
fn make_executable(dir: &Path, name: &str) {
    let path = dir.join(name);
    fs::write(&path, b"").unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn run_case(case: &CompletionCase, workdir: &Path, bin_dir: &Path) -> Vec<String> {
    for name in &case.path_executables {
        make_executable(bin_dir, name);
    }
    let functions: Vec<&str> = case.functions.iter().map(String::as_str).collect();
    let aliases: Vec<(&str, &str)> = case
        .aliases
        .iter()
        .map(|a| (a.name.as_str(), a.value.as_str()))
        .collect();
    compgen_via_bash(
        CompletionKind::Command,
        &case.prefix,
        &[bin_dir],
        &functions,
        &aliases,
        workdir,
    )
    .expect("compgen_via_bash failed")
}

#[test]
fn phase6_completion_oracle_agrees_with_itself() {
    let phase6 = corpus::corpus_root().join("phase6").join("completion");
    let cases =
        corpus::load_completion_dir(&phase6).expect("phase 6 completion corpus failed to load");

    let mut failures = Vec::new();
    for case in &cases {
        let workdir_a = tempfile::tempdir().unwrap();
        let bin_dir_a = tempfile::tempdir().unwrap();
        let workdir_b = tempfile::tempdir().unwrap();
        let bin_dir_b = tempfile::tempdir().unwrap();

        let a = run_case(case, workdir_a.path(), bin_dir_a.path());
        let b = run_case(case, workdir_b.path(), bin_dir_b.path());

        if a != b {
            failures.push(format!(
                "{}: two independent bash-oracle completion runs disagreed:\n  run a: {a:?}\n  \
                 run b: {b:?}",
                case.name
            ));
        }
    }

    println!(
        "conch-difftest [phase6 completion oracle self-check]: {} checked, {} failed",
        cases.len(),
        failures.len()
    );
    for failure in &failures {
        println!("  FAIL {failure}");
    }

    assert!(
        failures.is_empty(),
        "{} phase 6 completion case(s) failed the oracle self-check -- see the report printed \
         above (run with `cargo test -- --nocapture` to see it locally)",
        failures.len()
    );
}
