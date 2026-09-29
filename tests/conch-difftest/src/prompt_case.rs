//! The Phase 6 prompt-expansion test-case schema.
//!
//! Deliberately a **separate** schema from [`crate::case::Case`], not a
//! bolt-on to it: every existing `Case` field (`script`, `invocation`,
//! `oracles`, `compare`, `stdin`, ...) exists to describe "feed this text
//! to two *shell binaries* via argv/stdin and diff stdout/stderr/exit
//! code" -- and `PS1`/`PS2` expansion structurally isn't that. There is no
//! way to make a compiled `conch` binary expand a prompt template via its
//! ordinary `-c`/script-file/stdin CLI surface at all (prompt expansion
//! only happens inside the interactive readline loop, which needs a real
//! pty this harness deliberately doesn't allocate -- see the crate
//! README's "Interactive mode scoping" section); the *candidate* side of
//! this comparison is instead a direct, in-process Rust call to a pure
//! core function (`conch_shell_core`'s prompt-expansion entry point, once
//! it lands -- see the README's "Phase 6 planning notes" for exactly what
//! that wiring looks like and why it isn't done yet). A [`PromptCase`]
//! therefore describes *shell state* (an environment, a working
//! directory, an exit status) and a *template string*, not a script.
//!
//! The oracle side has no `Invocation`/`stdin` choice either: it's always
//! real bash's own `${PARAMETER@P}` transform (bash >= 4.4 -- see
//! [`crate::prompt_oracle`]'s module docs for why this is a faithful,
//! non-interactive route into bash's *real* prompt-expansion pipeline,
//! not a hand-rolled reimplementation of what it "should" do), and always
//! `oracles = ["bash"]` in spirit -- POSIX `sh`/dash has no backslash-escape
//! prompt-expansion mechanism at all (an unset `PS1` falls back to a
//! bare `$ `/`# `, and dash never processes `\w`-style escapes in a
//! user-supplied one), so there is no second oracle to agree with here,
//! unlike most of `corpus/phase1-5/`.

use std::path::PathBuf;

use serde::Deserialize;

use crate::case::NormalizeRule;

/// The top-level shape of a single `corpus/phase6/*.toml` prompt-case
/// file -- same `[[case]]`-array-of-tables shape as [`crate::case::CaseFile`]
/// for consistency, even though the schema inside each table differs.
#[derive(Debug, Deserialize)]
pub struct PromptCaseFile {
    #[serde(rename = "case", default)]
    pub cases: Vec<PromptCase>,
}

/// Which prompt variable a case exercises. Bash expands `PS1` and `PS2`
/// through the identical escape/`promptvars` pipeline (just a different
/// source variable), so [`crate::prompt_oracle`] has one function per
/// variant rather than a shared one taking a variable name string --
/// see that module's docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptVar {
    Ps1,
    Ps2,
}

/// A single differential (or known-difference) prompt-expansion case.
#[derive(Debug, Deserialize)]
pub struct PromptCase {
    /// Unique, kebab-case identifier (unique across *this* corpus
    /// namespace -- deliberately not required to be globally unique
    /// against `corpus/phase1-5/`'s `Case::name`s too, since the two
    /// schemas are loaded and reported on independently).
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Which prompt variable this case's `template` is expanded as.
    #[serde(default = "default_prompt_var")]
    pub prompt: PromptVar,
    /// The raw, unexpanded prompt template -- e.g. `"\\u@\\h:\\w\\$ "`.
    /// Always applied via a *single-quoted* shell assignment on the
    /// oracle side ([`crate::prompt_oracle`]), so write this exactly as a
    /// user would in `.bashrc` (backslash escapes un-doubled from Rust's
    /// own string-literal escaping -- TOML basic strings need `\\` for a
    /// literal backslash, same as this doc comment does).
    pub template: String,
    /// Environment variables to set (both sides -- see
    /// [`crate::prompt_oracle`]) before expanding `template`, for a case
    /// whose template references `$SOME_VAR` directly (bash's
    /// `promptvars` parameter-expansion pass, on by default) rather than
    /// only a backslash escape.
    #[serde(default)]
    pub env: Vec<EnvAssignment>,
    /// The value `$?` must hold at the moment of expansion -- see
    /// [`crate::prompt_oracle`]'s doc comment for exactly how both sides
    /// arrange this without any later setup command silently clobbering
    /// it first. Defaults to `0` (the common case: a template that
    /// doesn't reference `$?`/exit-status at all).
    #[serde(default)]
    pub last_status: i32,
    /// A subdirectory (created fresh under this run's own temp workdir)
    /// to actually expand the template *from* -- needed for any
    /// `\w`/`\W`-exercising case to get a working directory whose
    /// basename is asserted on, distinct from the bare temp root every
    /// other case implicitly runs from. `None` means "the workdir root
    /// itself".
    #[serde(default)]
    pub cwd_subdir: Option<String>,
    /// Same normalization rules [`crate::case::Case`] uses -- in practice
    /// only [`NormalizeRule::Workdir`] is meaningful here (for a
    /// `\w`/`\W`-exercising template whose expansion embeds the run's own
    /// absolute temp path), but reusing the enum instead of inventing a
    /// prompt-specific one keeps `normalize::apply` a single, shared
    /// implementation.
    #[serde(default)]
    pub normalize: Vec<NormalizeRule>,
    /// Set for a template whose expansion conch deliberately and
    /// permanently diverges on (e.g. `\s` reporting `"conch"` rather than
    /// `"bash"` -- see `known-differences.md` once such a decision is
    /// actually made). When set, no live bash oracle run happens at all
    /// for this case.
    #[serde(default)]
    pub known_difference: Option<PromptKnownDifference>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(skip)]
    pub source_file: PathBuf,
}

/// One `NAME=value` pair -- a plain 2-tuple struct (rather than reusing a
/// `HashMap<String, String>`) so the corpus's TOML can express it as an
/// ordinary `{ name = "...", value = "..." }` inline table and so
/// iteration order (the order `export` statements are emitted in, on the
/// oracle side) is exactly the order the case author wrote, even though
/// no case in this corpus currently depends on export ordering mattering.
#[derive(Debug, Deserialize)]
pub struct EnvAssignment {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct PromptKnownDifference {
    pub id: String,
    pub expect: String,
}

impl PromptCase {
    /// Structural checks that don't require running anything.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("prompt case name must not be empty".to_string());
        }
        let name_ok = self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.name.starts_with('-')
            && !self.name.ends_with('-');
        if !name_ok {
            return Err(format!(
                "prompt case name {:?} must be kebab-case ASCII (lowercase letters, digits, \
                 hyphens; no leading/trailing hyphen)",
                self.name
            ));
        }
        if self.template.is_empty() {
            return Err(format!(
                "prompt case {:?}: template must not be empty (an empty PS1/PS2 is legal in \
                 bash but uninteresting to test)",
                self.name
            ));
        }
        if let Some(subdir) = &self.cwd_subdir
            && (subdir.is_empty() || subdir.contains('/') || subdir.contains('\\'))
        {
            return Err(format!(
                "prompt case {:?}: cwd_subdir must be a single path component (no `/`), got \
                 {subdir:?} -- nested subdirectories aren't needed by anything in this corpus \
                 yet and would need explicit `create_dir_all` handling if that ever changes",
                self.name
            ));
        }
        Ok(())
    }
}

fn default_prompt_var() -> PromptVar {
    PromptVar::Ps1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_case() -> PromptCase {
        PromptCase {
            name: "example-case".to_string(),
            description: "an example".to_string(),
            tags: Vec::new(),
            prompt: PromptVar::Ps1,
            template: "\\$ ".to_string(),
            env: Vec::new(),
            last_status: 0,
            cwd_subdir: None,
            normalize: Vec::new(),
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
    fn empty_template_is_rejected() {
        let mut case = minimal_case();
        case.template = String::new();
        assert!(case.validate().is_err());
    }

    #[test]
    fn uppercase_name_is_rejected() {
        let mut case = minimal_case();
        case.name = "Example-Case".to_string();
        assert!(case.validate().is_err());
    }

    #[test]
    fn nested_cwd_subdir_is_rejected() {
        let mut case = minimal_case();
        case.cwd_subdir = Some("a/b".to_string());
        assert!(case.validate().is_err());
    }

    #[test]
    fn single_component_cwd_subdir_is_allowed() {
        let mut case = minimal_case();
        case.cwd_subdir = Some("sub".to_string());
        assert!(case.validate().is_ok());
    }

    #[test]
    fn deserializes_a_minimal_case_file() {
        let toml = r#"
            [[case]]
            name = "ps1-basic"
            description = "a literal PS1 with no escapes expands unchanged"
            template = "$ "
        "#;
        let parsed: PromptCaseFile = toml::from_str(toml).unwrap();
        assert_eq!(parsed.cases.len(), 1);
        let case = &parsed.cases[0];
        assert_eq!(case.prompt, PromptVar::Ps1);
        assert_eq!(case.last_status, 0);
        assert!(case.env.is_empty());
        assert_eq!(case.cwd_subdir, None);
    }

    #[test]
    fn deserializes_all_optional_fields() {
        let toml = r#"
            [[case]]
            name = "ps1-with-env-and-status"
            description = "d"
            prompt = "ps2"
            template = "[$?] \\w> "
            last_status = 7
            cwd_subdir = "sub"
            normalize = ["workdir"]
            tags = ["prompt"]
            note = "n"
            [[case.env]]
            name = "MY_VAR"
            value = "hi"
        "#;
        let parsed: PromptCaseFile = toml::from_str(toml).unwrap();
        let case = &parsed.cases[0];
        assert_eq!(case.prompt, PromptVar::Ps2);
        assert_eq!(case.last_status, 7);
        assert_eq!(case.cwd_subdir, Some("sub".to_string()));
        assert_eq!(case.env.len(), 1);
        assert_eq!(case.env[0].name, "MY_VAR");
        assert_eq!(case.env[0].value, "hi");
        assert_eq!(case.normalize, vec![NormalizeRule::Workdir]);
    }

    #[test]
    fn known_difference_case_deserializes() {
        let toml = r#"
            [[case]]
            name = "ps1-shell-name-is-conch-not-bash"
            description = "d"
            template = "\\s"
            known_difference = { id = "KD-0000", expect = "conch" }
        "#;
        let parsed: PromptCaseFile = toml::from_str(toml).unwrap();
        let case = &parsed.cases[0];
        let kd = case.known_difference.as_ref().unwrap();
        assert_eq!(kd.id, "KD-0000");
        assert_eq!(kd.expect, "conch");
    }
}
