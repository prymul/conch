//! `exec [command [argument...]]` (POSIX special builtin) — replaces
//! this process's image with `command` via `execve` (never returns on
//! success), rather than forking a child the way an ordinary command
//! invocation does.
//!
//! # Grounding: what `exec`'s target actually resolves against
//!
//! Confirmed against real bash (not assumed from POSIX's own wording
//! alone, which is less specific about this exact point):
//! - `exec`'s target is looked up as an **external command only** —
//!   shell functions are never consulted at all (`f() { :; }; exec f` is
//!   `exec: f: not found`, exit 127, exactly as if `f` weren't defined).
//! - A **builtin** target *is* recognized, but since a builtin has no
//!   process of its own to `execve` into in the first place, `exec`
//!   instead runs it directly and then terminates the whole shell with
//!   that builtin's own exit status (`exec pwd` prints the cwd, then the
//!   shell exits `0` — confirmed empirically: nothing after `exec pwd`
//!   in the same script ever runs).
//! - A target that resolves to neither a builtin nor a real `PATH`
//!   entry is fatal to a **non-interactive** shell specifically (`exec
//!   nonexistent; echo after` never reaches `after`, exit 127) — the
//!   same "fatal in non-interactive mode only" pattern already
//!   established for `${var:?}`/`set -u` elsewhere in this codebase, and
//!   distinct from an *ordinary* command-not-found error (which only
//!   fails that one command, script continues regardless of
//!   interactivity).
//!
//! # Known gap: bare `exec > file` (no command, redirects only)
//!
//! POSIX-valid, and meant to apply the redirect *permanently* to the
//! running shell rather than temporarily to one command's invocation.
//! Not implemented: every builtin in this crate communicates its output
//! through the [`conch_shell_core::Builtin`] trait's buffered
//! `stdout`/`stderr` writers (see that trait's own docs), which
//! `conch-shell-core::exec::exec_builtin` then optionally redirects to a
//! file *for that one call only* — there's no channel through this
//! abstraction for a builtin to reach out and permanently `dup2` a file
//! onto the shell's own real fd 1/2/etc. for every *subsequent* command
//! too. A real fix needs new plumbing in `conch-shell-core::exec` itself
//! (a shell-level "permanently redirected fd" table consulted by every
//! future command's own stdio setup), not something this builtin can
//! express alone — flagged clearly rather than silently no-op'd.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt as _;

use conch_shell_core::{Builtin, Shell};

pub struct Exec;
impl Builtin for Exec {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let Some((name, rest)) = args.split_first() else {
            let _ = writeln!(
                stderr,
                "exec: a bare `exec` with only redirects (no command) is not yet supported"
            );
            return 0;
        };

        if shell.builtin(name).is_some() {
            // Deliberately *not* this call's own `stdin`/`stdout`/`stderr`
            // parameters: those are whatever `exec_builtin`
            // (`conch-shell-core::exec`) set up for *this* "exec" command's
            // own invocation, which — for its `stdout`/`stderr` half —
            // means an in-memory buffer that `exec_builtin` only ever
            // flushes to the real process stdout/applies a redirect to
            // *after* this function returns. Since this path never
            // returns at all (`std::process::exit` below), anything
            // written to that buffer would be silently lost — caught by
            // a manual end-to-end smoke test against the real compiled
            // binary, not by a unit test (`exec pwd`'s output vanishing
            // entirely was the actual observed symptom). Using the real
            // `std::io` handles directly sidesteps that, at the cost of
            // a known, narrower gap: `exec pwd > file`'s own redirect
            // doesn't apply to `pwd`'s output this way (the same
            // category of gap already documented on `Eval`/`Command` for
            // an analogous "nested output bypasses the outer builtin's
            // own redirect capture" reason).
            let _ = stdin;
            let _ = stdout;
            let builtin = shell
                .take_builtin(name)
                .expect("just checked this builtin exists");
            let mut real_stdin = std::io::stdin();
            let mut real_stdout = std::io::stdout();
            let mut real_stderr = std::io::stderr();
            let status = builtin.run(
                shell,
                rest,
                &mut real_stdin,
                &mut real_stdout,
                &mut real_stderr,
            );
            shell.register_builtin(name.to_string(), builtin);
            shell.run_exit_trap();
            std::process::exit(status);
        }

        match conch_shell_core::find_in_path(shell, name) {
            Some(_) => {
                let mut command = std::process::Command::new(name);
                command
                    .args(rest)
                    .current_dir(&shell.cwd)
                    .env_clear()
                    .envs(&shell.env_vars);
                // Resets this shell's own customized signal dispositions
                // (the interactive-idle `SIG_IGN`s, any `trap`'d
                // handler) back to their OS defaults before the image is
                // replaced -- required for the same reason an ordinary
                // external-command spawn's `pre_exec` hook does this
                // (`Shell::prepare_child_for_job_control`'s own docs):
                // the *new* program must never inherit dispositions that
                // only meant something for *this* shell's own
                // deferred-signal-dispatch machinery, which no longer
                // exists once this call replaces the process. `false`:
                // `exec` never creates a new process group -- it's still
                // the exact same process, just running different code.
                shell.prepare_child_for_job_control(&mut command, false);
                let err = command.exec(); // never returns on success
                let _ = writeln!(stderr, "exec: {name}: {err}");
            }
            None => {
                let _ = writeln!(stderr, "exec: {name}: not found");
            }
        }

        // Confirmed against real bash: a failed `exec` (target not
        // found, or the `execve` call itself failed) is fatal to a
        // non-interactive shell specifically -- see the module docs.
        if !shell.is_interactive {
            shell.run_exit_trap();
            std::process::exit(127);
        }
        127
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // `exec`'s two "always terminates" outcomes (a successful replace,
    // or a failed one on a *non-interactive* shell) both call
    // `std::process::exit` directly, which would abort this whole test
    // binary -- matching this codebase's own established precedent for
    // every other `std::process::exit`-calling path (`report_expand_error`'s
    // fatal branch, `errexit`), the only safe thing to unit-test in
    // process is the one outcome that *doesn't* terminate: a failed
    // lookup on an *interactive* shell. The terminating paths are
    // covered by a manual end-to-end smoke test against the real
    // compiled binary instead (see this phase's own progress notes).

    #[test]
    fn unresolvable_command_on_an_interactive_shell_reports_and_returns_without_exiting() {
        let mut shell = Shell::new();
        shell.is_interactive = true;
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = Exec.run(
            &mut shell,
            &s(&["nope_not_a_real_command_hopefully"]),
            &mut stdin,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 127);
        assert!(String::from_utf8(stderr).unwrap().contains("not found"));
    }

    #[test]
    fn bare_exec_with_no_command_is_a_documented_no_op() {
        let mut shell = Shell::new();
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = Exec.run(&mut shell, &s(&[]), &mut stdin, &mut stdout, &mut stderr);
        assert_eq!(status, 0);
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("not yet supported")
        );
    }
}
