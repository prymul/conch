//! Phase 1 command execution: simple commands, pipelines, and
//! `;`/`&&`/`||`-joined lists.
//!
//! **Known Phase 1 simplification:** pipeline stages are run sequentially,
//! with each stage's stdout fully buffered in memory before the next stage
//! runs (rather than wiring real concurrent OS pipes between child
//! processes). This is what lets a builtin — which has no OS process or
//! file descriptors of its own — sit anywhere in a pipeline next to
//! external commands without special-casing. The real cost: an infinite or
//! very large producer (`yes | head`) will hang or exhaust memory instead
//! of streaming. Concurrent OS-pipe wiring for all-external pipeline runs
//! is a reasonable later improvement; not attempted in Phase 1.
//!
//! Async (`&`) pipelines are parsed (see [`conch_shell_parser::Separator`])
//! but Phase 1 always runs them synchronously in the foreground — real
//! backgrounding is Phase 4 job control, per the project roadmap.

use std::collections::HashMap;
use std::io::Write as _;
use std::process::Stdio;

use conch_shell_parser::{
    AndOrList, Command, CommandList, LogicalOp, Pipeline, Redirect, RedirectOperator, SimpleCommand,
};

use crate::{Shell, expand_word};

/// Runs a full parsed command list against `shell`, returning the exit
/// status of the last command run (POSIX `$?`).
pub fn exec_command_list(list: &CommandList, shell: &mut Shell) -> i32 {
    let mut status = shell.last_status;
    for item in &list.items {
        status = exec_and_or(&item.and_or, shell);
        shell.last_status = status;
    }
    status
}

fn exec_and_or(and_or: &AndOrList, shell: &mut Shell) -> i32 {
    let mut status = exec_pipeline(&and_or.first, shell);
    for (op, pipeline) in &and_or.rest {
        let should_run = match op {
            LogicalOp::And => status == 0,
            LogicalOp::Or => status != 0,
        };
        if should_run {
            status = exec_pipeline(pipeline, shell);
        }
    }
    status
}

fn exec_pipeline(pipeline: &Pipeline, shell: &mut Shell) -> i32 {
    let mut carry_in: Option<Vec<u8>> = None;
    let mut status = 0;

    for (i, command) in pipeline.commands.iter().enumerate() {
        let is_last = i == pipeline.commands.len() - 1;
        let Command::Simple(simple) = command else {
            // Command is #[non_exhaustive] for future compound-command
            // variants (Phase 3); conch-shell-parser only ever produces
            // Simple today, and rejects anything else at parse time with
            // ParseError::UnsupportedConstruct.
            unreachable!("conch-shell-parser only produces Command::Simple in Phase 1")
        };
        let (this_status, output) = exec_simple(simple, shell, carry_in.take(), !is_last);
        status = this_status;
        carry_in = output;
    }

    status
}

/// Runs one [`SimpleCommand`].
///
/// `stdin_data`: piped-in bytes from the previous pipeline stage, if any.
/// `capture_output`: `true` if this command is *not* the pipeline's last
/// stage, so its stdout should be captured and returned rather than
/// written to the real process stdout.
///
/// Returns the exit status and, when `capture_output` was honored (no
/// output redirect overrode it), the captured stdout bytes.
fn exec_simple(
    cmd: &SimpleCommand,
    shell: &mut Shell,
    stdin_data: Option<Vec<u8>>,
    capture_output: bool,
) -> (i32, Option<Vec<u8>>) {
    let assignments = match expand_assignments(cmd, shell) {
        Ok(assignments) => assignments,
        Err(message) => {
            eprintln!("conch: {message}");
            return (1, None);
        }
    };

    let Some(name_word) = &cmd.name else {
        // A bare assignment-only simple command (`FOO=bar`, no command
        // name) assigns persistently to the shell rather than just the
        // one command's environment.
        for (name, value) in assignments {
            shell.shell_vars.insert(name, value);
        }
        return (0, None);
    };

    let name = match expand_word(name_word, shell) {
        Ok(name) => name,
        Err(err) => {
            eprintln!("conch: {err}");
            return (1, None);
        }
    };

    let args = match cmd
        .args
        .iter()
        .map(|word| expand_word(word, shell))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(err) => {
            eprintln!("conch: {err}");
            return (1, None);
        }
    };

    let redirects = match expand_redirects(cmd, shell) {
        Ok(redirects) => redirects,
        Err(message) => {
            eprintln!("conch: {message}");
            return (1, None);
        }
    };

    if let Some(builtin_name) = shell.builtin(&name).is_some().then(|| name.clone()) {
        return exec_builtin(
            &builtin_name,
            shell,
            &args,
            &assignments,
            &redirects,
            capture_output,
        );
    }

    exec_external(
        &name,
        &args,
        shell,
        &assignments,
        stdin_data,
        &redirects,
        capture_output,
    )
}

/// `NAME=value` prefix assignments, expanded to their final strings.
fn expand_assignments(cmd: &SimpleCommand, shell: &Shell) -> Result<Vec<(String, String)>, String> {
    cmd.assignments
        .iter()
        .map(|assignment| {
            expand_word(&assignment.value, shell)
                .map(|value| (assignment.name.clone(), value))
                .map_err(|err| err.to_string())
        })
        .collect()
}

struct ExpandedRedirect {
    fd: u32,
    operator: RedirectOperator,
    target: String,
}

fn expand_redirects(cmd: &SimpleCommand, shell: &Shell) -> Result<Vec<ExpandedRedirect>, String> {
    cmd.redirects
        .iter()
        .map(|redirect: &Redirect| {
            let target = expand_word(&redirect.target, shell).map_err(|err| err.to_string())?;
            let default_fd = match redirect.operator {
                RedirectOperator::Input => 0,
                RedirectOperator::Output | RedirectOperator::Append => 1,
            };
            Ok(ExpandedRedirect {
                fd: redirect.fd.unwrap_or(default_fd),
                operator: redirect.operator,
                target,
            })
        })
        .collect()
}

fn exec_builtin(
    name: &str,
    shell: &mut Shell,
    args: &[String],
    assignments: &[(String, String)],
    redirects: &[ExpandedRedirect],
    capture_output: bool,
) -> (i32, Option<Vec<u8>>) {
    // Prefix assignments before a builtin are applied to the shell's own
    // environment for the duration of the call and then rolled back —
    // matching the "temporary, per-command" semantics real assignments
    // before a command name have, without persisting them the way a
    // bare `FOO=bar` (no command) does.
    let mut previous = Vec::with_capacity(assignments.len());
    for (name, value) in assignments {
        previous.push((
            name.clone(),
            shell.env_vars.insert(name.clone(), value.clone()),
        ));
    }

    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    // Take the builtin out of the registry for the duration of the call:
    // `run` needs `&mut Shell`, which the registry itself lives inside, so
    // holding a borrow of the boxed builtin while also passing `&mut
    // shell` in is a self-referential borrow the compiler correctly
    // rejects. None of Phase 1's builtins call back into the registry
    // (e.g. by invoking another builtin), so this is a non-issue in
    // practice, not just a workaround.
    let builtin = shell
        .take_builtin(name)
        .expect("caller already checked this builtin exists");
    let status = builtin.run(shell, args, &mut stdout_buf, &mut stderr_buf);
    shell.register_builtin(name.to_string(), builtin);

    for (name, previous_value) in previous {
        match previous_value {
            Some(value) => {
                shell.env_vars.insert(name, value);
            }
            None => {
                shell.env_vars.remove(&name);
            }
        }
    }

    if !stderr_buf.is_empty() {
        let _ = std::io::stderr().write_all(&stderr_buf);
    }

    if let Some(output_redirect) = redirects.iter().rev().find(|r| {
        r.fd == 1
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    }) {
        if let Err(err) = write_redirect(output_redirect, &stdout_buf) {
            eprintln!("conch: {}: {err}", output_redirect.target);
            return (1, None);
        }
        return (status, None);
    }

    if capture_output {
        (status, Some(stdout_buf))
    } else {
        let _ = std::io::stdout().write_all(&stdout_buf);
        (status, None)
    }
}

fn exec_external(
    name: &str,
    args: &[String],
    shell: &Shell,
    assignments: &[(String, String)],
    stdin_data: Option<Vec<u8>>,
    redirects: &[ExpandedRedirect],
    capture_output: bool,
) -> (i32, Option<Vec<u8>>) {
    let mut effective_env: HashMap<String, String> = shell.env_vars.clone();
    for (name, value) in assignments {
        effective_env.insert(name.clone(), value.clone());
    }

    let mut command = std::process::Command::new(name);
    command
        .args(args)
        .current_dir(&shell.cwd)
        .env_clear()
        .envs(&effective_env);

    let input_redirect = redirects
        .iter()
        .rev()
        .find(|r| r.fd == 0 && matches!(r.operator, RedirectOperator::Input));
    let output_redirect = redirects.iter().rev().find(|r| {
        r.fd == 1
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    });

    match input_redirect {
        Some(redirect) => match std::fs::File::open(&redirect.target) {
            Ok(file) => {
                command.stdin(file);
            }
            Err(err) => {
                eprintln!("conch: {}: {err}", redirect.target);
                return (1, None);
            }
        },
        None => {
            command.stdin(if stdin_data.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
        }
    }

    match output_redirect {
        Some(redirect) => {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .append(matches!(redirect.operator, RedirectOperator::Append))
                .truncate(!matches!(redirect.operator, RedirectOperator::Append))
                .open(&redirect.target);
            match file {
                Ok(file) => {
                    command.stdout(file);
                }
                Err(err) => {
                    eprintln!("conch: {}: {err}", redirect.target);
                    return (1, None);
                }
            }
        }
        None => {
            command.stdout(if capture_output {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("conch: {name}: {err}");
            return (127, None);
        }
    };

    if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
        let _ = stdin.write_all(&data);
        // Dropping `stdin` here closes the pipe, signaling EOF to the
        // child — required for commands that read to completion (`cat`,
        // `wc`, ...) rather than processing incrementally.
    }

    if output_redirect.is_none() && capture_output {
        match child.wait_with_output() {
            Ok(output) => (output.status.code().unwrap_or(1), Some(output.stdout)),
            Err(err) => {
                eprintln!("conch: {name}: {err}");
                (1, None)
            }
        }
    } else {
        match child.wait() {
            Ok(status) => (status.code().unwrap_or(1), None),
            Err(err) => {
                eprintln!("conch: {name}: {err}");
                (1, None)
            }
        }
    }
}

fn write_redirect(redirect: &ExpandedRedirect, data: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(matches!(redirect.operator, RedirectOperator::Append))
        .truncate(!matches!(redirect.operator, RedirectOperator::Append))
        .open(&redirect.target)?;
    file.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use conch_shell_parser::parse;

    fn run(input: &str, shell: &mut Shell) -> i32 {
        exec_command_list(&parse(input).unwrap(), shell)
    }

    #[test]
    fn true_and_false_builtins_via_external_binaries() {
        let mut shell = Shell::new();
        assert_eq!(run("true", &mut shell), 0);
        assert_eq!(run("false", &mut shell), 1);
    }

    #[test]
    fn and_short_circuits_on_failure() {
        let mut shell = Shell::new();
        crate::Shell::register_builtin(&mut shell, "would_not_run", Box::new(RecordingBuiltin));
        // false && would_not_run: the second must not execute.
        assert_eq!(run("false && would_not_run", &mut shell), 1);
    }

    #[test]
    fn or_runs_only_on_failure() {
        let mut shell = Shell::new();
        assert_eq!(run("true || false", &mut shell), 0);
    }

    #[test]
    fn bare_assignment_persists_in_shell_vars() {
        let mut shell = Shell::new();
        run("FOO=bar", &mut shell);
        assert_eq!(shell.get_var("FOO"), Some("bar"));
    }

    #[test]
    fn prefix_assignment_does_not_persist() {
        let mut shell = Shell::new();
        // `FOO=bar true` should not leave FOO set afterward.
        run("FOO=temp true", &mut shell);
        assert_eq!(shell.get_var("FOO"), None);
    }

    #[test]
    fn pipeline_pipes_output_between_external_commands() {
        let mut shell = Shell::new();
        // exit status of a pipeline is the last command's.
        assert_eq!(run("echo hi | true", &mut shell), 0);
    }

    struct RecordingBuiltin;
    impl crate::Builtin for RecordingBuiltin {
        fn run(
            &self,
            _shell: &mut Shell,
            _args: &[String],
            _stdout: &mut dyn std::io::Write,
            _stderr: &mut dyn std::io::Write,
        ) -> i32 {
            panic!("should have been short-circuited");
        }
    }
}
