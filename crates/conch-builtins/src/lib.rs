//! Builtin commands: `cd`, `exit`, `export`, `echo`, `pwd` (Phase 1); the
//! `break`/`continue` POSIX special builtins (Phase 3); and `local`,
//! `return`, a narrow `set --`, and `shift` (the functions/positional-
//! parameters follow-up).

use std::io::Write;

use conch_shell_core::{Builtin, ControlFlow, Shell};

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
}
