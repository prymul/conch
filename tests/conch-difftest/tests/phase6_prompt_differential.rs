//! The real differential suite for Phase 6 `PS1`/`PS2` expansion: conch's
//! own `conch_shell_core::expand_prompt`, called directly, in-process --
//! vs. real bash's `${VAR@P}` transform (`prompt_oracle.rs`).
//!
//! Unlike every Phase 1-5 `*_differential.rs`, the *candidate* side here
//! is never a spawned `conch` binary -- see README.md's "Phase 6: what's
//! differential and what isn't" for the full "why" (prompt rendering only
//! happens inside conch's interactive readline loop, which needs a real
//! pty this harness never allocates; `expand_prompt` is the pure core
//! function pulled out specifically so it's reachable without one). This
//! is *why* this crate has a `[dev-dependencies]` entry on
//! `conch-shell-core` at all -- see this crate's `Cargo.toml` for that
//! entry's own doc comment, and README.md for the crate-location risk
//! that was flagged and resolved before this file could be written at
//! all (`expand_prompt` had to actually live in a `[lib]`-target crate).
//!
//! Same `CONCH_DIFFTEST_STRICT_PHASE6` convention as every other phase's
//! own strict-mode flag (see README.md's "Phase-specific strict gates"):
//! report-only by default, a hard gate only once explicitly opted in.
//! Shared with `phase6_completion_differential.rs` -- both are two halves
//! of the same phase, not two different phases (see this crate's Phase 6
//! planning section for that call).

use std::path::{Path, PathBuf};

use conch_difftest::corpus;
use conch_difftest::normalize::{self, NormalizeContext};
use conch_difftest::prompt_case::{PromptCase, PromptVar};
use conch_difftest::prompt_oracle::{expand_ps1_via_bash, expand_ps2_via_bash};
use conch_shell_core::{Shell, expand_prompt};

/// Builds a `Shell` reflecting `case`'s state: the real inherited
/// environment (`Shell::new()`'s own default -- matching the oracle side,
/// which spawns bash with the same inherited environment, see
/// `prompt_oracle.rs`), `case.env` layered on top, `cwd` set to
/// `workdir`'s *canonicalized* form (matching what a real shell's own
/// `$PWD`/`pwd` would report -- see [`conch_difftest::normalize`]'s own
/// "workdir" rule doc comment for why canonicalization matters here, e.g.
/// macOS's `/tmp` -> `/private/tmp`), and `last_status` forced to
/// `case.last_status`.
fn candidate_shell(case: &PromptCase, workdir: &Path) -> Shell {
    let mut shell = Shell::new();
    for env in &case.env {
        shell.env_vars.insert(env.name.clone(), env.value.clone());
    }
    shell.cwd = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    shell.last_status = case.last_status;
    shell
}

/// Resolves the actual directory a case should be expanded from -- the
/// exact sibling of `phase6_prompt_oracle_selfcheck.rs`'s own
/// `resolve_workdir`, duplicated rather than shared across two
/// independent test binaries (each `tests/*.rs` file compiles as its own
/// crate; sharing would need a `tests/common/mod.rs`-style module for two
/// call sites, not worth it yet).
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

fn oracle_env(case: &PromptCase) -> Vec<(&str, &str)> {
    case.env
        .iter()
        .map(|e| (e.name.as_str(), e.value.as_str()))
        .collect()
}

/// Runs one non-`known_difference` case both ways and returns `Err`
/// describing the mismatch (or a harness-level I/O error) if they
/// disagree once both sides are normalized per `case.normalize` against
/// their own respective workdir.
fn differential_one(case: &PromptCase) -> Result<(), String> {
    let candidate_dir = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;
    let oracle_dir = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;
    let candidate_workdir = resolve_workdir(candidate_dir.path(), case)
        .map_err(|err| format!("resolve candidate workdir: {err}"))?;
    let oracle_workdir = resolve_workdir(oracle_dir.path(), case)
        .map_err(|err| format!("resolve oracle workdir: {err}"))?;

    let mut shell = candidate_shell(case, &candidate_workdir);
    // `history_len` (the third `expand_prompt` argument, for `\!`/`\#`) is
    // fixed at `0` uniformly -- this corpus deliberately never exercises
    // either escape (see `prompt_oracle.rs`'s "Non-determinism this
    // module's callers must avoid": a fresh oracle invocation and conch's
    // own `Shell` have no shared, meaningful history-length convention to
    // agree on).
    let candidate_output = expand_prompt(&mut shell, &case.template, 0);

    let env = oracle_env(case);
    let oracle_outcome = match case.prompt {
        PromptVar::Ps1 => {
            expand_ps1_via_bash(&case.template, &env, case.last_status, &oracle_workdir)
        }
        PromptVar::Ps2 => {
            expand_ps2_via_bash(&case.template, &env, case.last_status, &oracle_workdir)
        }
    }
    .map_err(|err| format!("oracle expansion: {err}"))?;

    let candidate_ctx = NormalizeContext {
        workdir: &candidate_workdir,
    };
    let oracle_ctx = NormalizeContext {
        workdir: &oracle_workdir,
    };
    let candidate_normalized =
        normalize::apply(&case.normalize, candidate_output.as_bytes(), &candidate_ctx);
    let oracle_normalized = normalize::apply(&case.normalize, &oracle_outcome.stdout, &oracle_ctx);

    if candidate_normalized != oracle_normalized {
        return Err(format!(
            "candidate (conch's expand_prompt) and oracle (real bash) disagreed:\n  \
             candidate: {:?}\n  oracle:    {:?}",
            String::from_utf8_lossy(&candidate_normalized),
            String::from_utf8_lossy(&oracle_normalized),
        ));
    }
    Ok(())
}

/// Runs one `known_difference` case: compares `expand_prompt`'s own
/// output directly against the pinned `expect` string, with no live
/// oracle run at all -- see `prompt_case.rs`'s `PromptKnownDifference`
/// doc comment.
fn known_difference_one(case: &PromptCase, expect: &str) -> Result<(), String> {
    let workdir = tempfile::tempdir().map_err(|err| format!("tempdir: {err}"))?;
    let candidate_workdir = resolve_workdir(workdir.path(), case)
        .map_err(|err| format!("resolve candidate workdir: {err}"))?;
    let mut shell = candidate_shell(case, &candidate_workdir);
    let candidate_output = expand_prompt(&mut shell, &case.template, 0);

    let ctx = NormalizeContext {
        workdir: &candidate_workdir,
    };
    let candidate_normalized = normalize::apply(&case.normalize, candidate_output.as_bytes(), &ctx);

    if candidate_normalized != expect.as_bytes() {
        return Err(format!(
            "known-difference case's pinned expectation didn't match: expected {:?}, got {:?}",
            expect,
            String::from_utf8_lossy(&candidate_normalized),
        ));
    }
    Ok(())
}

#[test]
fn phase6_prompt_differential_against_conch() {
    let phase6 = corpus::corpus_root().join("phase6").join("prompt");
    let cases = corpus::load_prompt_dir(&phase6).expect("phase 6 prompt corpus failed to load");

    let mut failures = Vec::new();
    let mut passed = 0usize;
    for case in &cases {
        let result = match &case.known_difference {
            Some(kd) => known_difference_one(case, &kd.expect),
            None => differential_one(case),
        };
        match result {
            Ok(()) => passed += 1,
            Err(err) => failures.push(format!("{}: {err}", case.name)),
        }
    }

    println!(
        "conch-difftest [phase6 prompt differential]: {passed} passed, {} failed ({} total)",
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
            "conch-difftest: Phase 6 prompt-expansion differential runs in report-only mode \
             regardless of any earlier phase's strict gate (those gate only their own corpora). \
             Set CONCH_DIFFTEST_STRICT_PHASE6=1 once this corpus is expected to be fully green -- \
             see tests/conch-difftest/README.md."
        );
        return;
    }

    assert!(
        failures.is_empty(),
        "{} phase 6 prompt case(s) did not pass under CONCH_DIFFTEST_STRICT_PHASE6=1 -- see the \
         report printed above",
        failures.len()
    );
}
