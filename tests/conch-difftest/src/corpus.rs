//! Loading and validating the on-disk case corpus.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::case::{Case, CaseFile};

#[derive(Debug)]
pub struct CorpusError(String);

impl fmt::Display for CorpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CorpusError {}

/// The absolute path to this crate's `corpus/` directory, resolved from the
/// crate manifest so it's independent of the test process's current
/// working directory (which individual cases may themselves change via
/// `cd` once actually executed).
pub fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// Recursively loads and validates every `*.toml` case file under `dir`.
///
/// Returns every parsed [`Case`] in a deterministic (path-sorted) order.
/// Fails closed: a malformed file, a case that fails
/// [`Case::validate`], or a case name reused across files is a hard error
/// rather than a silently-skipped file, since a corpus that can grow to
/// hundreds of cases across phases needs authoring mistakes caught here
/// rather than discovered as a confusing runtime skip later.
pub fn load_dir(dir: &Path) -> Result<Vec<Case>, CorpusError> {
    let mut files = Vec::new();
    collect_toml_files(dir, &mut files)?;
    files.sort();

    let mut cases = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    for file in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|err| CorpusError(format!("{}: {err}", file.display())))?;
        let parsed: CaseFile = toml::from_str(&text)
            .map_err(|err| CorpusError(format!("{}: {err}", file.display())))?;

        for mut case in parsed.cases {
            case.source_file = file.clone();
            case.validate()
                .map_err(|msg| CorpusError(format!("{}: {msg}", file.display())))?;
            if !seen_names.insert(case.name.clone()) {
                return Err(CorpusError(format!(
                    "{}: duplicate case name {:?} (case names must be unique across the whole \
                     corpus, since they're used as test identifiers in reports)",
                    file.display(),
                    case.name
                )));
            }
            cases.push(case);
        }
    }

    Ok(cases)
}

fn collect_toml_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), CorpusError> {
    let entries =
        std::fs::read_dir(dir).map_err(|err| CorpusError(format!("{}: {err}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|err| CorpusError(format!("{}: {err}", dir.display())))?;
        let path = entry.path();
        if path.is_dir() {
            collect_toml_files(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_case_file(dir: &Path, name: &str, contents: &str) {
        let mut file = std::fs::File::create(dir.join(name)).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn loads_cases_from_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        write_case_file(
            dir.path(),
            "a.toml",
            r#"
            [[case]]
            name = "top-level-case"
            description = "d"
            script = "echo hi"
            "#,
        );
        write_case_file(
            &dir.path().join("sub"),
            "b.toml",
            r#"
            [[case]]
            name = "nested-case"
            description = "d"
            script = "echo hi"
            "#,
        );

        let cases = load_dir(dir.path()).unwrap();
        let mut names: Vec<_> = cases.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["nested-case", "top-level-case"]);
    }

    #[test]
    fn duplicate_case_names_across_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_case_file(
            dir.path(),
            "a.toml",
            r#"
            [[case]]
            name = "dup"
            description = "d"
            script = "echo hi"
            "#,
        );
        write_case_file(
            dir.path(),
            "b.toml",
            r#"
            [[case]]
            name = "dup"
            description = "d"
            script = "echo hi"
            "#,
        );

        let err = load_dir(dir.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate case name"));
    }

    #[test]
    fn invalid_case_is_rejected_with_file_context() {
        let dir = tempfile::tempdir().unwrap();
        write_case_file(
            dir.path(),
            "a.toml",
            r#"
            [[case]]
            name = "bad"
            description = "d"
            script = "   "
            "#,
        );

        let err = load_dir(dir.path()).unwrap_err();
        assert!(err.to_string().contains("a.toml"));
        assert!(err.to_string().contains("script must not be empty"));
    }

    #[test]
    fn real_phase1_corpus_loads_and_validates() {
        // This exercises the actual shipped corpus, not a fixture -- it's
        // the same load the integration tests perform.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let phase1 = manifest_dir.join("corpus").join("phase1");
        if !phase1.exists() {
            // The scratch/validation sandbox doesn't ship the real corpus.
            return;
        }
        let cases = load_dir(&phase1).unwrap();
        assert!(!cases.is_empty());
    }
}
