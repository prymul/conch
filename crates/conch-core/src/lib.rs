//! Shell state and the builtin-command registry.
//!
//! The executor (spawning external commands, wiring pipelines and
//! redirection) is added once `conch-parser`'s AST lands; this module
//! defines the pieces that don't depend on it.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

/// Mutable state for one shell session.
pub struct Shell {
    /// The shell's current working directory.
    pub cwd: PathBuf,
    /// Exported environment variables, visible to spawned child processes.
    pub env_vars: HashMap<String, String>,
    /// Shell-only variables (set but not exported).
    pub shell_vars: HashMap<String, String>,
    /// The exit status of the most recently run command (`$?`).
    pub last_status: i32,
    builtins: HashMap<String, Box<dyn Builtin>>,
}

impl Shell {
    /// Creates a new shell session, inheriting the current process's
    /// working directory and environment.
    pub fn new() -> Self {
        Self {
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env_vars: env::vars().collect(),
            shell_vars: HashMap::new(),
            last_status: 0,
            builtins: HashMap::new(),
        }
    }

    /// Looks up a variable, preferring exported environment variables and
    /// falling back to shell-only variables.
    pub fn get_var(&self, name: &str) -> Option<&str> {
        self.env_vars
            .get(name)
            .or_else(|| self.shell_vars.get(name))
            .map(String::as_str)
    }

    /// Registers a builtin under `name`, replacing any existing builtin
    /// with the same name.
    pub fn register_builtin(&mut self, name: impl Into<String>, builtin: Box<dyn Builtin>) {
        self.builtins.insert(name.into(), builtin);
    }

    /// Returns the builtin registered under `name`, if any.
    pub fn builtin(&self, name: &str) -> Option<&dyn Builtin> {
        self.builtins.get(name).map(AsRef::as_ref)
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// A command conch runs in-process instead of spawning an external
/// program — `cd`, `exit`, `export`, and similar commands that must
/// mutate the shell's own state.
pub trait Builtin {
    /// Runs the builtin with the given arguments (not including the
    /// builtin's own name) and returns its exit status.
    fn run(&self, shell: &mut Shell, args: &[String]) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysZero;
    impl Builtin for AlwaysZero {
        fn run(&self, _shell: &mut Shell, _args: &[String]) -> i32 {
            0
        }
    }

    #[test]
    fn register_and_look_up_builtin() {
        let mut shell = Shell::new();
        shell.register_builtin("noop", Box::new(AlwaysZero));
        assert!(shell.builtin("noop").is_some());
        assert!(shell.builtin("missing").is_none());
    }

    #[test]
    fn env_var_takes_precedence_over_shell_var() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("FOO".to_string(), "shell".to_string());
        shell.env_vars.insert("FOO".to_string(), "env".to_string());
        assert_eq!(shell.get_var("FOO"), Some("env"));
    }
}
