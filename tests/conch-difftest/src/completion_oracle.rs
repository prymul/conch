//! Drives real bash's `compgen` builtin as the oracle for Phase 6 tab
//! completion candidate generation.
//!
//! ## Why `compgen`, not real keypress-driven completion
//!
//! Real interactive tab completion needs a pty (a live readline instance
//! reacting to an actual Tab keypress) -- structurally out of scope for
//! this harness, same as `PS1`/`PS2` (see [`crate::prompt_oracle`]'s module
//! docs and the crate README's "Interactive mode scoping" section). bash
//! ships a dedicated builtin specifically for querying its own completion
//! logic *without* a terminal: `compgen [options] [word]` "generates
//! possible completion matches ... and prints them" straight to stdout, no
//! readline/pty involved (bash manual, "Programmable Completion"),
//! confirmed empirically (see this module's tests): `compgen -c` lists
//! command-name candidates (builtins, functions, aliases, keywords, and
//! every executable found across `$PATH`), `compgen -f`/`-d` list
//! filenames/directories relative to the current working directory,
//! `compgen -A function`/`-A alias`/`-A variable` list exactly those
//! categories.
//!
//! ## Determinism: a fully controlled `$PATH`, not the real one
//!
//! Every call here **replaces** `$PATH` with the exact directory list the
//! caller supplies (via a script-embedded `export PATH=...`, not
//! [`std::process::Command::env`] -- see [`compgen_via_bash`]'s doc
//! comment for why) rather than inheriting the test process's real `PATH`.
//! Without this, `compgen -c` would enumerate every command actually
//! installed on whatever machine happens to run this suite -- useless for
//! a corpus that needs the exact same, small, known candidate set on every
//! run, in CI and locally alike.
//!
//! ## Candidate ordering is deliberately not part of the comparison
//!
//! `compgen`'s own output order depends on bash's internal hash-table
//! iteration for builtins/aliases and `$PATH` directory-scan order for
//! executables -- not a stable, cross-version contract worth chasing
//! byte-for-byte the way this crate's stdout/exit-code comparisons
//! elsewhere are. [`compgen_via_bash`] returns a sorted, deduplicated
//! `Vec<String>`; callers should compare *sets* of candidates, not raw
//! output order.

use std::path::Path;

use crate::case::Invocation;
use crate::invoke::{self, ShellUnderTest};

/// Which category of candidate to query -- see the module doc comment for
/// what real bash includes in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// `compgen -c`: builtins, functions, aliases, keywords, and every
    /// executable on `$PATH` -- what bash's own default completion offers
    /// at the first word of a command line with no `complete`/`-F`
    /// programmable completion registered for it.
    Command,
    /// `compgen -f`: filenames (any type) relative to the current
    /// directory -- bash's own default completion fallback for any word
    /// position past the first, with no programmable completion
    /// registered.
    Filename,
    /// `compgen -d`: directories only.
    Directory,
    /// `compgen -A function`: shell functions only.
    Function,
    /// `compgen -A alias`: aliases only.
    Alias,
    /// `compgen -A variable`: shell + environment variable names.
    Variable,
}

impl CompletionKind {
    fn compgen_args(self) -> &'static str {
        match self {
            CompletionKind::Command => "-c",
            CompletionKind::Filename => "-f",
            CompletionKind::Directory => "-d",
            CompletionKind::Function => "-A function",
            CompletionKind::Alias => "-A alias",
            CompletionKind::Variable => "-A variable",
        }
    }
}

/// Queries real bash's `compgen` for every candidate of `kind` matching
/// `prefix`, with `$PATH` set to exactly `path_dirs` (joined with `:`,
/// in order), `functions` defined as no-op shell functions, and `aliases`
/// defined verbatim -- all via a script this function synthesizes and
/// runs through [`invoke::run`] (the same spawn/timeout/byte-capture path
/// every other oracle invocation in this crate uses), inside `workdir`.
///
/// Returns candidates sorted and deduplicated -- see the module doc
/// comment for why raw `compgen` output order isn't part of the contract
/// here.
///
/// `functions`' names are interpolated directly into the script as bash
/// function names, and must therefore already be valid, unquoted shell
/// identifiers -- this is an internal test-authoring helper, not a
/// boundary that needs to defend against arbitrary/adversarial input the
/// way a real shell's own parser does.
pub fn compgen_via_bash(
    kind: CompletionKind,
    prefix: &str,
    path_dirs: &[&Path],
    functions: &[&str],
    aliases: &[(&str, &str)],
    workdir: &Path,
) -> std::io::Result<Vec<String>> {
    let mut script = String::new();

    let path_value = path_dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    script.push_str("export PATH=");
    script.push_str(&shell_single_quote(&path_value));
    script.push('\n');

    for name in functions {
        script.push_str(name);
        script.push_str("() { :; }\n");
    }
    for (name, value) in aliases {
        script.push_str("alias ");
        script.push_str(name);
        script.push('=');
        script.push_str(&shell_single_quote(value));
        script.push('\n');
    }

    script.push_str("compgen ");
    script.push_str(kind.compgen_args());
    script.push(' ');
    script.push_str(&shell_single_quote(prefix));
    script.push('\n');

    let outcome = invoke::run(
        &ShellUnderTest::Oracle("bash"),
        Invocation::DashC,
        &script,
        workdir,
        None,
    )?;

    let text = String::from_utf8_lossy(&outcome.stdout);
    let mut candidates: Vec<String> = text
        .lines()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect();
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Creates an executable (empty, no shebang needed -- `compgen -c`
    /// only checks the executable bit and directory-entry presence, not
    /// that the file is actually runnable) file named `name` inside `dir`.
    fn make_executable(dir: &Path, name: &str) {
        let path = dir.join(name);
        fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn command_completion_finds_an_executable_on_the_controlled_path() {
        let workdir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        make_executable(bin_dir.path(), "frobnicate");
        make_executable(bin_dir.path(), "frobulate");
        make_executable(bin_dir.path(), "other");

        let candidates = compgen_via_bash(
            CompletionKind::Command,
            "frob",
            &[bin_dir.path()],
            &[],
            &[],
            workdir.path(),
        )
        .unwrap();

        assert_eq!(candidates, vec!["frobnicate", "frobulate"]);
    }

    #[test]
    fn command_completion_does_not_see_commands_outside_the_controlled_path() {
        let workdir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        make_executable(bin_dir.path(), "onlyme");

        // `ls` is a real, ordinarily-on-`$PATH` command -- proves the
        // replaced `$PATH` is genuinely exclusive, not merely prepended.
        let candidates = compgen_via_bash(
            CompletionKind::Command,
            "ls",
            &[bin_dir.path()],
            &[],
            &[],
            workdir.path(),
        )
        .unwrap();

        assert!(candidates.is_empty());
    }

    #[test]
    fn command_completion_includes_a_defined_function() {
        let workdir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();

        let candidates = compgen_via_bash(
            CompletionKind::Command,
            "my_",
            &[bin_dir.path()],
            &["my_function"],
            &[],
            workdir.path(),
        )
        .unwrap();

        assert_eq!(candidates, vec!["my_function"]);
    }

    #[test]
    fn command_completion_includes_a_defined_alias() {
        let workdir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();

        let candidates = compgen_via_bash(
            CompletionKind::Command,
            "ll",
            &[bin_dir.path()],
            &[],
            &[("ll", "ls -la")],
            workdir.path(),
        )
        .unwrap();

        assert_eq!(candidates, vec!["ll"]);
    }

    #[test]
    fn command_completion_includes_a_builtin() {
        let workdir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();

        let candidates = compgen_via_bash(
            CompletionKind::Command,
            "ech",
            &[bin_dir.path()],
            &[],
            &[],
            workdir.path(),
        )
        .unwrap();

        assert_eq!(candidates, vec!["echo"]);
    }

    #[test]
    fn filename_completion_lists_matching_files_in_the_workdir() {
        let workdir = tempfile::tempdir().unwrap();
        fs::write(workdir.path().join("foo.txt"), b"").unwrap();
        fs::write(workdir.path().join("foobar.sh"), b"").unwrap();
        fs::write(workdir.path().join("other.txt"), b"").unwrap();

        let candidates = compgen_via_bash(
            CompletionKind::Filename,
            "foo",
            &[],
            &[],
            &[],
            workdir.path(),
        )
        .unwrap();

        assert_eq!(candidates, vec!["foo.txt", "foobar.sh"]);
    }

    #[test]
    fn directory_completion_excludes_plain_files() {
        let workdir = tempfile::tempdir().unwrap();
        fs::write(workdir.path().join("foo.txt"), b"").unwrap();
        fs::create_dir(workdir.path().join("foodir")).unwrap();

        let candidates = compgen_via_bash(
            CompletionKind::Directory,
            "foo",
            &[],
            &[],
            &[],
            workdir.path(),
        )
        .unwrap();

        assert_eq!(candidates, vec!["foodir"]);
    }

    #[test]
    fn function_completion_only_lists_functions_not_builtins_or_executables() {
        let workdir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        make_executable(bin_dir.path(), "echoish");

        let candidates = compgen_via_bash(
            CompletionKind::Function,
            "echo",
            &[bin_dir.path()],
            &["echo_helper"],
            &[],
            workdir.path(),
        )
        .unwrap();

        assert_eq!(candidates, vec!["echo_helper"]);
    }

    #[test]
    fn variable_completion_finds_an_exported_variable() {
        let workdir = tempfile::tempdir().unwrap();
        let candidates = compgen_via_bash(
            CompletionKind::Variable,
            "PA",
            &[],
            &[],
            &[],
            workdir.path(),
        )
        .unwrap();
        // `PATH` is always exported by this module's own script preamble.
        assert!(candidates.contains(&"PATH".to_string()));
    }

    #[test]
    fn no_match_returns_an_empty_list_not_an_error() {
        let workdir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        let candidates = compgen_via_bash(
            CompletionKind::Command,
            "zzz_nope_zzz",
            &[bin_dir.path()],
            &[],
            &[],
            workdir.path(),
        )
        .unwrap();
        assert!(candidates.is_empty());
    }
}
