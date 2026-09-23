//! Comparing a candidate's run outcome against an oracle's, per a case's
//! configured `compare` targets and `normalize` rules.

use std::path::Path;

use crate::case::{Case, CompareTarget, KnownDifference};
use crate::invoke::RunOutcome;
use crate::normalize::{self, NormalizeContext};
use crate::report::render_bytes;

/// A single mismatched comparison target, rendered for display.
#[derive(Debug)]
pub struct Mismatch {
    pub target: CompareTarget,
    pub candidate: String,
    pub oracle: String,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} mismatch:\n    candidate: {:?}\n    oracle:    {:?}",
            self.target, self.candidate, self.oracle
        )
    }
}

/// Compares `candidate` against `oracle` for every target in
/// `case.compare`, normalizing each side with `case.normalize` and its own
/// working directory first. Returns one [`Mismatch`] per target that
/// differs (empty means everything the case asked to compare matched).
pub fn compare(
    case: &Case,
    candidate: &RunOutcome,
    candidate_workdir: &Path,
    oracle: &RunOutcome,
    oracle_workdir: &Path,
) -> Vec<Mismatch> {
    let candidate_ctx = NormalizeContext {
        workdir: candidate_workdir,
    };
    let oracle_ctx = NormalizeContext {
        workdir: oracle_workdir,
    };

    let mut mismatches = Vec::new();
    for target in &case.compare {
        match target {
            CompareTarget::Stdout => {
                let c = normalize::apply(&case.normalize, &candidate.stdout, &candidate_ctx);
                let o = normalize::apply(&case.normalize, &oracle.stdout, &oracle_ctx);
                if c != o {
                    mismatches.push(Mismatch {
                        target: *target,
                        candidate: render_bytes(&c),
                        oracle: render_bytes(&o),
                    });
                }
            }
            CompareTarget::Stderr => {
                let c = normalize::apply(&case.normalize, &candidate.stderr, &candidate_ctx);
                let o = normalize::apply(&case.normalize, &oracle.stderr, &oracle_ctx);
                if c != o {
                    mismatches.push(Mismatch {
                        target: *target,
                        candidate: render_bytes(&c),
                        oracle: render_bytes(&o),
                    });
                }
            }
            CompareTarget::ExitCode => {
                if candidate.exit_code != oracle.exit_code {
                    mismatches.push(Mismatch {
                        target: *target,
                        candidate: format!("{:?}", candidate.exit_code),
                        oracle: format!("{:?}", oracle.exit_code),
                    });
                }
            }
        }
    }
    mismatches
}

/// Compares an actual run against a [`KnownDifference`]'s pinned
/// expectations, for a case that deliberately diverges from any live
/// oracle. Only the `expect_*` fields that are actually set are checked
/// (each populates its own [`Mismatch`] with `oracle` holding the pinned
/// expected value rather than a second live run's output).
pub fn compare_known_difference(
    case: &Case,
    known_difference: &KnownDifference,
    actual: &RunOutcome,
    workdir: &Path,
) -> Vec<Mismatch> {
    let ctx = NormalizeContext { workdir };
    let mut mismatches = Vec::new();

    if let Some(expected) = &known_difference.expect_stdout {
        let actual_norm = normalize::apply(&case.normalize, &actual.stdout, &ctx);
        if actual_norm != expected.as_bytes() {
            mismatches.push(Mismatch {
                target: CompareTarget::Stdout,
                candidate: render_bytes(&actual_norm),
                oracle: expected.clone(),
            });
        }
    }
    if let Some(expected) = &known_difference.expect_stderr {
        let actual_norm = normalize::apply(&case.normalize, &actual.stderr, &ctx);
        if actual_norm != expected.as_bytes() {
            mismatches.push(Mismatch {
                target: CompareTarget::Stderr,
                candidate: render_bytes(&actual_norm),
                oracle: expected.clone(),
            });
        }
    }
    if let Some(expected) = known_difference.expect_exit_code
        && actual.exit_code != Some(expected)
    {
        mismatches.push(Mismatch {
            target: CompareTarget::ExitCode,
            candidate: format!("{:?}", actual.exit_code),
            oracle: format!("{:?}", Some(expected)),
        });
    }

    mismatches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{Invocation, Oracle};
    use std::path::PathBuf;

    fn case_with(compare: Vec<CompareTarget>, normalize: Vec<crate::case::NormalizeRule>) -> Case {
        Case {
            name: "t".to_string(),
            description: "t".to_string(),
            tags: Vec::new(),
            oracles: vec![Oracle::Bash],
            invocation: Invocation::DashC,
            compare,
            normalize,
            script: "echo hi".to_string(),
            known_difference: None,
            note: None,
            source_file: PathBuf::new(),
        }
    }

    #[test]
    fn identical_outcomes_produce_no_mismatches() {
        let case = case_with(
            vec![CompareTarget::Stdout, CompareTarget::ExitCode],
            Vec::new(),
        );
        let outcome = RunOutcome {
            stdout: b"hi\n".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let mismatches = compare(&case, &outcome, Path::new("/a"), &outcome, Path::new("/b"));
        assert!(mismatches.is_empty());
    }

    #[test]
    fn differing_stdout_is_reported() {
        let case = case_with(vec![CompareTarget::Stdout], Vec::new());
        let candidate = RunOutcome {
            stdout: b"one\n".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let oracle = RunOutcome {
            stdout: b"two\n".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let mismatches = compare(&case, &candidate, Path::new("/a"), &oracle, Path::new("/b"));
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].target, CompareTarget::Stdout);
    }

    #[test]
    fn differing_exit_code_is_reported() {
        let case = case_with(vec![CompareTarget::ExitCode], Vec::new());
        let candidate = RunOutcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: Some(1),
        };
        let oracle = RunOutcome {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let mismatches = compare(&case, &candidate, Path::new("/a"), &oracle, Path::new("/b"));
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].target, CompareTarget::ExitCode);
    }

    #[test]
    fn stderr_is_ignored_unless_requested() {
        let case = case_with(vec![CompareTarget::Stdout], Vec::new());
        let candidate = RunOutcome {
            stdout: b"same\n".to_vec(),
            stderr: b"candidate stderr\n".to_vec(),
            exit_code: Some(0),
        };
        let oracle = RunOutcome {
            stdout: b"same\n".to_vec(),
            stderr: b"totally different oracle stderr\n".to_vec(),
            exit_code: Some(0),
        };
        let mismatches = compare(&case, &candidate, Path::new("/a"), &oracle, Path::new("/b"));
        assert!(mismatches.is_empty());
    }

    #[test]
    fn workdir_normalization_makes_different_tempdirs_compare_equal() {
        let case = case_with(
            vec![CompareTarget::Stdout],
            vec![crate::case::NormalizeRule::Workdir],
        );
        let candidate = RunOutcome {
            stdout: b"/tmp/candidate-dir\n".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let oracle = RunOutcome {
            stdout: b"/tmp/oracle-dir\n".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let mismatches = compare(
            &case,
            &candidate,
            Path::new("/tmp/candidate-dir"),
            &oracle,
            Path::new("/tmp/oracle-dir"),
        );
        assert!(mismatches.is_empty());
    }

    #[test]
    fn known_difference_checks_only_the_pinned_fields() {
        let case = case_with(Vec::new(), Vec::new());
        let kd = crate::case::KnownDifference {
            id: "KD-0000".to_string(),
            expect_stdout: Some("hi\n".to_string()),
            expect_stderr: None,
            expect_exit_code: Some(0),
        };
        let matching = RunOutcome {
            stdout: b"hi\n".to_vec(),
            stderr: b"anything, not checked".to_vec(),
            exit_code: Some(0),
        };
        assert!(compare_known_difference(&case, &kd, &matching, Path::new("/a")).is_empty());

        let wrong_stdout = RunOutcome {
            stdout: b"bye\n".to_vec(),
            stderr: Vec::new(),
            exit_code: Some(0),
        };
        let mismatches = compare_known_difference(&case, &kd, &wrong_stdout, Path::new("/a"));
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].target, CompareTarget::Stdout);
    }
}
