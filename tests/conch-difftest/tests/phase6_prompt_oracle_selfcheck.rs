//! Runs every Phase 6 prompt-case's bash-oracle expansion *against a
//! second, independent invocation of itself* and asserts they agree --
//! the [`prompt_case`]/[`prompt_oracle`] sibling of `tests/oracle_selfcheck.rs`.
//!
//! This validates the entire Phase 6 prompt-oracle mechanism -- script
//! synthesis (env exports, the `(exit N)`-then-`printf` ordering
//! `prompt_oracle`'s doc comment explains), process spawning, byte-safe
//! capture, and [`conch_difftest::normalize`]'s workdir substitution --
//! using real bash as a stand-in for conch on *both* sides, exactly the
//! same "prove the harness itself is trustworthy before conch's own
//! implementation exists to compare against" role every earlier phase's
//! `*_oracle_selfcheck.rs` already plays. It requires `bash` on `PATH`
//! (present by default in CI and on any normal dev machine) but not the
//! conch binary.
//!
//! A failure here means the Phase 6 prompt-oracle mechanism itself has a
//! bug (or a corpus case made an incorrect assumption about bash's own
//! prompt-expansion behavior) -- not that conch is non-compliant.
//! `tests/phase6_prompt_differential.rs` (conch's own `expand_prompt` vs.
//! this same oracle) doesn't exist yet -- see the crate README's "Phase 6
//! planning notes" for exactly what's blocking it and how to wire it up
//! once unblocked.

use std::path::{Path, PathBuf};

use conch_difftest::corpus;
use conch_difftest::invoke::RunOutcome;
use conch_difftest::normalize::{self, NormalizeContext};
use conch_difftest::prompt_case::{PromptCase, PromptVar};
use conch_difftest::prompt_oracle::{expand_ps1_via_bash, expand_ps2_via_bash};

/// Resolves the actual directory a case should be expanded from: the
/// fresh temp root itself, or a freshly created subdirectory of it when
/// `cwd_subdir` is set (see [`PromptCase::cwd_subdir`]'s doc comment).
fn resolve_workdir(root: &Path, case: &PromptCase) -> std::io::Result<PathBuf> {
    match &case.cwd_subdir {
        Some(sub) => {
            let dir = root.join(sub);
            std::fs::create_dir(&dir)?;
            Ok(dir)
        }
        None => Ok(root.to_path_buf()),
    }
}

fn expand(case: &PromptCase, workdir: &Path) -> std::io::Result<RunOutcome> {
    let env: Vec<(&str, &str)> = case
        .env
        .iter()
        .map(|e| (e.name.as_str(), e.value.as_str()))
        .collect();
    match case.prompt {
        PromptVar::Ps1 => expand_ps1_via_bash(&case.template, &env, case.last_status, workdir),
        PromptVar::Ps2 => expand_ps2_via_bash(&case.template, &env, case.last_status, workdir),
    }
}

/// Runs `case`'s bash-oracle expansion twice, in two independently
/// created temp directories, and returns `Err` describing the mismatch
/// (or harness-level I/O error) if the two runs disagree once both sides
/// are normalized per `case.normalize` against their *own* workdir --
/// exactly the two-workdirs-never-shared discipline every other
/// oracle-vs-oracle/candidate-vs-oracle comparison in this crate follows,
/// applied here to prove the mechanism is deterministic on its own before
/// any real candidate exists to compare against.
fn selfcheck_one(case: &PromptCase) -> Result<(), String> {
    let dir_a = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;
    let dir_b = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;
    let workdir_a =
        resolve_workdir(dir_a.path(), case).map_err(|err| format!("resolve workdir a: {err}"))?;
    let workdir_b =
        resolve_workdir(dir_b.path(), case).map_err(|err| format!("resolve workdir b: {err}"))?;

    let a = expand(case, &workdir_a).map_err(|err| format!("expand (run a): {err}"))?;
    let b = expand(case, &workdir_b).map_err(|err| format!("expand (run b): {err}"))?;

    let ctx_a = NormalizeContext {
        workdir: &workdir_a,
    };
    let ctx_b = NormalizeContext {
        workdir: &workdir_b,
    };
    let normalized_a = normalize::apply(&case.normalize, &a.stdout, &ctx_a);
    let normalized_b = normalize::apply(&case.normalize, &b.stdout, &ctx_b);

    if normalized_a != normalized_b {
        return Err(format!(
            "two independent bash-oracle expansions of the same template disagreed:\n  \
             run a: {:?}\n  run b: {:?}",
            String::from_utf8_lossy(&normalized_a),
            String::from_utf8_lossy(&normalized_b),
        ));
    }
    if a.exit_code != Some(0) || b.exit_code != Some(0) {
        return Err(format!(
            "expected both expansion runs to exit 0, got {:?} and {:?}",
            a.exit_code, b.exit_code
        ));
    }
    Ok(())
}

#[test]
fn phase6_prompt_oracle_agrees_with_itself() {
    let phase6 = corpus::corpus_root().join("phase6");
    let cases = corpus::load_prompt_dir(&phase6).expect("phase 6 prompt corpus failed to load");

    let mut failures = Vec::new();
    let mut checked = 0usize;
    let mut skipped = 0usize;
    for case in &cases {
        if case.known_difference.is_some() {
            // Nothing to self-check: a known-difference case pins conch's
            // own expected output directly and never runs a live oracle
            // at all (see `prompt_case.rs`'s doc comment) -- same
            // exclusion `run_oracle_selfcheck` gets "for free" for Phase
            // 1-5's `Case::known_difference` via an empty `oracles` list.
            skipped += 1;
            continue;
        }
        checked += 1;
        if let Err(err) = selfcheck_one(case) {
            failures.push(format!("{}: {err}", case.name));
        }
    }

    println!(
        "conch-difftest [phase6 prompt oracle self-check]: {checked} checked, {skipped} skipped \
         (known-difference), {} failed",
        failures.len()
    );
    for failure in &failures {
        println!("  FAIL {failure}");
    }

    assert!(
        failures.is_empty(),
        "{} phase 6 prompt case(s) failed the oracle self-check -- see the report printed above \
         (run with `cargo test -- --nocapture` to see it locally)",
        failures.len()
    );
}
