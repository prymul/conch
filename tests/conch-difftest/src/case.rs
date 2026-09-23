//! The Phase 1 test-case schema: what a single differential test case looks
//! like on disk (TOML) and in memory.
//!
//! See `corpus/phase1/*.toml` for real examples and the crate-level README
//! for the full field-by-field rationale.

use std::path::PathBuf;

use serde::Deserialize;

/// The top-level shape of a single corpus `*.toml` file: an array of
/// `[[case]]` tables, oils-for-unix-spec-test style (many small, focused
/// cases grouped into one readable file per category).
#[derive(Debug, Deserialize)]
pub struct CaseFile {
    #[serde(rename = "case", default)]
    pub cases: Vec<Case>,
}

/// A single differential (or known-difference) test case.
#[derive(Debug, Deserialize)]
pub struct Case {
    /// Unique, kebab-case identifier used in test names and reports.
    pub name: String,
    /// One-line human description of what this case demonstrates.
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Which real shell(s) to treat as the oracle. Each is compared to the
    /// candidate independently, so listing more than one shell asserts
    /// they already agree with each other for this construct.
    #[serde(default = "default_oracles")]
    pub oracles: Vec<Oracle>,
    /// How the script is fed to the shell under test.
    #[serde(default)]
    pub invocation: Invocation,
    /// Which parts of the process outcome must match between candidate
    /// and oracle. Defaults to stdout + exit code: stderr wording is
    /// notoriously shell- and version-specific (see
    /// `command-not-found-exit-code` for a case that would fail on stderr
    /// wording alone despite bash and dash fully agreeing on behavior),
    /// so it's opt-in per case rather than on by default.
    #[serde(default = "default_compare")]
    pub compare: Vec<CompareTarget>,
    /// Byte-level normalization passes applied identically to both
    /// candidate and oracle output before comparing (e.g. substituting
    /// each run's own temp working directory with a stable placeholder).
    #[serde(default)]
    pub normalize: Vec<NormalizeRule>,
    /// The shell source under test.
    pub script: String,
    /// When set, this case is not compared against a live oracle at all;
    /// it pins conch's own expected output directly. Use this for
    /// deliberate, documented divergences from bash/sh behavior (see
    /// `known-differences.md`), never as a shortcut around actually
    /// running the oracle.
    #[serde(default)]
    pub known_difference: Option<KnownDifference>,
    /// Free-text context for maintainers; not used by the harness itself.
    #[serde(default)]
    pub note: Option<String>,
    /// Which corpus file this case was loaded from. Populated by
    /// [`crate::corpus::load_dir`] after parsing, not part of the TOML
    /// schema.
    #[serde(skip)]
    pub source_file: PathBuf,
}

impl Case {
    /// Structural checks that don't require running anything. Returns a
    /// human-readable error describing the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("case name must not be empty".to_string());
        }
        let name_ok = !self.name.is_empty()
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.name.starts_with('-')
            && !self.name.ends_with('-');
        if !name_ok {
            return Err(format!(
                "case name {:?} must be kebab-case ASCII (lowercase letters, digits, hyphens; \
                 no leading/trailing hyphen)",
                self.name
            ));
        }
        if self.script.trim().is_empty() {
            return Err(format!("case {:?}: script must not be empty", self.name));
        }
        match &self.known_difference {
            Some(kd) => {
                if kd.expect_stdout.is_none()
                    && kd.expect_stderr.is_none()
                    && kd.expect_exit_code.is_none()
                {
                    return Err(format!(
                        "case {:?}: known_difference must pin at least one of expect_stdout, \
                         expect_stderr, expect_exit_code",
                        self.name
                    ));
                }
            }
            None => {
                if self.oracles.is_empty() {
                    return Err(format!(
                        "case {:?}: oracles must not be empty unless known_difference is set",
                        self.name
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Oracle {
    Bash,
    Sh,
}

impl Oracle {
    /// The program name passed to `Command::new`, resolved via `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            Oracle::Bash => "bash",
            Oracle::Sh => "sh",
        }
    }
}

impl std::fmt::Display for Oracle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.program())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Invocation {
    /// `<shell> -c '<script>'` — Phase 1's `conch -c "cmd"` mode.
    #[default]
    DashC,
    /// `<shell> <path-to-tempfile-containing-script>` — Phase 1's
    /// `conch script.sh` mode.
    ScriptFile,
    /// `<script bytes> | <shell>` — a non-PTY proxy for interactive input.
    /// True PTY-based interactive/prompt testing (fixed 24x80 terminal,
    /// mirroring brush's e2e approach) is deliberately out of scope until
    /// a later phase's line-editing/prompt work lands; see the crate
    /// README for the full rationale.
    StdinPipe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompareTarget {
    Stdout,
    Stderr,
    ExitCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NormalizeRule {
    /// Strip a single trailing `\n`, if present, from both sides before
    /// comparing.
    TrailingNewline,
    /// Replace every occurrence of *that run's own* temp working
    /// directory path with a stable `<CWD>` placeholder before comparing.
    /// Needed for any case whose output embeds an absolute path (`pwd`,
    /// `cd` errors, ...), since the candidate and oracle each run in their
    /// own freshly created temp directory and so never share a literal
    /// path even when semantically correct.
    Workdir,
}

#[derive(Debug, Deserialize)]
pub struct KnownDifference {
    /// Identifier cross-referenced in `known-differences.md`, e.g.
    /// `"KD-0001"`.
    pub id: String,
    #[serde(default)]
    pub expect_stdout: Option<String>,
    #[serde(default)]
    pub expect_stderr: Option<String>,
    #[serde(default)]
    pub expect_exit_code: Option<i32>,
}

fn default_oracles() -> Vec<Oracle> {
    vec![Oracle::Bash]
}

fn default_compare() -> Vec<CompareTarget> {
    vec![CompareTarget::Stdout, CompareTarget::ExitCode]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_case() -> Case {
        Case {
            name: "example-case".to_string(),
            description: "an example".to_string(),
            tags: Vec::new(),
            oracles: default_oracles(),
            invocation: Invocation::default(),
            compare: default_compare(),
            normalize: Vec::new(),
            script: "echo hi".to_string(),
            known_difference: None,
            note: None,
            source_file: PathBuf::new(),
        }
    }

    #[test]
    fn valid_case_passes_validation() {
        assert!(minimal_case().validate().is_ok());
    }

    #[test]
    fn empty_script_is_rejected() {
        let mut case = minimal_case();
        case.script = "   ".to_string();
        assert!(case.validate().is_err());
    }

    #[test]
    fn uppercase_name_is_rejected() {
        let mut case = minimal_case();
        case.name = "Example-Case".to_string();
        assert!(case.validate().is_err());
    }

    #[test]
    fn empty_oracles_without_known_difference_is_rejected() {
        let mut case = minimal_case();
        case.oracles = Vec::new();
        assert!(case.validate().is_err());
    }

    #[test]
    fn empty_oracles_with_known_difference_is_allowed() {
        let mut case = minimal_case();
        case.oracles = Vec::new();
        case.known_difference = Some(KnownDifference {
            id: "KD-0000".to_string(),
            expect_stdout: Some("hi\n".to_string()),
            expect_stderr: None,
            expect_exit_code: None,
        });
        assert!(case.validate().is_ok());
    }

    #[test]
    fn known_difference_without_any_expectation_is_rejected() {
        let mut case = minimal_case();
        case.known_difference = Some(KnownDifference {
            id: "KD-0000".to_string(),
            expect_stdout: None,
            expect_stderr: None,
            expect_exit_code: None,
        });
        assert!(case.validate().is_err());
    }

    #[test]
    fn deserializes_a_minimal_case_file() {
        let toml = r#"
            [[case]]
            name = "echo-basic"
            description = "echo prints its arguments space-joined"
            script = '''
            echo hello world
            '''
        "#;
        let parsed: CaseFile = toml::from_str(toml).unwrap();
        assert_eq!(parsed.cases.len(), 1);
        let case = &parsed.cases[0];
        assert_eq!(case.name, "echo-basic");
        assert_eq!(case.oracles, vec![Oracle::Bash]);
        assert_eq!(case.invocation, Invocation::DashC);
        assert_eq!(
            case.compare,
            vec![CompareTarget::Stdout, CompareTarget::ExitCode]
        );
    }

    #[test]
    fn deserializes_all_optional_fields() {
        let toml = r#"
            [[case]]
            name = "cd-then-pwd"
            description = "cd followed by pwd reflects the new directory"
            oracles = ["bash", "sh"]
            invocation = "script-file"
            compare = ["stdout", "stderr", "exit-code"]
            normalize = ["workdir", "trailing-newline"]
            tags = ["builtins", "cd"]
            note = "needs workdir normalization since pwd embeds an absolute path"
            script = '''
            cd sub
            pwd
            '''
        "#;
        let parsed: CaseFile = toml::from_str(toml).unwrap();
        let case = &parsed.cases[0];
        assert_eq!(case.oracles, vec![Oracle::Bash, Oracle::Sh]);
        assert_eq!(case.invocation, Invocation::ScriptFile);
        assert_eq!(
            case.compare,
            vec![
                CompareTarget::Stdout,
                CompareTarget::Stderr,
                CompareTarget::ExitCode
            ]
        );
        assert_eq!(
            case.normalize,
            vec![NormalizeRule::Workdir, NormalizeRule::TrailingNewline]
        );
    }
}
