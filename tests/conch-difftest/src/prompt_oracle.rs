//! Drives real bash's own non-interactive prompt-expansion pipeline -- the
//! oracle half of Phase 6's `PS1`/`PS2` differential testing.
//!
//! ## Why `${VAR@P}`, not a real interactive prompt draw
//!
//! `PS1`'s backslash-escape expansion (`\u`, `\h`, `\w`, `\$`, ...) plus its
//! `promptvars` parameter/command-substitution pass only normally run when
//! bash actually draws an interactive prompt -- confirmed directly:
//! `bash -c 'PS1="\u@\h "; echo "$PS1"'` prints the *raw*, unexpanded
//! template, since that expansion code path never fires for a
//! non-interactive `-c` invocation with no prompt ever drawn. Bash >= 4.4's
//! `${parameter@P}` transform operator (bash manual, "Shell Parameter
//! Expansion" -- "the expansion is a string that is the result of
//! expanding the value of parameter as if it were `PS1`") runs *exactly*
//! that same expansion pipeline on demand, with no prompt draw and no pty
//! required. Confirmed empirically against real bash 5.3 (see this
//! module's tests): backslash escapes, `promptvars` variable/command-
//! substitution expansion, and live `$?` are all reproduced faithfully.
//! This is the one bash mechanism that lets this harness reach bash's
//! *real* prompt-expansion pipeline from a plain `-c` invocation, matching
//! this crate's overall philosophy of testing against real bash rather
//! than a hand-written guess at what bash "should" do.
//!
//! ubuntu-latest (this project's CI oracle host) has shipped bash >= 5.0
//! for years, well above the 4.4 floor `@P` needs -- not a practical
//! concern for this project's CI.
//!
//! ## Non-determinism this module's callers must avoid
//!
//! A handful of bash's prompt escapes are *not* safe to put in a
//! live-oracle-compared corpus case at all, the same "exclude or normalize"
//! rule the crate README already states for `$RANDOM`/hostnames/TTY-
//! dependent formatting generally:
//!
//! - `\d`, `\t`, `\T`, `\@`, `\A`, `\D{fmt}` (wall-clock date/time) --
//!   genuinely time-of-day-dependent; the candidate (an in-process Rust
//!   call) and this oracle (a spawned bash subprocess) run at
//!   microseconds-to-milliseconds apart, which is enough to occasionally
//!   disagree right at a second/minute boundary. Worth a plain unit test
//!   with an injected/fixed clock on the conch side, if the implementation
//!   supports one -- not a live comparison here.
//! - `\j` (background job count), `\!` (history number), `\#` (command
//!   number) -- depend on interpreter-internal counters (job table size,
//!   history length, command count) that a fresh `bash -c` invocation and
//!   conch's own `Shell` initialize independently and have no reason to
//!   agree on numerically, even if both correctly implement the *escape
//!   itself*.
//! - `\s` (shell name) -- expected to permanently, deliberately differ
//!   (bash reports `"bash"`; conch should report its own name) -- a
//!   `known_difference`-schema case once that's actually decided, never a
//!   live comparison.
//!
//! `\u`/`\h`/`\H`/`\w`/`\W`/`\$`/`\n`/`\r`/`\a`/`\e`/`\\`/`\[`/`\]`/`\v`,
//! plain `promptvars` variable/command-substitution expansion, and `$?`
//! (via a literal, unescaped `$?` in the template, forced to a known value
//! per-case -- see [`expand_ps1_via_bash`]'s doc comment) are all safe,
//! deterministic, and exactly what `corpus/phase6/prompt_expansion.toml`
//! sticks to.

use std::path::Path;

use crate::case::Invocation;
use crate::invoke::{self, RunOutcome, ShellUnderTest};

/// One `NAME=value` environment variable to export before expanding a
/// template -- for a case whose template references `$SOME_VAR` via
/// `promptvars` rather than only a backslash escape.
pub type EnvVar<'a> = (&'a str, &'a str);

/// Expands `template` under real bash's own `PS1`-expansion pipeline
/// (`${PS1@P}`).
///
/// `env` is exported first; `$?` is then forced to `last_status`
/// *immediately* before the `printf` that actually triggers expansion --
/// in that exact order, deliberately, since an ordinary assignment/`export`
/// command's own exit status is `0`, and running one *after* the
/// `(exit N)` that sets up `$?` would silently overwrite it back to `0`
/// before `${PS1@P}` ever got a chance to observe it. Confirmed this
/// ordering is right by testing the opposite order first while building
/// this module and watching a `[$?]`-style template report `0` no matter
/// what `last_status` was requested.
///
/// `workdir` becomes the child's cwd via [`invoke::run`] (the same
/// spawn/timeout/byte-capture path every other oracle invocation in this
/// crate uses, so a hung bash under this mechanism gets the same
/// protection as everything else) -- pass the result through
/// [`crate::normalize::apply`] with [`crate::case::NormalizeRule::Workdir`]
/// before comparing against a candidate expanded against a *different*
/// temp directory, exactly like every `pwd`-observing case elsewhere in
/// this crate already does.
pub fn expand_ps1_via_bash(
    template: &str,
    env: &[EnvVar<'_>],
    last_status: i32,
    workdir: &Path,
) -> std::io::Result<RunOutcome> {
    run_prompt_script("PS1", template, env, last_status, workdir)
}

/// Same as [`expand_ps1_via_bash`] but for `PS2`, the continuation prompt
/// -- bash's `${PS2@P}` transform runs the identical expansion pipeline
/// against a different source variable. No special "mid-continuation"
/// shell state is needed to trigger it: `@P` expands `PS2`'s value on
/// demand the same way it does `PS1`'s, regardless of whether a real
/// continuation line is actually in progress.
pub fn expand_ps2_via_bash(
    template: &str,
    env: &[EnvVar<'_>],
    last_status: i32,
    workdir: &Path,
) -> std::io::Result<RunOutcome> {
    run_prompt_script("PS2", template, env, last_status, workdir)
}

fn run_prompt_script(
    var: &str,
    template: &str,
    env: &[EnvVar<'_>],
    last_status: i32,
    workdir: &Path,
) -> std::io::Result<RunOutcome> {
    let mut script = String::new();
    for (name, value) in env {
        script.push_str("export ");
        script.push_str(name);
        script.push('=');
        script.push_str(&shell_single_quote(value));
        script.push('\n');
    }
    script.push_str(var);
    script.push('=');
    script.push_str(&shell_single_quote(template));
    script.push('\n');
    // See this function's doc comment: must be the *last* setup statement.
    script.push_str(&format!("(exit {last_status})\n"));
    script.push_str(&format!("printf '%s' \"${{{var}@P}}\"\n"));

    invoke::run(
        &ShellUnderTest::Oracle("bash"),
        Invocation::DashC,
        &script,
        workdir,
        None,
    )
}

/// Wraps `value` in single quotes, escaping an embedded single quote the
/// POSIX-standard way (close the quote, an escaped literal quote, reopen:
/// `'\''`) -- the same technique this crate would use anywhere shell
/// source is synthesized from an arbitrary Rust string rather than
/// authored directly in a corpus file's `script`.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(template: &str) -> String {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = expand_ps1_via_bash(template, &[], 0, workdir.path()).unwrap();
        String::from_utf8(outcome.stdout).unwrap()
    }

    #[test]
    fn plain_text_with_no_escapes_expands_unchanged() {
        assert_eq!(expand("just text"), "just text");
    }

    #[test]
    fn dollar_escape_renders_a_plain_dollar_sign() {
        assert_eq!(expand(r"\$ "), "$ ");
    }

    #[test]
    fn literal_backslash_escape_renders_one_backslash() {
        assert_eq!(expand(r"a\\b"), r"a\b");
    }

    #[test]
    fn newline_escape_renders_a_real_newline() {
        assert_eq!(expand(r"a\nb"), "a\nb");
    }

    #[test]
    fn working_directory_escape_reflects_the_given_workdir() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = expand_ps1_via_bash(r"\w", &[], 0, workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        let canonical = workdir.path().canonicalize().unwrap();
        assert_eq!(printed, canonical.to_string_lossy());
    }

    #[test]
    fn basename_working_directory_escape_reflects_only_the_last_component() {
        let workdir = tempfile::tempdir().unwrap();
        let sub = workdir.path().join("myproject");
        std::fs::create_dir(&sub).unwrap();
        let outcome = expand_ps1_via_bash(r"\W", &[], 0, &sub).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed, "myproject");
    }

    #[test]
    fn last_status_is_visible_via_plain_promptvars_dollar_question() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = expand_ps1_via_bash("[$?]", &[], 42, workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed, "[42]");
    }

    #[test]
    fn zero_last_status_is_the_default_and_is_visible_too() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = expand_ps1_via_bash("[$?]", &[], 0, workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed, "[0]");
    }

    #[test]
    fn env_var_is_visible_via_promptvars_parameter_expansion() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome =
            expand_ps1_via_bash("var:$MY_VAR", &[("MY_VAR", "hello")], 0, workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed, "var:hello");
    }

    #[test]
    fn multiple_env_vars_are_all_exported() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome =
            expand_ps1_via_bash("$A-$B", &[("A", "one"), ("B", "two")], 0, workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed, "one-two");
    }

    #[test]
    fn env_value_containing_a_single_quote_is_handled_correctly() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = expand_ps1_via_bash("$X", &[("X", "it's fine")], 0, workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed, "it's fine");
    }

    #[test]
    fn template_containing_a_single_quote_is_handled_correctly() {
        // The template itself (not just an env var's value) may contain a
        // literal `'` -- exercises `shell_single_quote` on the template
        // path, not only the `export` path.
        assert_eq!(expand("it's a prompt> "), "it's a prompt> ");
    }

    #[test]
    fn ps2_expands_through_the_same_pipeline_as_ps1() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = expand_ps2_via_bash(r"> ", &[], 0, workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed, "> ");
    }

    #[test]
    fn command_substitution_in_the_template_is_expanded_via_promptvars() {
        assert_eq!(expand("cmdsub:$(echo hi)"), "cmdsub:hi");
    }

    #[test]
    fn exit_code_is_zero_for_a_successful_expansion() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = expand_ps1_via_bash("plain", &[], 0, workdir.path()).unwrap();
        assert_eq!(outcome.exit_code, Some(0));
    }
}
