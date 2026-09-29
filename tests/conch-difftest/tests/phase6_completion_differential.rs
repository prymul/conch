//! The real differential suite for Phase 6 command-position tab
//! completion: conch's own `conch_shell_core::completion::command_candidates`,
//! called directly, in-process -- vs. real bash's `compgen -c`
//! (`completion_oracle.rs`).
//!
//! Same shape and rationale as `phase6_prompt_differential.rs` -- see that
//! file's own doc comment and README.md's "Phase 6: what's differential
//! and what isn't" for the general "why an in-process call, not a spawned
//! binary" background. Shares `CONCH_DIFFTEST_STRICT_PHASE6` with the
//! prompt-expansion differential (two halves of the same phase).
//!
//! **Deliberately excludes real conch/bash builtin name comparison** --
//! `CompletionState.builtins` is always left empty here. See
//! `completion_case.rs`'s module doc comment for the full "why": conch's
//! actual registered-builtin roster and bash's own are two independently
//! designed, genuinely different (if overlapping) sets, and asserting
//! they agree at an arbitrary prefix would be exactly the "don't force a
//! differential comparison that isn't testing the real pipeline" mistake
//! this project's guardrails warn against -- not a real product bug were
//! it to disagree.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use conch_difftest::completion_case::CompletionCase;
use conch_difftest::completion_oracle::{CompletionKind, compgen_via_bash};
use conch_difftest::corpus;
use conch_shell_core::{CompletionState, command_candidates};

fn make_executable(dir: &Path, name: &str) {
    fs::write(dir.join(name), b"").unwrap();
    #[cfg(unix)]
    fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o755)).unwrap();
}

/// Runs `case` both ways (candidate: `command_candidates` in-process;
/// oracle: real bash's `compgen -c`, fully-replaced `$PATH`) and returns
/// `Err` describing the mismatch if the two (sorted, deduplicated -- see
/// `completion_oracle.rs`'s own doc comment for why raw ordering isn't
/// part of the contract) candidate sets disagree.
fn differential_one(case: &CompletionCase) -> Result<(), String> {
    let candidate_bin_dir = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;
    let oracle_workdir = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;
    let oracle_bin_dir = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;

    for name in &case.path_executables {
        make_executable(candidate_bin_dir.path(), name);
        make_executable(oracle_bin_dir.path(), name);
    }

    let state = CompletionState {
        functions: case.functions.clone(),
        aliases: case.aliases.iter().map(|a| a.name.clone()).collect(),
        // Deliberately empty -- see this file's own doc comment.
        builtins: Vec::new(),
        path: candidate_bin_dir.path().display().to_string(),
    };
    let mut candidate = command_candidates(&state, &case.prefix);
    candidate.sort();
    candidate.dedup();

    let functions: Vec<&str> = case.functions.iter().map(String::as_str).collect();
    let aliases: Vec<(&str, &str)> = case
        .aliases
        .iter()
        .map(|a| (a.name.as_str(), a.value.as_str()))
        .collect();
    let oracle = compgen_via_bash(
        CompletionKind::Command,
        &case.prefix,
        &[oracle_bin_dir.path()],
        &functions,
        &aliases,
        oracle_workdir.path(),
    )
    .map_err(|err| format!("oracle completion: {err}"))?;

    if candidate != oracle {
        return Err(format!(
            "candidate (conch's command_candidates) and oracle (real bash's compgen -c) \
             disagreed:\n  candidate: {candidate:?}\n  oracle:    {oracle:?}"
        ));
    }
    Ok(())
}

#[test]
fn phase6_completion_differential_against_conch() {
    let phase6 = corpus::corpus_root().join("phase6").join("completion");
    let cases =
        corpus::load_completion_dir(&phase6).expect("phase 6 completion corpus failed to load");

    let mut failures = Vec::new();
    let mut passed = 0usize;
    for case in &cases {
        match differential_one(case) {
            Ok(()) => passed += 1,
            Err(err) => failures.push(format!("{}: {err}", case.name)),
        }
    }

    println!(
        "conch-difftest [phase6 completion differential]: {passed} passed, {} failed ({} total)",
        failures.len(),
        cases.len()
    );
    for failure in &failures {
        println!("  FAIL {failure}");
    }

    let strict = std::env::var("CONCH_DIFFTEST_STRICT_PHASE6")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !strict {
        eprintln!(
            "conch-difftest: Phase 6 completion differential runs in report-only mode regardless \
             of any earlier phase's strict gate (those gate only their own corpora). Set \
             CONCH_DIFFTEST_STRICT_PHASE6=1 once this corpus is expected to be fully green -- see \
             tests/conch-difftest/README.md."
        );
        return;
    }

    assert!(
        failures.is_empty(),
        "{} phase 6 completion case(s) did not pass under CONCH_DIFFTEST_STRICT_PHASE6=1 -- see \
         the report printed above",
        failures.len()
    );
}
