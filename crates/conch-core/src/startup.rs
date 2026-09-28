//! Startup-file sourcing: `~/.conchrc`, read once at the start of an
//! interactive session.
//!
//! # What this collapses bash's login/non-login/`BASH_ENV` split into
//!
//! Real bash reads a *different* set of files depending on session kind:
//! an interactive *login* shell (or `--login`) reads `/etc/profile` then
//! the first existing/readable of `~/.bash_profile`/`~/.bash_login`/
//! `~/.profile`; an interactive *non-login* shell reads `~/.bashrc`
//! instead; a *non-interactive* shell sources whatever `$BASH_ENV` names
//! (expanded, but not `$PATH`-searched). Conch has no login-shell concept
//! modeled anywhere — no `-l` flag, no leading-`-` `argv[0]` check — and
//! this phase's plan describes a single `~/.conchrc`-style file, not a
//! reproduction of bash's full matrix. This is that deliberate
//! simplification: **interactive sessions only** read **one** file,
//! `~/.conchrc`; there is no login/non-login distinction and no
//! `$BASH_ENV`/`$ENV`-equivalent for non-interactive (`-c`/script-file)
//! runs. A future phase that adds a real login-shell concept would be
//! the natural place to widen this, not a silent gap today.
//!
//! # Gated on `Shell::is_interactive`
//!
//! [`source_startup_file`] refuses to do anything at all unless
//! `shell.is_interactive` is already `true` (security review finding,
//! confirmed against real bash: `~/.bashrc`-equivalent files only ever
//! run for an interactive shell, never for `-c`/a script file). `conch`'s
//! own binary crate only ever calls this from `run_interactive` — which
//! already sets `is_interactive` first — so this check is currently
//! always true by the time it's reached in practice; it's still checked
//! explicitly, defensively, rather than relying on "nothing else calls
//! this" staying true forever as a caller-side invariant no one has to
//! think about again. Without it, every non-interactive/CI/cron
//! invocation of conch that happened to call this would also execute the
//! user's interactive rc file — a real blast-radius expansion beyond
//! what bash itself does.
//!
//! # Reuses `.`/`source`'s own execution plumbing
//!
//! Sourcing `~/.conchrc` runs through the *exact same* mechanism the
//! `.`/`source` builtin itself uses — `Shell::take_builtin(".")`, run it,
//! `Shell::register_builtin` it back (the established "run a builtin
//! with a `&mut Shell` already borrowed" pattern this codebase already
//! uses elsewhere for the same borrow-conflict reason) — rather than
//! duplicating that builtin's file-read-and-parse-and-execute logic here.
//! `conch-shell-builtins::Source::run`'s own `read_source_file` helper
//! already skips `$PATH` search for any name containing `/` (POSIX 2.14
//! behavior), so handing it `~/.conchrc`'s fully-resolved *absolute* path
//! (this module resolves `~` to `$HOME` itself first, rather than
//! relying on any shell-level tilde expansion — there is none at this
//! point, nothing has been parsed yet) is handled correctly with zero new
//! file-reading logic. Confirmed against real bash directly (security
//! review): a `chmod 777` rc file sources with zero complaint — bash
//! doesn't gate on rc-file permissions, so this doesn't either, matching
//! parity rather than leaving a gap.
//!
//! A missing `~/.conchrc` is silently skipped, not an error — matching
//! real bash's own behavior for a missing `~/.bashrc`.

use std::path::PathBuf;

use crate::Shell;

/// `~/.conchrc`'s fully-resolved path — `None` if `$HOME` can't be
/// determined (nowhere to look; silently means "no startup file to
/// source", not an error — see this module's own docs).
#[must_use]
pub fn conchrc_path(shell: &Shell) -> Option<PathBuf> {
    shell
        .get_var("HOME")
        .map(|home| PathBuf::from(home).join(".conchrc"))
}

/// Sources `~/.conchrc` into `shell`, if it exists — see this module's
/// own docs for exactly what runs, the `Shell::is_interactive` gate, and
/// why a missing file is silently fine. Any *other* failure (a real I/O
/// error reading an existing file, or a syntax error in it) is reported
/// to stderr, matching how an interactive `. somefile` failing would be
/// reported, but does not prevent the interactive session from starting.
pub fn source_startup_file(shell: &mut Shell) {
    if !shell.is_interactive {
        return;
    }
    let Some(path) = conchrc_path(shell) else {
        return;
    };
    if !path.exists() {
        return;
    }
    let Some(source) = shell.take_builtin(".") else {
        // Should be unreachable in practice (`.`/`source` are always
        // registered by `conch_shell_builtins::register_all` before
        // `run_interactive` starts) -- fail soft rather than panic if
        // that invariant is ever violated.
        return;
    };
    let path_arg = path.to_string_lossy().into_owned();
    let mut stdin = std::io::empty();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let status = source.run(shell, &[path_arg], &mut stdin, &mut stdout, &mut stderr);
    shell.register_builtin(".", source);
    if status != 0 {
        eprintln!("conch: warning: ~/.conchrc exited with status {status}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Builtin;
    use std::io::{Read, Write};

    /// A self-contained stand-in for `conch-shell-builtins::Source`
    /// (which this crate can't depend on -- `conch-shell-builtins`
    /// depends on `conch-shell-core`, not the other way around) that's
    /// just enough to exercise `source_startup_file`'s own
    /// take/run/register-back sequencing: read the named file, parse it,
    /// and execute it against the same `Shell`.
    struct TestSource;
    impl Builtin for TestSource {
        fn run(
            &self,
            shell: &mut Shell,
            args: &[String],
            _stdin: &mut dyn Read,
            _stdout: &mut dyn Write,
            stderr: &mut dyn Write,
        ) -> i32 {
            let Some(path) = args.first() else {
                return 2;
            };
            match std::fs::read_to_string(path) {
                Ok(source) => match conch_shell_parser::parse(&source) {
                    Ok(list) => crate::exec_command_list(&list, shell),
                    Err(err) => {
                        let _ = writeln!(stderr, "{err}");
                        2
                    }
                },
                Err(err) => {
                    let _ = writeln!(stderr, "{err}");
                    1
                }
            }
        }
    }

    fn interactive_shell_with_home(home: &std::path::Path) -> Shell {
        let mut shell = Shell::new();
        shell.is_interactive = true;
        shell
            .env_vars
            .insert("HOME".to_string(), home.to_string_lossy().into_owned());
        shell.register_builtin(".", Box::new(TestSource));
        shell
    }

    #[test]
    fn resolves_under_home() {
        let mut shell = Shell::new();
        shell
            .env_vars
            .insert("HOME".to_string(), "/home/conch".to_string());
        assert_eq!(
            conchrc_path(&shell),
            Some(PathBuf::from("/home/conch/.conchrc"))
        );
    }

    #[test]
    fn none_without_home() {
        let mut shell = Shell::new();
        shell.env_vars.remove("HOME");
        shell.shell_vars.remove("HOME");
        assert_eq!(conchrc_path(&shell), None);
    }

    #[test]
    fn missing_file_is_silently_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut shell = interactive_shell_with_home(dir.path());
        // Must not panic, error, or leave `.` unregistered.
        source_startup_file(&mut shell);
        assert!(shell.builtin(".").is_some());
    }

    #[test]
    fn existing_file_is_sourced_into_the_current_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A plain assignment (not `export`) so this test's own `TestSource`
        // stub -- which has no builtins registered at all, unlike a real
        // shell via `conch_shell_builtins::register_all` -- doesn't need
        // one: a bare `NAME=value` with no command name is ordinary
        // executor-level assignment syntax, not a builtin dispatch.
        std::fs::write(dir.path().join(".conchrc"), "GREETING=hi\n").expect("write");
        let mut shell = interactive_shell_with_home(dir.path());
        source_startup_file(&mut shell);
        assert_eq!(shell.get_var("GREETING"), Some("hi"));
        assert!(shell.builtin(".").is_some());
    }

    #[test]
    fn non_interactive_shell_never_sources_the_rc_file() {
        // Security review finding: `~/.bashrc`-equivalent files only run
        // for an interactive shell in real bash -- a `-c`/script-file
        // conch invocation must never execute the user's interactive rc
        // file, even if one happens to exist at the resolved path.
        let dir = tempfile::tempdir().expect("tempdir");
        // A plain assignment (not `export`) so this test's own `TestSource`
        // stub -- which has no builtins registered at all, unlike a real
        // shell via `conch_shell_builtins::register_all` -- doesn't need
        // one: a bare `NAME=value` with no command name is ordinary
        // executor-level assignment syntax, not a builtin dispatch.
        std::fs::write(dir.path().join(".conchrc"), "GREETING=hi\n").expect("write");
        let mut shell = interactive_shell_with_home(dir.path());
        shell.is_interactive = false;
        source_startup_file(&mut shell);
        assert_eq!(shell.get_var("GREETING"), None);
    }
}
