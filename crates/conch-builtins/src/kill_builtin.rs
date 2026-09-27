//! `kill [-s sigspec | -sigspec | -n signum] pid|%job...` / `kill -l
//! [sigspec...]` — reuses Phase 4's job-table/signal-handling
//! infrastructure entirely (`conch_shell_core::Shell::kill_by_spec`,
//! itself built on the same `JobTable::resolve_spec`/`Shell::signal_job`
//! `fg`/`bg` already use — see `resolve_fg_bg_job` in this crate's root
//! module for the shape this mirrors) rather than reimplementing
//! process/signal lookup or job-spec parsing: this builtin is a thin
//! argument-parsing wrapper, exactly like `trap`'s own relationship to
//! `conch-shell-core::signals`.
//!
//! This crate deliberately has no direct `nix` dependency of its own
//! (see the crate root module docs) — every signal name/number this
//! builtin ever handles stays a plain `&str` all the way into
//! `conch-shell-core`, never a raw `nix::sys::signal::Signal`.

use std::io::{Read, Write};

use conch_shell_core::{Builtin, Shell};

pub struct Kill;
impl Builtin for Kill {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let mut rest = args;

        if matches!(rest.first().map(String::as_str), Some("-l" | "-L")) {
            return run_list(&rest[1..], stdout, stderr);
        }

        let mut signal_name = "TERM".to_string();
        match rest.first().map(String::as_str) {
            Some("-s") => {
                let Some(name) = rest.get(1) else {
                    let _ = writeln!(stderr, "kill: -s: option requires an argument");
                    return 2;
                };
                signal_name = name.clone();
                rest = &rest[2..];
            }
            Some("-n") => {
                let Some(number) = rest.get(1) else {
                    let _ = writeln!(stderr, "kill: -n: option requires an argument");
                    return 2;
                };
                signal_name = number.clone();
                rest = &rest[2..];
            }
            Some(arg) if arg.len() > 1 && arg.starts_with('-') && arg != "--" => {
                signal_name = arg[1..].to_string();
                rest = &rest[1..];
            }
            Some("--") => rest = &rest[1..],
            _ => {}
        }

        if rest.is_empty() {
            let _ = writeln!(
                stderr,
                "kill: usage: kill [-s sigspec | -sigspec | -n signum] pid|%job..."
            );
            return 2;
        }

        let mut status = 0;
        for target in rest {
            if let Err(err) = shell.kill_by_spec(target, &signal_name) {
                let _ = writeln!(stderr, "{err}");
                status = 1;
            }
        }
        status
    }
}

/// `kill -l [sigspec...]` — see [`Shell::all_signal_names`]/
/// [`Shell::describe_signal`]. Known cosmetic gap: bare `kill -l`'s
/// listing is one bare name per line here, not real bash's own
/// numbered, tab-aligned multi-column grid (` 1) SIGHUP  2) SIGINT
/// ...`) — POSIX doesn't mandate that exact presentation (only that a
/// list of names be produced), and it isn't even consistent between real
/// shells (dash's own `-l` output is a single space-separated line), so
/// this doesn't attempt to replicate bash's specific column layout.
fn run_list(specs: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    if specs.is_empty() {
        for name in Shell::all_signal_names() {
            let _ = writeln!(stdout, "{name}");
        }
        return 0;
    }
    let mut status = 0;
    for spec in specs {
        match Shell::describe_signal(spec) {
            Ok(name) => {
                let _ = writeln!(stdout, "{name}");
            }
            Err(err) => {
                let _ = writeln!(stderr, "{err}");
                status = 1;
            }
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(shell: &mut Shell, args: &[String]) -> (i32, String, String) {
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = Kill.run(shell, args, &mut stdin, &mut stdout, &mut stderr);
        (
            status,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_job_spec_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["%1"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("no such job"));
    }

    #[test]
    fn nonexistent_pid_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["999999"]));
        assert_eq!(status, 1);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn dash_0_checks_existence_without_sending_a_real_signal() {
        // `kill -0 $$` (this very test process) must succeed -- confirmed
        // against a real strict-mode differential regression this fixes:
        // `kill -0` is the standard "is this job still running?" idiom
        // (`corpus/phase4/jobs_status.toml`), and before this crate's own
        // `kill` builtin existed at all, it fell through to the real
        // external `kill(1)`, which already handles `0` correctly.
        let mut shell = Shell::new();
        let self_pid = std::process::id().to_string();
        let (status, _, stderr) = run(&mut shell, &s(&["-0", self_pid.as_str()]));
        assert_eq!(status, 0, "stderr: {stderr}");
    }

    #[test]
    fn invalid_target_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["not-a-pid"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("process or job IDs"));
    }

    #[test]
    fn invalid_signal_name_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["-NOTASIGNAL", "1"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("invalid signal"));
    }

    #[test]
    fn no_operands_is_a_usage_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&[]));
        assert_eq!(status, 2);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn dash_l_with_no_operand_lists_every_signal() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&mut shell, &s(&["-l"]));
        assert_eq!(status, 0);
        assert!(stdout.lines().any(|l| l == "KILL"));
        assert!(stdout.lines().any(|l| l == "TERM"));
    }

    #[test]
    fn dash_l_translates_a_number_to_a_name() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&mut shell, &s(&["-l", "9"]));
        assert_eq!(status, 0);
        assert_eq!(stdout.trim(), "KILL");
    }

    #[test]
    fn dash_l_translates_a_128_plus_n_exit_status() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&mut shell, &s(&["-l", "137"]));
        assert_eq!(status, 0);
        assert_eq!(stdout.trim(), "KILL");
    }
}
