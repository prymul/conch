//! The Phase 6 tab-completion (command-position candidate generation)
//! test-case schema.
//!
//! A third, separate schema alongside [`crate::case::Case`] and
//! [`crate::prompt_case::PromptCase`] -- see those modules' own doc
//! comments for the general "why a new schema" rationale, which applies
//! here too: the candidate is conch's own
//! `conch_shell_core::completion::command_candidates` called in-process,
//! not a spawned shell binary, and the oracle is always real bash's
//! `compgen -c` (see [`crate::completion_oracle`]'s module docs), never a
//! second shell to independently agree with.
//!
//! ## Scope: command position only, and never a real builtin name
//!
//! `command_candidates` itself only ever covers *command-position*
//! completion (functions, aliases, builtins, reserved words, `$PATH`
//! executables) -- argument-position filename completion is delegated
//! wholesale to `rustyline`'s own `FilenameCompleter` on the adapter side
//! and has no `Shell`-dependent pure core to test here at all (see that
//! module's own doc comment). So every case in this corpus is implicitly
//! "at command position"; there's no field for argument position because
//! there's nothing on the candidate side that would ever consult one.
//!
//! Deliberately **no field for real conch builtin names** (`cd`, `echo`,
//! `read`, ...): conch's actual registered-builtin roster and bash's own
//! actual builtin roster are two independently-designed, genuinely
//! different (if overlapping) POSIX-plus-extensions sets -- there is no
//! principled reason to expect them to agree name-for-name at an
//! arbitrary prefix, and forcing a comparison there would be exactly the
//! "don't force a differential comparison that isn't testing the real
//! pipeline" mistake this project's own guardrails warn against. Every
//! case here instead controls its own candidate set entirely through
//! `functions`/`aliases`/`path_executables` -- picked deliberately (see
//! each case's own name in `corpus/phase6/completion_candidates.toml`) to
//! never collide with a real bash builtin/keyword/host `$PATH` command,
//! the same "obviously-fake test token" discipline
//! `completion_oracle.rs`'s own unit tests already use. One case *does*
//! deliberately target a real, shared, permanent point of agreement
//! instead: a POSIX reserved word (`while`), which both conch's
//! `word_scan::RESERVED_WORDS` and bash's own keyword table treat as
//! fixed shell grammar, not an extensible roster either shell could
//! plausibly diverge on.

use std::path::PathBuf;

use serde::Deserialize;

/// The top-level shape of a single `corpus/phase6/*.toml` completion-case
/// file -- same `[[case]]`-array-of-tables shape as
/// [`crate::case::CaseFile`]/[`crate::prompt_case::PromptCaseFile`].
#[derive(Debug, Deserialize)]
pub struct CompletionCaseFile {
    #[serde(rename = "case", default)]
    pub cases: Vec<CompletionCase>,
}

/// One `NAME=value` alias definition -- see
/// [`crate::prompt_case::EnvAssignment`] for why this is a plain struct
/// rather than a `HashMap`. Only `name` matters on the candidate side
/// (`CompletionState::aliases` is names only); `value` exists purely so
/// the oracle side (`alias name=value`, a real bash requirement) has
/// something syntactically valid to define -- its actual content is
/// never asserted on.
#[derive(Debug, Deserialize)]
pub struct AliasDefinition {
    pub name: String,
    #[serde(default = "default_alias_value")]
    pub value: String,
}

fn default_alias_value() -> String {
    "true".to_string()
}

/// A single command-position completion differential case.
#[derive(Debug, Deserialize)]
pub struct CompletionCase {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The word being completed. `""` is valid and means "match
    /// everything" (bash: a bare Tab with nothing typed yet).
    #[serde(default)]
    pub prefix: String,
    /// Function names to define on both sides (a real no-op `name() {
    /// :; }` on the oracle side; just a name on the candidate's
    /// `CompletionState::functions` -- see this module's own doc comment
    /// for why only names ever matter here).
    #[serde(default)]
    pub functions: Vec<String>,
    /// Aliases to define on both sides.
    #[serde(default)]
    pub aliases: Vec<AliasDefinition>,
    /// Executable file names to create (empty, `+x` -- see
    /// [`crate::completion_oracle`]'s own `make_executable`-equivalent
    /// helper) in a fresh, exclusive directory used as the *entire*
    /// `$PATH` on both sides -- never appended to the real, inherited
    /// `$PATH`, so a case's result can never accidentally depend on
    /// whatever happens to be installed on the machine running the
    /// suite.
    #[serde(default)]
    pub path_executables: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(skip)]
    pub source_file: PathBuf,
}

impl CompletionCase {
    /// Structural checks that don't require running anything.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("completion case name must not be empty".to_string());
        }
        let name_ok = self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.name.starts_with('-')
            && !self.name.ends_with('-');
        if !name_ok {
            return Err(format!(
                "completion case name {:?} must be kebab-case ASCII (lowercase letters, digits, \
                 hyphens; no leading/trailing hyphen)",
                self.name
            ));
        }
        if self.functions.is_empty() && self.aliases.is_empty() && self.path_executables.is_empty()
        {
            return Err(format!(
                "completion case {:?}: must define at least one of functions/aliases/\
                 path_executables, or a reserved-word-only case that needs none of them should \
                 say so via a `note` -- an entirely empty case is more likely a copy-paste \
                 mistake than an intentional reserved-word case",
                self.name
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_case() -> CompletionCase {
        CompletionCase {
            name: "example-case".to_string(),
            description: "an example".to_string(),
            tags: Vec::new(),
            prefix: "ex".to_string(),
            functions: vec!["example_fn".to_string()],
            aliases: Vec::new(),
            path_executables: Vec::new(),
            note: None,
            source_file: PathBuf::new(),
        }
    }

    #[test]
    fn valid_case_passes_validation() {
        assert!(minimal_case().validate().is_ok());
    }

    #[test]
    fn uppercase_name_is_rejected() {
        let mut case = minimal_case();
        case.name = "Example-Case".to_string();
        assert!(case.validate().is_err());
    }

    #[test]
    fn entirely_empty_candidate_sources_is_rejected() {
        let mut case = minimal_case();
        case.functions.clear();
        assert!(case.validate().is_err());
    }

    #[test]
    fn aliases_or_path_executables_alone_are_sufficient() {
        let mut case = minimal_case();
        case.functions.clear();
        case.aliases.push(AliasDefinition {
            name: "a".to_string(),
            value: "true".to_string(),
        });
        assert!(case.validate().is_ok());

        let mut case2 = minimal_case();
        case2.functions.clear();
        case2.path_executables.push("fake_exe".to_string());
        assert!(case2.validate().is_ok());
    }

    #[test]
    fn deserializes_a_minimal_case_file() {
        let toml = r#"
            [[case]]
            name = "func-completion"
            description = "d"
            prefix = "gr"
            functions = ["greet"]
        "#;
        let parsed: CompletionCaseFile = toml::from_str(toml).unwrap();
        assert_eq!(parsed.cases.len(), 1);
        let case = &parsed.cases[0];
        assert_eq!(case.prefix, "gr");
        assert_eq!(case.functions, vec!["greet".to_string()]);
        assert!(case.aliases.is_empty());
        assert!(case.path_executables.is_empty());
    }

    #[test]
    fn deserializes_all_optional_fields() {
        let toml = r#"
            [[case]]
            name = "full-case"
            description = "d"
            prefix = "x"
            functions = ["xf"]
            path_executables = ["xexe"]
            tags = ["completion"]
            note = "n"
            [[case.aliases]]
            name = "xa"
            value = "ls -la"
        "#;
        let parsed: CompletionCaseFile = toml::from_str(toml).unwrap();
        let case = &parsed.cases[0];
        assert_eq!(case.aliases.len(), 1);
        assert_eq!(case.aliases[0].name, "xa");
        assert_eq!(case.aliases[0].value, "ls -la");
        assert_eq!(case.path_executables, vec!["xexe".to_string()]);
    }

    #[test]
    fn alias_value_defaults_when_omitted() {
        let toml = r#"
            [[case]]
            name = "alias-default-value"
            description = "d"
            prefix = "x"
            [[case.aliases]]
            name = "xa"
        "#;
        let parsed: CompletionCaseFile = toml::from_str(toml).unwrap();
        assert_eq!(parsed.cases[0].aliases[0].value, "true");
    }
}
