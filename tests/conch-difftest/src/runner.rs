//! Orchestrates running the corpus and produces a trackable summary
//! (pass/fail/skip counts plus categorized detail), matching how brush and
//! oils-for-unix both report differential-test results as an ongoing
//! metric rather than a single binary "compatible or not" verdict.

use std::path::{Path, PathBuf};

use crate::case::{Case, Oracle};
use crate::compare::{self, Mismatch};
use crate::invoke::{self, ShellUnderTest};

pub struct CaseResult {
    pub case_name: String,
    pub source_file: PathBuf,
    /// A short label for what this result is about: an oracle's program
    /// name (`"bash"`, `"sh"`) for an ordinary differential/self-check
    /// result, or `"known-difference"` for a pinned-expectation result.
    pub role: String,
    pub outcome: CaseOutcome,
}

pub enum CaseOutcome {
    Passed,
    Failed(Vec<Mismatch>),
    /// Not run at all, with a human-readable reason (e.g. "conch binary
    /// not available yet").
    Skipped(String),
    /// The harness itself failed to run the shell (spawn error, I/O
    /// error, ...) -- distinct from a `Failed` comparison mismatch.
    Error(String),
}

fn fresh_workdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir()
}

/// Runs every applicable case's oracle *against itself* (bash vs. a
/// second, independent bash invocation; sh vs. a second sh invocation).
///
/// This can run today, without conch existing at all, and validates the
/// entire harness plumbing that the real differential run depends on:
/// process spawning per [`crate::case::Invocation`] mode, byte-safe output
/// capture, [`crate::normalize`] rules (including workdir substitution),
/// and [`crate::compare`]'s diffing logic. A failure here means the
/// harness itself is broken, not conch.
pub fn run_oracle_selfcheck(cases: &[Case]) -> Vec<CaseResult> {
    let mut results = Vec::new();
    for case in cases {
        for oracle in &case.oracles {
            results.push(run_one_oracle_pair(case, *oracle));
        }
    }
    results
}

fn run_one_oracle_pair(case: &Case, oracle: Oracle) -> CaseResult {
    let role = oracle.program().to_string();
    let (candidate_dir, oracle_dir) = match (fresh_workdir(), fresh_workdir()) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(err), _) | (_, Err(err)) => {
            return CaseResult {
                case_name: case.name.clone(),
                source_file: case.source_file.clone(),
                role,
                outcome: CaseOutcome::Error(format!("failed to create temp workdir: {err}")),
            };
        }
    };

    let shell = ShellUnderTest::Oracle(oracle.program());
    let a = invoke::run(&shell, case.invocation, &case.script, candidate_dir.path());
    let b = invoke::run(&shell, case.invocation, &case.script, oracle_dir.path());

    match (a, b) {
        (Ok(a), Ok(b)) => {
            let mismatches =
                compare::compare(case, &a, candidate_dir.path(), &b, oracle_dir.path());
            let outcome = if mismatches.is_empty() {
                CaseOutcome::Passed
            } else {
                CaseOutcome::Failed(mismatches)
            };
            CaseResult {
                case_name: case.name.clone(),
                source_file: case.source_file.clone(),
                role,
                outcome,
            }
        }
        (Err(err), _) | (_, Err(err)) => CaseResult {
            case_name: case.name.clone(),
            source_file: case.source_file.clone(),
            role,
            outcome: CaseOutcome::Error(format!("failed to run {oracle}: {err}")),
        },
    }
}

/// Runs the real differential suite: conch vs. each case's configured
/// oracle(s), or conch vs. a [`crate::case::KnownDifference`]'s pinned
/// expectations. `conch_bin` is `None` until the conch binary exists /
/// is discoverable (see [`invoke::find_conch_binary`]); every case
/// produces a `Skipped` result in that situation rather than being
/// omitted, so the report always accounts for the full corpus.
pub fn run_differential(cases: &[Case], conch_bin: Option<&Path>) -> Vec<CaseResult> {
    let mut results = Vec::new();
    for case in cases {
        if let Some(known_difference) = &case.known_difference {
            results.push(run_known_difference(case, known_difference, conch_bin));
            continue;
        }
        for oracle in &case.oracles {
            results.push(run_one_differential_pair(case, *oracle, conch_bin));
        }
    }
    results
}

fn run_one_differential_pair(case: &Case, oracle: Oracle, conch_bin: Option<&Path>) -> CaseResult {
    let role = oracle.program().to_string();
    let Some(conch_bin) = conch_bin else {
        return CaseResult {
            case_name: case.name.clone(),
            source_file: case.source_file.clone(),
            role,
            outcome: CaseOutcome::Skipped(
                "conch binary not available yet (see tests/conch-difftest/README.md)".to_string(),
            ),
        };
    };

    let (candidate_dir, oracle_dir) = match (fresh_workdir(), fresh_workdir()) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(err), _) | (_, Err(err)) => {
            return CaseResult {
                case_name: case.name.clone(),
                source_file: case.source_file.clone(),
                role,
                outcome: CaseOutcome::Error(format!("failed to create temp workdir: {err}")),
            };
        }
    };

    let conch = ShellUnderTest::Conch(conch_bin.to_path_buf());
    let oracle_shell = ShellUnderTest::Oracle(oracle.program());
    let candidate = invoke::run(&conch, case.invocation, &case.script, candidate_dir.path());
    let oracle_run = invoke::run(
        &oracle_shell,
        case.invocation,
        &case.script,
        oracle_dir.path(),
    );

    match (candidate, oracle_run) {
        (Ok(candidate), Ok(oracle_run)) => {
            let mismatches = compare::compare(
                case,
                &candidate,
                candidate_dir.path(),
                &oracle_run,
                oracle_dir.path(),
            );
            let outcome = if mismatches.is_empty() {
                CaseOutcome::Passed
            } else {
                CaseOutcome::Failed(mismatches)
            };
            CaseResult {
                case_name: case.name.clone(),
                source_file: case.source_file.clone(),
                role,
                outcome,
            }
        }
        (candidate, oracle_run) => {
            let err = candidate
                .err()
                .or(oracle_run.err())
                .expect("one side errored");
            CaseResult {
                case_name: case.name.clone(),
                source_file: case.source_file.clone(),
                role,
                outcome: CaseOutcome::Error(format!("failed to run: {err}")),
            }
        }
    }
}

fn run_known_difference(
    case: &Case,
    known_difference: &crate::case::KnownDifference,
    conch_bin: Option<&Path>,
) -> CaseResult {
    let role = "known-difference".to_string();
    let Some(conch_bin) = conch_bin else {
        return CaseResult {
            case_name: case.name.clone(),
            source_file: case.source_file.clone(),
            role,
            outcome: CaseOutcome::Skipped(
                "conch binary not available yet (see tests/conch-difftest/README.md)".to_string(),
            ),
        };
    };

    let workdir = match fresh_workdir() {
        Ok(dir) => dir,
        Err(err) => {
            return CaseResult {
                case_name: case.name.clone(),
                source_file: case.source_file.clone(),
                role,
                outcome: CaseOutcome::Error(format!("failed to create temp workdir: {err}")),
            };
        }
    };

    let conch = ShellUnderTest::Conch(conch_bin.to_path_buf());
    let outcome = invoke::run(&conch, case.invocation, &case.script, workdir.path());

    match outcome {
        Ok(actual) => {
            let mismatches =
                compare::compare_known_difference(case, known_difference, &actual, workdir.path());
            let outcome = if mismatches.is_empty() {
                CaseOutcome::Passed
            } else {
                CaseOutcome::Failed(mismatches)
            };
            CaseResult {
                case_name: case.name.clone(),
                source_file: case.source_file.clone(),
                role,
                outcome,
            }
        }
        Err(err) => CaseResult {
            case_name: case.name.clone(),
            source_file: case.source_file.clone(),
            role,
            outcome: CaseOutcome::Error(format!("failed to run conch: {err}")),
        },
    }
}

/// A trackable summary: counts, not a single pass/fail verdict.
#[derive(Debug, Default, Clone, Copy)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errored: usize,
}

impl Summary {
    pub fn from_results(results: &[CaseResult]) -> Self {
        let mut summary = Summary::default();
        for result in results {
            match &result.outcome {
                CaseOutcome::Passed => summary.passed += 1,
                CaseOutcome::Failed(_) => summary.failed += 1,
                CaseOutcome::Skipped(_) => summary.skipped += 1,
                CaseOutcome::Error(_) => summary.errored += 1,
            }
        }
        summary
    }

    pub fn total(&self) -> usize {
        self.passed + self.failed + self.skipped + self.errored
    }
}

/// Prints a categorized report: a one-line summary followed by detail for
/// every non-passing result, grouped by case.
pub fn print_report(label: &str, results: &[CaseResult]) {
    let summary = Summary::from_results(results);
    println!(
        "conch-difftest [{label}]: {} passed, {} failed, {} skipped, {} errored ({} total)",
        summary.passed,
        summary.failed,
        summary.skipped,
        summary.errored,
        summary.total()
    );
    for result in results {
        match &result.outcome {
            CaseOutcome::Passed => {}
            CaseOutcome::Failed(mismatches) => {
                println!(
                    "  FAIL {} [{}] ({})",
                    result.case_name,
                    result.role,
                    result.source_file.display()
                );
                for mismatch in mismatches {
                    for line in mismatch.to_string().lines() {
                        println!("    {line}");
                    }
                }
            }
            CaseOutcome::Skipped(reason) => {
                println!("  SKIP {} [{}]: {reason}", result.case_name, result.role);
            }
            CaseOutcome::Error(message) => {
                println!("  ERROR {} [{}]: {message}", result.case_name, result.role);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{CompareTarget, Invocation};

    fn passing_case(name: &str) -> Case {
        Case {
            name: name.to_string(),
            description: "d".to_string(),
            tags: Vec::new(),
            oracles: vec![Oracle::Bash],
            invocation: Invocation::DashC,
            compare: vec![CompareTarget::Stdout, CompareTarget::ExitCode],
            normalize: Vec::new(),
            script: "echo hi".to_string(),
            known_difference: None,
            note: None,
            source_file: PathBuf::from("test.toml"),
        }
    }

    #[test]
    fn oracle_selfcheck_passes_for_a_deterministic_case() {
        let cases = vec![passing_case("selfcheck-case")];
        let results = run_oracle_selfcheck(&cases);
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].outcome, CaseOutcome::Passed));
    }

    #[test]
    fn oracle_selfcheck_catches_workdir_dependent_output_without_normalization() {
        // `pwd` output legitimately differs between the two independently
        // created temp workdirs unless the case opts into workdir
        // normalization -- this proves the harness surfaces that as a
        // failure rather than silently ignoring it.
        let mut case = passing_case("pwd-without-normalization");
        case.script = "pwd".to_string();
        let results = run_oracle_selfcheck(&[case]);
        assert!(matches!(results[0].outcome, CaseOutcome::Failed(_)));
    }

    #[test]
    fn oracle_selfcheck_passes_for_workdir_dependent_output_with_normalization() {
        let mut case = passing_case("pwd-with-normalization");
        case.script = "pwd".to_string();
        case.normalize = vec![crate::case::NormalizeRule::Workdir];
        let results = run_oracle_selfcheck(&[case]);
        assert!(matches!(results[0].outcome, CaseOutcome::Passed));
    }

    #[test]
    fn differential_run_skips_every_case_without_a_conch_binary() {
        let cases = vec![passing_case("a"), passing_case("b")];
        let results = run_differential(&cases, None);
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| matches!(r.outcome, CaseOutcome::Skipped(_)))
        );
    }

    #[test]
    fn known_difference_case_is_skipped_without_a_conch_binary() {
        let mut case = passing_case("kd-case");
        case.oracles = Vec::new();
        case.known_difference = Some(crate::case::KnownDifference {
            id: "KD-0000".to_string(),
            expect_stdout: Some("hi\n".to_string()),
            expect_stderr: None,
            expect_exit_code: None,
        });
        let results = run_differential(&[case], None);
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].outcome, CaseOutcome::Skipped(_)));
        assert_eq!(results[0].role, "known-difference");
    }

    #[test]
    fn summary_counts_each_outcome_kind() {
        let results = vec![
            CaseResult {
                case_name: "a".to_string(),
                source_file: PathBuf::new(),
                role: "bash".to_string(),
                outcome: CaseOutcome::Passed,
            },
            CaseResult {
                case_name: "b".to_string(),
                source_file: PathBuf::new(),
                role: "bash".to_string(),
                outcome: CaseOutcome::Failed(Vec::new()),
            },
            CaseResult {
                case_name: "c".to_string(),
                source_file: PathBuf::new(),
                role: "bash".to_string(),
                outcome: CaseOutcome::Skipped("n/a".to_string()),
            },
            CaseResult {
                case_name: "d".to_string(),
                source_file: PathBuf::new(),
                role: "bash".to_string(),
                outcome: CaseOutcome::Error("boom".to_string()),
            },
        ];
        let summary = Summary::from_results(&results);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.errored, 1);
        assert_eq!(summary.total(), 4);
    }
}
