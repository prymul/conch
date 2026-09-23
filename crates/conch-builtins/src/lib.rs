//! Phase 1 builtin commands: `cd`, `exit`, `export`, `echo`, `pwd`.

use std::io::Write;

use conch_shell_core::{Builtin, Shell};

/// Registers every Phase 1 builtin into `shell`.
pub fn register_all(shell: &mut Shell) {
    shell.register_builtin("cd", Box::new(Cd));
    shell.register_builtin("exit", Box::new(Exit));
    shell.register_builtin("export", Box::new(Export));
    shell.register_builtin("echo", Box::new(Echo));
    shell.register_builtin("pwd", Box::new(Pwd));
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
    fn register_all_registers_every_phase_1_builtin() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        for name in ["cd", "exit", "export", "echo", "pwd"] {
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
}
