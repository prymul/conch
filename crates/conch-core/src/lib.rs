//! Shell state, word expansion, and the command executor.

mod brace;
mod exec;
mod expand;

pub use brace::brace_expand;
pub use exec::{exec_command_list, exec_program};
pub use expand::{ExpandError, expand_word_fields, expand_word_single};

/// A `break [n]`/`continue [n]` (POSIX special builtins) in progress,
/// working its way back up through the executor to whichever enclosing
/// loop should catch it.
///
/// Builtins only ever get to communicate through their `i32` exit status
/// and whatever they write to `stdout`/`stderr` (see [`Builtin::run`]) —
/// there's no separate "and also do this control-flow thing" channel in
/// that trait, matching every other POSIX special builtin. So `break`/
/// `continue` (`conch-shell-builtins`) instead set
/// [`Shell::pending_control_flow`], and every loop/list-execution site in
/// `conch-shell-core`'s executor checks it after each command — see that
/// crate's `exec.rs` for the full propagation mechanism (each loop level
/// a signal passes through decrements its `n` by one, stopping there once
/// `n` reaches `1`).
///
/// Deliberately *not* a two-variant enum hardcoded to just these two
/// cases: a subshell-scoped `exit [n]` was an earlier candidate third
/// variant here, but doesn't belong — POSIX requires real fork semantics
/// for a subshell (`cd`/`exit`/every variable assignment inside must
/// never affect the parent), and `conch-shell-core::exec` gets that for
/// free by running a subshell as a genuine child process rather than
/// in-process (see [`crate::exec::exec_subshell`]'s docs), so `exit`
/// inside one already only terminates that child — no in-process signal
/// needed at all. The deferred-functions follow-up's `return [n]` *will*
/// need exactly this shape, though: a function call is not a process
/// boundary, so unwinding out of one has to happen through this same
/// kind of pending, decrementing-per-level signal. This type is kept as
/// its own enum (not e.g. folded into a bool/two separate `Option`
/// fields) specifically so adding a `Return(i32)` variant later is a
/// small, natural extension rather than a redesign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    /// `break n` — stop the loop `n` levels up entirely (`n == 1` for
    /// the innermost enclosing loop).
    Break(u32),
    /// `continue n` — skip straight to the next iteration of the loop
    /// `n` levels up.
    Continue(u32),
}

use std::collections::HashMap;
use std::env;
use std::io::Write;
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
    /// Whether this session is an interactive REPL rather than a `-c`
    /// string or script file. Defaults to `false`
    /// ([`Shell::new`]/[`Shell::default`]); the `conch` binary's
    /// interactive-mode entry point sets it explicitly. Affects only
    /// whether a POSIX-mandated *fatal* expansion error (`${var:?word}`
    /// on an unset parameter) exits the whole process or just reports a
    /// failure and moves on — see `conch-shell-core`'s `exec.rs`'s
    /// `report_expand_error` for the full rationale.
    pub is_interactive: bool,
    /// How many `while`/`until`/`for` loops currently enclose whatever's
    /// executing right now — incremented/decremented by the executor
    /// around each loop's body. A subshell runs as a genuinely separate
    /// process (see [`crate::exec::exec_subshell`]'s docs) with its own
    /// fresh `Shell`, so — unlike an earlier in-process design this
    /// field went through — nothing here needs to reset or isolate this
    /// counter at a subshell boundary; there's no shared state to
    /// isolate it *from*. The `break`/`continue` builtins
    /// (`conch-shell-builtins`) check this before setting
    /// [`Self::pending_control_flow`] at all, matching real bash
    /// printing "only meaningful in a `for', `while', or `until' loop"
    /// and otherwise doing nothing when used outside any loop.
    pub loop_depth: u32,
    /// See [`ControlFlow`]'s docs.
    pub pending_control_flow: Option<ControlFlow>,
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
            is_interactive: false,
            loop_depth: 0,
            pending_control_flow: None,
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

    /// Removes and returns the builtin registered under `name`, if any.
    ///
    /// Exists so callers can run a builtin with a `&mut Shell` in hand
    /// without a self-referential borrow: take it out, call it, put it
    /// back with [`Shell::register_builtin`].
    pub fn take_builtin(&mut self, name: &str) -> Option<Box<dyn Builtin>> {
        self.builtins.remove(name)
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
///
/// `stdout`/`stderr` are passed explicitly (rather than the builtin
/// writing to the real process streams directly) so that redirecting a
/// builtin's output (`echo hi > file`) works the same way it does for an
/// external command, and so builtins are testable against an in-memory
/// buffer instead of asserting on real stdout.
pub trait Builtin {
    /// Runs the builtin with the given arguments (not including the
    /// builtin's own name) and returns its exit status.
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysZero;
    impl Builtin for AlwaysZero {
        fn run(
            &self,
            _shell: &mut Shell,
            _args: &[String],
            _stdout: &mut dyn Write,
            _stderr: &mut dyn Write,
        ) -> i32 {
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
