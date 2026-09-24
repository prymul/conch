//! Builtin commands: `cd`, `exit`, `export`, `echo`, `pwd` (Phase 1), and
//! the `break`/`continue` POSIX special builtins (Phase 3).

use std::io::Write;

use conch_shell_core::{Builtin, ControlFlow, Shell};

/// Registers every builtin this crate implements into `shell`.
pub fn register_all(shell: &mut Shell) {
    shell.register_builtin("cd", Box::new(Cd));
    shell.register_builtin("exit", Box::new(Exit));
    shell.register_builtin("export", Box::new(Export));
    shell.register_builtin("echo", Box::new(Echo));
    shell.register_builtin("pwd", Box::new(Pwd));
    shell.register_builtin("break", Box::new(Break));
    shell.register_builtin("continue", Box::new(Continue));
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
        for name in ["cd", "exit", "export", "echo", "pwd", "break", "continue"] {
            assert!(shell.builtin(name).is_some(), "missing builtin: {name}");
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
}
