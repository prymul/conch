//! Builtin commands: `cd`, `exit`, `export`, `echo`, `pwd` (Phase 1); the
//! `break`/`continue` POSIX special builtins (Phase 3); `local`,
//! `return`, a narrow `set --`, and `shift` (the functions/positional-
//! parameters follow-up); and `jobs`/`fg`/`bg`/`wait`/`trap` (Phase 4 job
//! control).
//!
//! Every job-control builtin here is a thin wrapper over
//! `conch-shell-core`'s own job-table/signal-handling API
//! (`conch_shell_core::job`/`conch_shell_core::signals`) — this crate
//! deliberately has no direct `nix` dependency of its own (see e.g.
//! [`Shell::trap_set_by_name`]'s own docs for why), so every one of these
//! builtins works entirely in terms of job ids/PIDs-as-strings and
//! [`conch_shell_core::TrapAction`] (which has no OS-signal type in it at
//! all), never a raw `nix::sys::signal::Signal`.

use std::io::Write;

use conch_shell_core::{Builtin, ControlFlow, Shell, TrapAction};

/// Registers every builtin this crate implements into `shell` — as either
/// a *regular* builtin ([`Shell::register_builtin`]) or a *special* one
/// ([`Shell::register_special_builtin`]), per POSIX 2.9.1/2.9.5's own
/// classification (`cd`/`echo`/`pwd`/`local` are ordinary utilities-ish
/// builtins; `break`/`continue`/`exit`/`export`/`return`/`set`/`shift`
/// are all on POSIX's special-builtin list). This distinction is
/// structural only — see [`Shell::special_builtins`]'s docs for why it's
/// *not* used to stop a same-named shell function from shadowing either
/// kind (confirmed against real bash's own default, non-`--posix`,
/// behavior).
pub fn register_all(shell: &mut Shell) {
    shell.register_builtin("cd", Box::new(Cd));
    shell.register_special_builtin("exit", Box::new(Exit));
    shell.register_special_builtin("export", Box::new(Export));
    shell.register_builtin("echo", Box::new(Echo));
    shell.register_builtin("pwd", Box::new(Pwd));
    shell.register_special_builtin("break", Box::new(Break));
    shell.register_special_builtin("continue", Box::new(Continue));
    shell.register_builtin("local", Box::new(Local));
    shell.register_special_builtin("return", Box::new(Return));
    shell.register_special_builtin("set", Box::new(Set));
    shell.register_special_builtin("shift", Box::new(Shift));
    shell.register_builtin("jobs", Box::new(Jobs));
    shell.register_builtin("fg", Box::new(Fg));
    shell.register_builtin("bg", Box::new(Bg));
    shell.register_special_builtin("wait", Box::new(Wait));
    shell.register_special_builtin("trap", Box::new(Trap));
}

struct Cd;
impl Builtin for Cd {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let target = match args.first() {
            Some(dir) => dir.clone(),
            None => match shell.get_var("HOME") {
                Some(home) => home.to_string(),
                None => {
                    let _ = writeln!(stderr, "cd: HOME not set");
                    return 1;
                }
            },
        };

        match std::env::set_current_dir(&target) {
            Ok(()) => {
                shell.cwd = match std::env::current_dir() {
                    Ok(cwd) => cwd,
                    Err(err) => {
                        let _ = writeln!(stderr, "cd: {err}");
                        return 1;
                    }
                };
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "cd: {target}: {err}");
                1
            }
        }
    }
}

struct Exit;
impl Builtin for Exit {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        // POSIX: `exit [n]` — if n is omitted, use the exit status of the
        // last command executed, not 0.
        let code = args
            .first()
            .and_then(|arg| arg.parse::<i32>().ok())
            .unwrap_or(shell.last_status);

        // POSIX: "the trap on EXIT shall be executed before the shell
        // terminates" -- every path that ends this process (this
        // builtin, falling off the end of a `-c`/script, or the
        // interactive loop ending) needs this same call; see
        // `Shell::run_exit_trap`'s own docs for why it's safe to call
        // even when nothing was ever trapped (a no-op) or when the trap
        // action itself calls `exit` again (already-taken, so it can't
        // recurse into itself).
        shell.run_exit_trap();

        // Unconditional: this always terminates the real process, no
        // signaling/indirection needed. That's correct even for `exit`
        // run inside a subshell — confirmed against real bash it only
        // terminates that subshell, not the parent — because
        // `conch-shell-core::exec`'s subshell execution runs the
        // subshell as a genuinely separate child process (re-execing
        // `conch -c <body>`), so *this* call, from inside that child,
        // naturally only ends the child. See that function's doc
        // comment for the full reasoning (and why an in-process
        // subshell design — an earlier candidate — couldn't make this
        // builtin correct no matter how it signaled).
        std::process::exit(code);
    }
}

struct Export;
impl Builtin for Export {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        for arg in args {
            match arg.split_once('=') {
                Some((name, value)) => {
                    shell.env_vars.insert(name.to_string(), value.to_string());
                }
                None => {
                    // Bare `export NAME` promotes an existing shell
                    // variable to an exported one.
                    if let Some(value) = shell.shell_vars.remove(arg) {
                        shell.env_vars.insert(arg.clone(), value);
                    } else {
                        shell.env_vars.entry(arg.clone()).or_default();
                    }
                }
            }
        }
        0
    }
}

struct Echo;
impl Builtin for Echo {
    fn run(
        &self,
        _shell: &mut Shell,
        args: &[String],
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let _ = writeln!(stdout, "{}", args.join(" "));
        0
    }
}

struct Pwd;
impl Builtin for Pwd {
    fn run(
        &self,
        shell: &mut Shell,
        _args: &[String],
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let _ = writeln!(stdout, "{}", shell.cwd.display());
        0
    }
}

/// `break [n]` / `continue [n]` (POSIX special builtins) share
/// everything except which [`ControlFlow`] variant they request — both
/// need the same `[n]` parsing (defaults to `1`; must be a positive
/// integer) and the same "only meaningful inside a loop" guard, matching
/// real bash's own wording for both.
fn run_loop_control(
    shell: &mut Shell,
    args: &[String],
    stderr: &mut dyn Write,
    name: &str,
    make: fn(u32) -> ControlFlow,
) -> i32 {
    let level = match args.first() {
        None => 1,
        Some(arg) => match arg.parse::<u32>() {
            // POSIX: n must be >= 1; bash additionally clamps an
            // out-of-range n down to the current nesting depth rather
            // than erroring (confirmed against real bash: `break 5` two
            // loops deep still breaks both, with the same "only
            // meaningful..." diagnostic for the remainder — see
            // conch-shell-core::exec's module docs) rather than treating
            // n itself as invalid, so only `0` and non-numeric/negative
            // text are rejected here.
            Ok(0) | Err(_) => {
                let _ = writeln!(stderr, "{name}: {arg}: loop count out of range");
                return 1;
            }
            Ok(n) => n,
        },
    };

    // Confirmed against real bash: `break`/`continue` outside any
    // enclosing loop is a harmless no-op (script execution continues
    // normally afterward) that only prints a diagnostic — it must *not*
    // set a pending signal a later, unrelated loop could mistakenly
    // catch.
    if shell.loop_depth == 0 {
        let _ = writeln!(
            stderr,
            "{name}: only meaningful in a `for', `while', or `until' loop"
        );
        return 0;
    }

    shell.pending_control_flow = Some(make(level));
    0
}

struct Break;
impl Builtin for Break {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        run_loop_control(shell, args, stderr, "break", ControlFlow::Break)
    }
}

struct Continue;
impl Builtin for Continue {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        run_loop_control(shell, args, stderr, "continue", ControlFlow::Continue)
    }
}

/// `local [NAME[=VALUE]]...` (bash extension — not POSIX). Only valid
/// inside a function call (confirmed against real bash: `local: can only
/// be used in a function`, `$?` = 1, and — confirmed separately — no
/// partial effect at all in that case, not even for a bare `local` with
/// zero arguments).
///
/// A single `local` invocation still processes every argument even after
/// one is rejected as an invalid name (confirmed against real bash:
/// `local 1x=2 y=3` still declares `y`, with `$?` = 1 only because of the
/// earlier, unrelated failure) — this doesn't `return` early on that
/// error, just records it and keeps going.
struct Local;
impl Builtin for Local {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if !shell.in_function_call() {
            let _ = writeln!(stderr, "local: can only be used in a function");
            return 1;
        }

        let mut status = 0;
        for arg in args {
            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            if !is_valid_identifier(name) {
                let _ = writeln!(stderr, "local: `{arg}': not a valid identifier");
                status = 1;
                continue;
            }
            let previous = shell.capture_var(name);
            shell.record_local(name.to_string(), previous);
            shell.set_local(name, value);
        }
        status
    }
}

/// The same `NAME` rule `conch-shell-parser` enforces for a function
/// definition/`for` loop variable (`[A-Za-z_][A-Za-z0-9_]*`) — duplicated
/// here (rather than depending on `conch-shell-parser` just for this)
/// since it's a five-line, self-contained rule and `conch-shell-builtins`
/// otherwise has no reason to depend on the parser crate at all.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `return [n]` (POSIX special builtin). Only valid inside a function
/// call (a sourced-script `return` is POSIX-valid too, but `.`/`source`
/// isn't implemented yet — see the module docs) — confirmed against real
/// bash this is a genuine error (unlike `break`/`continue` outside a
/// loop, which silently no-op): `return: can only `return' from a
/// function or sourced script`, `$?` = 2, and nothing after it in the
/// same list runs (matching an ordinary command failure, not a
/// loop-control no-op).
struct Return;
impl Builtin for Return {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if !shell.in_function_call() {
            let _ = writeln!(
                stderr,
                "return: can only `return' from a function or sourced script"
            );
            return 2;
        }

        // POSIX: `n` omitted -> the exit status of the last command
        // executed (same convention `exit`, conch-shell-builtins, already
        // follows). Confirmed against real bash a non-numeric `n` is a
        // real error ("numeric argument required", `$?` = 2) that still
        // stops the function immediately, exactly like a valid `return`
        // does -- it doesn't fall through and keep running the rest of
        // the function body.
        let code = match args.first() {
            None => shell.last_status,
            Some(arg) => match arg.parse::<i32>() {
                Ok(code) => code,
                Err(_) => {
                    let _ = writeln!(stderr, "return: {arg}: numeric argument required");
                    shell.pending_control_flow = Some(ControlFlow::Return(2));
                    return 2;
                }
            },
        };
        // Confirmed against real bash `return`'s argument wraps into an
        // unsigned byte immediately (observable via `$?` inside the very
        // same shell, not just the final process exit code) -- `return
        // 300` leaves `$?` at 44, `return -1` leaves it at 255 -- the
        // same `rem_euclid(256)` conch's binary crate's `main` already
        // uses for the whole *process's* own final exit code, applied
        // here instead so it's correct for an in-shell `$?` observation
        // too, not just a `-c`/script-file invocation's process exit.
        shell.pending_control_flow = Some(ControlFlow::Return(code.rem_euclid(256)));
        0
    }
}

/// `set -- [arg...]` — a deliberately narrow slice of real `set`: only
/// the `--`-prefixed positional-parameter-replacement form is
/// implemented. Every other `set` form (options like `-e`/`-x`, `set` on
/// its own to list variables, `set -o ...`, ...) is a later phase (see
/// the module docs) and reported clearly rather than silently
/// misbehaving.
struct Set;
impl Builtin for Set {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.first().map(String::as_str) == Some("--") {
            shell.positional_params = args[1..].to_vec();
            return 0;
        }
        let _ = writeln!(
            stderr,
            "conch: only `set -- [arg...]` is supported so far (a narrow initial implementation -- full `set` is a later phase)"
        );
        1
    }
}

/// `shift [n]` (POSIX special builtin) — removes the first `n` (default
/// `1`) positional parameters. Confirmed against real bash: `n` greater
/// than `$#` is an all-or-nothing error (`$?` = 1, positional parameters
/// left completely unchanged), as is a negative `n` (with an explicit
/// "shift count out of range" diagnostic in that case specifically,
/// unlike the silent `n > $#` case — this always prints one, which is
/// harmless either way since `tests/conch-difftest` only compares stderr
/// wording when a case opts into it); `shift 0` is a valid no-op.
struct Shift;
impl Builtin for Shift {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let n = match args.first() {
            None => 1,
            Some(arg) => match arg.parse::<i64>() {
                Ok(n) => n,
                Err(_) => {
                    let _ = writeln!(stderr, "shift: {arg}: numeric argument required");
                    return 1;
                }
            },
        };
        let out_of_range =
            n < 0 || usize::try_from(n).is_ok_and(|n| n > shell.positional_params.len());
        if out_of_range {
            let _ = writeln!(stderr, "shift: shift count out of range");
            return 1;
        }
        // `n` is already confirmed in `0..=positional_params.len()`.
        shell.positional_params.drain(0..n as usize);
        0
    }
}

// ---- job control: jobs / fg / bg / wait / trap ----------------------------

/// `jobs [-p] [job_spec...]` — lists background/stopped jobs.
/// `job_spec` arguments (beyond `-p`) are accepted but not (yet) used to
/// filter the listing — every currently tracked job is always shown,
/// matching bare `jobs`' own behavior and a harmless superset of a
/// filtered listing's.
///
/// After listing, purges every job that was *already* finished *and*
/// already reported once before (see
/// [`Shell::purge_finished_notified_jobs`]) — the same "show a completed
/// job's status exactly once, then forget it" policy real bash follows,
/// applied here since `jobs` is one of the two places (the other: the
/// interactive prompt loop's own per-prompt notification pass, `conch`)
/// that "reports" a job's state at all.
struct Jobs;
impl Builtin for Jobs {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let pids_only = args.iter().any(|arg| arg == "-p");
        let mut lines = Vec::new();
        for job in shell.job_table.iter() {
            if pids_only {
                lines.push(job.leader.to_string());
            } else {
                lines.push(format!(
                    "[{}]  {:<11} {}",
                    job.id,
                    job.state.label(),
                    job.command
                ));
            }
            // `jobs` itself is what marks a *currently* `Running`/
            // `Stopped` job's state as "already reported" — a completed
            // job it prints here is exactly the one report real bash
            // guarantees before forgetting it.
        }
        for line in &lines {
            let _ = writeln!(stdout, "{line}");
        }
        // Every job just listed above has now had its current state
        // reported once — mark it `notified` so a `Done`/`Signaled` one
        // gets purged below (and a `Running`/`Stopped` one simply
        // doesn't get re-announced by the interactive prompt loop's own
        // notification pass for the *same* state again).
        let ids: Vec<u32> = shell.job_table.iter().map(|job| job.id).collect();
        for id in ids {
            if let Some(job) = shell.job_table.get_mut(id) {
                job.notified = true;
            }
        }
        shell.purge_finished_notified_jobs();
        0
    }
}

/// Resolves a `fg`/`bg`-style optional job-spec argument, reporting
/// real bash's own "no job control" error first when appropriate (both
/// builtins need this exact same guard-then-resolve sequence).
fn resolve_fg_bg_job(
    shell: &Shell,
    args: &[String],
    name: &str,
    stderr: &mut dyn Write,
) -> Result<u32, i32> {
    if !shell.job_control_active {
        let _ = writeln!(stderr, "{name}: no job control in this shell");
        return Err(1);
    }
    shell
        .job_table
        .resolve_spec(args.first().map(String::as_str))
        .map_err(|err| {
            let _ = writeln!(stderr, "{name}: {err}");
            1
        })
}

/// `fg [job_spec]` — resumes a stopped-or-backgrounded job in the
/// foreground (terminal ownership, blocking wait, stoppable via Ctrl-Z
/// again) — see [`conch_shell_core::resume_job_in_foreground`].
struct Fg;
impl Builtin for Fg {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let id = match resolve_fg_bg_job(shell, args, "fg", stderr) {
            Ok(id) => id,
            Err(status) => return status,
        };
        match conch_shell_core::resume_job_in_foreground(shell, id) {
            Ok(status) => status,
            Err(err) => {
                let _ = writeln!(stderr, "{err}");
                1
            }
        }
    }
}

/// `bg [job_spec]` — resumes a stopped job in the background (`SIGCONT`,
/// no terminal handoff, doesn't block) — see
/// [`Shell::resume_job_in_background`].
struct Bg;
impl Builtin for Bg {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let id = match resolve_fg_bg_job(shell, args, "bg", stderr) {
            Ok(id) => id,
            Err(status) => return status,
        };
        let command = shell
            .job_table
            .get(id)
            .map(|job| job.command.clone())
            .unwrap_or_default();
        match shell.resume_job_in_background(id) {
            Ok(()) => {
                let _ = writeln!(stdout, "[{id}]  {command} &");
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "{err}");
                1
            }
        }
    }
}

/// `wait [pid...]` (POSIX special builtin) — with operands, blocks until
/// each named job finishes (in argument order) and reports the *last*
/// one's exit status as `$?`; with none, blocks until every currently
/// running background job finishes and always reports `0` — see
/// [`Shell::wait_for_pid`]/[`Shell::wait_for_all_background_jobs`] for
/// the actual blocking mechanics.
struct Wait;
impl Builtin for Wait {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.is_empty() {
            shell.wait_for_all_background_jobs();
            shell.purge_finished_notified_jobs();
            return 0;
        }

        let mut status = 0;
        for arg in args {
            let Ok(pid) = arg.parse::<i32>() else {
                let _ = writeln!(stderr, "wait: {arg}: arguments must be process or job IDs");
                status = 127;
                continue;
            };
            match shell.wait_for_pid(pid) {
                Some(code) => status = code,
                None => {
                    let _ = writeln!(stderr, "wait: pid {pid} is not a child of this shell");
                    status = 127;
                }
            }
        }
        shell.purge_finished_notified_jobs();
        status
    }
}

/// `trap [-p] [action] [signal...]` (POSIX special builtin) — see
/// [`Shell::trap_set_by_name`]/[`Shell::trap_describe_by_name`]/
/// [`Shell::trap_list`] for the actual signal-name parsing/disposition
/// bookkeeping this wraps.
///
/// `action`: `-` resets each named signal to its original disposition
/// ([`TrapAction::Default`]); any other first non-flag argument is the
/// command text to run ([`TrapAction::Command`]) — including an empty
/// string, which is *not* a no-op text to run but [`TrapAction::Ignore`]
/// (POSIX: `trap '' SIG` ignores the signal, distinct from never
/// trapping it at all — see that variant's own docs for why the
/// distinction matters).
struct Trap;
impl Builtin for Trap {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.first().map(String::as_str) == Some("-p") {
            let specs = &args[1..];
            if specs.is_empty() {
                for line in shell.trap_list() {
                    let _ = writeln!(stdout, "{line}");
                }
                return 0;
            }
            let mut status = 0;
            for spec in specs {
                match shell.trap_describe_by_name(spec) {
                    Ok(Some(line)) => {
                        let _ = writeln!(stdout, "{line}");
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = writeln!(stderr, "{err}");
                        status = 1;
                    }
                }
            }
            return status;
        }

        let Some(action_arg) = args.first() else {
            // Bare `trap`: list every currently registered trap.
            for line in shell.trap_list() {
                let _ = writeln!(stdout, "{line}");
            }
            return 0;
        };

        let action = if action_arg == "-" {
            TrapAction::Default
        } else if action_arg.is_empty() {
            TrapAction::Ignore
        } else {
            TrapAction::Command(action_arg.clone())
        };

        let specs = &args[1..];
        if specs.is_empty() {
            let _ = writeln!(stderr, "trap: usage: trap [-p] [action] [signal...]");
            return 2;
        }

        let mut status = 0;
        for spec in specs {
            if let Err(err) = shell.trap_set_by_name(spec, action.clone()) {
                let _ = writeln!(stderr, "{err}");
                status = 1;
            }
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(builtin: &dyn Builtin, shell: &mut Shell, args: &[String]) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = builtin.run(shell, args, &mut stdout, &mut stderr);
        (
            status,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn register_all_registers_every_builtin() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        for name in [
            "cd", "exit", "export", "echo", "pwd", "break", "continue", "local", "return", "set",
            "shift",
        ] {
            assert!(shell.builtin(name).is_some(), "missing builtin: {name}");
        }
    }

    #[test]
    fn register_all_classifies_special_vs_regular_builtins_per_posix_2_9_1() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        for name in [
            "break", "continue", "exit", "export", "return", "set", "shift",
        ] {
            assert!(
                shell.is_special_builtin(name),
                "expected {name} to be special"
            );
        }
        for name in ["cd", "echo", "pwd", "local"] {
            assert!(
                !shell.is_special_builtin(name),
                "expected {name} to be regular"
            );
        }
    }

    #[test]
    fn export_with_value_sets_env_var() {
        let mut shell = Shell::new();
        run(&Export, &mut shell, &["FOO=bar".to_string()]);
        assert_eq!(shell.get_var("FOO"), Some("bar"));
    }

    #[test]
    fn bare_export_promotes_shell_var() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("FOO".to_string(), "bar".to_string());
        run(&Export, &mut shell, &["FOO".to_string()]);
        assert_eq!(shell.get_var("FOO"), Some("bar"));
        assert!(!shell.shell_vars.contains_key("FOO"));
    }

    #[test]
    fn cd_updates_cwd() {
        let mut shell = Shell::new();
        let original = shell.cwd.clone();
        let (status, _, _) = run(&Cd, &mut shell, &["/tmp".to_string()]);
        assert_eq!(status, 0);
        assert_ne!(shell.cwd, original);
        // Restore so other tests running in-process aren't affected.
        std::env::set_current_dir(&original).unwrap();
    }

    #[test]
    fn cd_nonexistent_dir_fails() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Cd, &mut shell, &["/no/such/path/hopefully".to_string()]);
        assert_eq!(status, 1);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn echo_writes_args_to_stdout() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&Echo, &mut shell, &["hi".to_string(), "there".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(stdout, "hi there\n");
    }

    #[test]
    fn pwd_writes_cwd_to_stdout() {
        let mut shell = Shell::new();
        let expected = format!("{}\n", shell.cwd.display());
        let (status, stdout, _) = run(&Pwd, &mut shell, &[]);
        assert_eq!(status, 0);
        assert_eq!(stdout, expected);
    }

    #[test]
    fn break_inside_a_loop_sets_pending_control_flow() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        let (status, _, stderr) = run(&Break, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(stderr.is_empty());
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Break(1)));
    }

    #[test]
    fn break_with_explicit_level() {
        let mut shell = Shell::new();
        shell.loop_depth = 3;
        run(&Break, &mut shell, &["2".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Break(2)));
    }

    #[test]
    fn continue_inside_a_loop_sets_pending_control_flow() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        run(&Continue, &mut shell, &[]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Continue(1)));
    }

    #[test]
    fn break_outside_any_loop_is_a_harmless_no_op() {
        // Confirmed against real bash: `break` outside a loop just
        // prints a diagnostic and does *not* stop the rest of the
        // script -- it must never set a pending signal here.
        let mut shell = Shell::new();
        assert_eq!(shell.loop_depth, 0);
        let (status, _, stderr) = run(&Break, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(!stderr.is_empty());
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn break_zero_is_out_of_range() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        let (status, _, stderr) = run(&Break, &mut shell, &["0".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("out of range"));
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn break_non_numeric_argument_is_out_of_range() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        let (status, _, stderr) = run(&Break, &mut shell, &["nope".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("out of range"));
    }

    // ---- local ---------------------------------------------------------------

    #[test]
    fn local_outside_a_function_errors_and_has_no_effect() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Local, &mut shell, &["X=1".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("can only be used in a function"));
        assert_eq!(shell.get_var("X"), None);
    }

    #[test]
    fn local_with_value_shadows_and_is_recorded_for_restoration() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        let (status, _, _) = run(&Local, &mut shell, &["X=inner".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("X"), Some("inner"));
        shell.pop_local_frame();
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn bare_local_with_no_value_makes_the_name_genuinely_unset() {
        // Confirmed against real bash: `local X` (no `=`) makes
        // `${X+is-set}` empty -- genuinely unset, not merely `""`.
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        run(&Local, &mut shell, &["X".to_string()]);
        assert_eq!(shell.get_var("X"), None);
        shell.pop_local_frame();
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn local_of_an_already_exported_variable_stays_exported() {
        // Confirmed against real bash: `export X=1; f() { local X=2; };
        // f` still shows `X=2` in a child process's inherited
        // environment -- `local` inherits the export attribute of an
        // existing same-named global, it doesn't strip it.
        let mut shell = Shell::new();
        shell.env_vars.insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        run(&Local, &mut shell, &["X=inner".to_string()]);
        assert!(shell.env_vars.contains_key("X"));
        assert_eq!(shell.get_var("X"), Some("inner"));
    }

    #[test]
    fn local_invalid_identifier_errors_but_still_processes_later_arguments() {
        // Confirmed against real bash: `local 1x=2 y=3` still declares
        // `y`, with `$?` = 1 only because of the earlier, unrelated
        // failure.
        let mut shell = Shell::new();
        shell.push_local_frame();
        let (status, _, stderr) = run(&Local, &mut shell, &["1x=2".to_string(), "y=3".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("not a valid identifier"));
        assert_eq!(shell.get_var("y"), Some("3"));
    }

    // ---- return ----------------------------------------------------------------

    #[test]
    fn return_outside_a_function_errors_without_setting_pending_control_flow() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Return, &mut shell, &[]);
        assert_eq!(status, 2);
        assert!(stderr.contains("can only `return'"));
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn return_with_explicit_code_sets_pending_control_flow() {
        let mut shell = Shell::new();
        shell.push_local_frame();
        run(&Return, &mut shell, &["5".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(5)));
    }

    #[test]
    fn return_with_no_argument_uses_the_last_status() {
        let mut shell = Shell::new();
        shell.last_status = 7;
        shell.push_local_frame();
        run(&Return, &mut shell, &[]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(7)));
    }

    #[test]
    fn return_wraps_an_out_of_range_code_into_a_byte_like_bash_does() {
        // Confirmed against real bash: `return 300` leaves `$?` at 44,
        // `return -1` leaves it at 255 -- wrapped immediately, observable
        // in the very same shell, not deferred to process exit.
        let mut shell = Shell::new();
        shell.push_local_frame();
        run(&Return, &mut shell, &["300".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(44)));

        let mut shell = Shell::new();
        shell.push_local_frame();
        run(&Return, &mut shell, &["-1".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(255)));
    }

    #[test]
    fn return_non_numeric_argument_still_stops_the_call() {
        let mut shell = Shell::new();
        shell.push_local_frame();
        let (status, _, stderr) = run(&Return, &mut shell, &["abc".to_string()]);
        assert_eq!(status, 2);
        assert!(stderr.contains("numeric argument required"));
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(2)));
    }

    // ---- set -- / shift ----------------------------------------------------------

    #[test]
    fn set_dash_dash_replaces_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["old".to_string()];
        let (status, _, _) = run(
            &Set,
            &mut shell,
            &["--".to_string(), "a".to_string(), "b".to_string()],
        );
        assert_eq!(status, 0);
        assert_eq!(
            shell.positional_params,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn set_dash_dash_alone_clears_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["old".to_string()];
        run(&Set, &mut shell, &["--".to_string()]);
        assert!(shell.positional_params.is_empty());
    }

    #[test]
    fn set_without_dash_dash_is_reported_as_unsupported() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Set, &mut shell, &["-e".to_string()]);
        assert_eq!(status, 1);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn shift_default_removes_one_positional_parameter() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (status, _, _) = run(&Shift, &mut shell, &[]);
        assert_eq!(status, 0);
        assert_eq!(
            shell.positional_params,
            vec!["b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn shift_n_removes_n_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        run(&Shift, &mut shell, &["2".to_string()]);
        assert_eq!(shell.positional_params, vec!["c".to_string()]);
    }

    #[test]
    fn shift_zero_is_a_no_op() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string()];
        let (status, _, _) = run(&Shift, &mut shell, &["0".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(shell.positional_params, vec!["a".to_string()]);
    }

    #[test]
    fn shift_beyond_available_params_errors_without_changing_them() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string(), "b".to_string()];
        let (status, _, _) = run(&Shift, &mut shell, &["5".to_string()]);
        assert_eq!(status, 1);
        assert_eq!(
            shell.positional_params,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn shift_negative_count_errors() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string()];
        let (status, _, stderr) = run(&Shift, &mut shell, &["-1".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("out of range"));
    }

    // ---- job control: jobs / fg / bg / wait / trap ---------------------
    //
    // The actual job-table-populated, real-process-backed behavior of
    // these builtins (a real backgrounded job showing up in `jobs`,
    // `fg`/`bg` actually resuming one, `wait` actually blocking) needs a
    // genuinely spawned process to exercise meaningfully — like
    // `exec_subshell`'s own execution (see `conch-shell-core::exec`'s
    // test module docs), that's `tests/conch-difftest`'s job, not a
    // unit test here. What's covered here is everything reachable
    // without spawning anything: error paths, and `trap`'s pure
    // string-table bookkeeping (already itself exercised more directly
    // in `conch-shell-core::signals`' own tests; these confirm the
    // builtin wires arguments to it correctly).

    #[test]
    fn fg_without_job_control_is_a_clear_error() {
        let mut shell = Shell::new();
        assert!(!shell.job_control_active);
        let (status, _, stderr) = run(&Fg, &mut shell, &[]);
        assert_eq!(status, 1);
        assert!(stderr.contains("no job control"));
    }

    #[test]
    fn bg_without_job_control_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Bg, &mut shell, &[]);
        assert_eq!(status, 1);
        assert!(stderr.contains("no job control"));
    }

    #[test]
    fn wait_on_an_unknown_pid_is_a_clear_error_without_blocking() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Wait, &mut shell, &["999999".to_string()]);
        assert_eq!(status, 127);
        assert!(stderr.contains("not a child"));
    }

    #[test]
    fn wait_with_a_non_numeric_argument_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Wait, &mut shell, &["not-a-pid".to_string()]);
        assert_eq!(status, 127);
        assert!(stderr.contains("must be process or job IDs"));
    }

    #[test]
    fn wait_with_no_operands_and_no_background_jobs_is_an_immediate_no_op() {
        let mut shell = Shell::new();
        let (status, _, _) = run(&Wait, &mut shell, &[]);
        assert_eq!(status, 0);
    }

    #[test]
    fn jobs_with_an_empty_table_prints_nothing() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&Jobs, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(stdout.is_empty());
    }

    #[test]
    fn trap_registers_a_command_and_lists_it() {
        let mut shell = Shell::new();
        let (status, _, _) = run(
            &Trap,
            &mut shell,
            &["echo caught".to_string(), "USR1".to_string()],
        );
        assert_eq!(status, 0);
        let (status, stdout, _) = run(&Trap, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(stdout.contains("echo caught"));
    }

    #[test]
    fn trap_dash_p_reports_nothing_for_an_untrapped_signal() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&Trap, &mut shell, &["-p".to_string(), "USR2".to_string()]);
        assert_eq!(status, 0);
        assert!(stdout.is_empty());
    }

    #[test]
    fn trap_dash_p_reports_a_registered_trap() {
        let mut shell = Shell::new();
        run(
            &Trap,
            &mut shell,
            &["echo hi".to_string(), "USR2".to_string()],
        );
        let (status, stdout, _) = run(&Trap, &mut shell, &["-p".to_string(), "USR2".to_string()]);
        assert_eq!(status, 0);
        assert!(stdout.contains("echo hi"));
    }

    #[test]
    fn trap_with_an_unrecognized_signal_errors() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(
            &Trap,
            &mut shell,
            &["echo hi".to_string(), "NOTASIGNAL".to_string()],
        );
        assert_eq!(status, 1);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn trap_exit_registers_the_exit_trap() {
        let mut shell = Shell::new();
        run(
            &Trap,
            &mut shell,
            &["echo bye".to_string(), "EXIT".to_string()],
        );
        assert_eq!(shell.exit_trap(), Some("echo bye"));
    }
}
