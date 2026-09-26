use std::process::ExitCode;

use conch_shell_core::{JobState, Shell, exec_program};
use conch_shell_parser::parse;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

fn main() -> ExitCode {
    let mut shell = Shell::new();
    conch_shell_builtins::register_all(&mut shell);
    // SIGCHLD reaping and deferred `trap` dispatch, needed uniformly
    // whether this ends up being `-c`, a script file, or interactive
    // (POSIX: background jobs and `trap` are both meaningful in a
    // non-interactive script too) — see `Shell::init_signal_handling`'s
    // own docs. Terminal ownership / process-group claiming
    // (`Shell::init_job_control`) is a *separate*, interactive-only step
    // — see `run_interactive`.
    shell.init_signal_handling();
    // `$0` defaults to this process's own argv[0] (confirmed against real
    // bash: `-c`/interactive mode uses the shell's own invocation name) —
    // both branches below may still override it, per POSIX/bash's own
    // `-c`/script-file conventions.
    if let Some(argv0) = std::env::args().next() {
        shell.arg0 = argv0;
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    let status = match args.first().map(String::as_str) {
        Some("-c") => {
            let Some(command) = args.get(1) else {
                eprintln!("conch: -c requires a command string");
                return ExitCode::from(2);
            };
            // POSIX/bash: `-c command_string [command_name [argument...]]`
            // — confirmed against real bash an optional argument right
            // after `command_string` overrides `$0`, and everything past
            // *that* becomes the initial positional parameters
            // (`$1`, `$2`, ...).
            if let Some(name) = args.get(2) {
                shell.arg0 = name.clone();
            }
            shell.positional_params = args.get(3..).map(<[String]>::to_vec).unwrap_or_default();
            run_source(command, &mut shell)
        }
        Some(script_path) => {
            // `$0` is the script path exactly as given on the command
            // line, regardless of the script's own arguments (confirmed
            // against real bash); everything after it is the initial
            // positional parameters.
            shell.arg0 = script_path.to_string();
            shell.positional_params = args.get(1..).map(<[String]>::to_vec).unwrap_or_default();
            match std::fs::read_to_string(script_path) {
                Ok(source) => run_source(&source, &mut shell),
                Err(err) => {
                    eprintln!("conch: {script_path}: {err}");
                    127
                }
            }
        }
        None => run_interactive(&mut shell),
    };

    ExitCode::from(status.rem_euclid(256) as u8)
}

/// Parses and runs `source` as one program (a whole `-c` string or script
/// file), returning its exit status. A parse error is reported and treated
/// as exit status 2, matching the convention real shells use for a syntax
/// error.
fn run_source(source: &str, shell: &mut Shell) -> i32 {
    let status = match parse(source) {
        Ok(list) => exec_program(&list, shell),
        Err(err) => {
            eprintln!("conch: {err}");
            2
        }
    };
    // POSIX: "the trap on EXIT shall be executed... prior to the shell
    // terminating" — for a `-c`/script invocation, that's right here,
    // once the whole program has finished, whether or not it ever
    // called `exit` explicitly itself (which also calls this — see its
    // own docs for why that's a safe, non-recursive no-op by the time
    // control would ever reach back here after it did).
    shell.run_exit_trap();
    status
}

fn run_interactive(shell: &mut Shell) -> i32 {
    // A POSIX-mandated *fatal* expansion error (`${var:?word}` on an
    // unset parameter) exits the whole process in non-interactive mode
    // but only returns to the prompt here — see `conch-shell-core`'s
    // `report_expand_error` for the full rationale.
    shell.is_interactive = true;

    // Claims the controlling terminal and this session's own process
    // group (see `Shell::init_job_control`'s own docs for the full GNU
    // libc manual "Initializing the Shell" sequence this runs) — called
    // *before* constructing the line editor below so job control's own
    // terminal-ownership state is already correct by the time rustyline
    // starts touching the terminal's mode (a separate, orthogonal
    // concern — see this crate's own notes on why the two don't
    // conflict: `tcsetpgrp` governs the terminal's *foreground process
    // group*, `tcsetattr`/`tcgetattr` govern its *mode* (raw/canonical/
    // echo/...), and rustyline's own `readline()` re-reads and
    // re-applies its desired mode fresh on *every* call — confirmed by
    // reading rustyline 18.0.1's own `enable_raw_mode` (`tty/unix.rs`),
    // not assumed — so it's already robust to whatever a foreground job
    // (e.g. `vim`) left the terminal's mode in when it exited, without
    // this crate needing to do anything extra around that handoff).
    // Silently leaves `Shell::job_control_active` `false` if there's no
    // real controlling terminal (e.g. under a test harness) rather than
    // erroring, matching real bash's own job-control-disabled fallback.
    let _ = shell.init_job_control();

    let mut editor = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(err) => {
            eprintln!("conch: failed to start line editor: {err}");
            return 1;
        }
    };

    loop {
        report_finished_jobs(shell);
        let prompt = format!("conch {} $ ", shell.cwd.display());
        match editor.readline(&prompt) {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line.as_str());
                match parse(&line) {
                    Ok(list) => {
                        shell.last_status = exec_program(&list, shell);
                    }
                    Err(err) => eprintln!("conch: {err}"),
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C: real shells abandon the current line and reprompt.
                continue;
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D on an empty line: exit, matching bash.
                break;
            }
            Err(err) => {
                eprintln!("conch: {err}");
                break;
            }
        }
    }

    // See `run_source`'s own doc comment — same POSIX EXIT-trap
    // requirement, just at the interactive session's own end instead.
    shell.run_exit_trap();
    shell.last_status
}

/// Prints bash's own `[1]+  Done                   sleep 5`-style
/// notification for every background/stopped job that changed state
/// since the last time this ran and hasn't been reported yet (`jobs`
/// itself is the other place that counts as "reported" — see
/// [`Shell::purge_finished_notified_jobs`]'s own docs), then forgets any
/// job that's now been reported once. Deliberately says nothing about a
/// job that's merely still [`JobState::Running`] with no state change —
/// that was already announced once, at the moment it was backgrounded
/// (`[1] 12345`, `conch-shell-core::exec::spawn_background_job`); only a
/// transition (`Stopped`, or finished) is news.
fn report_finished_jobs(shell: &mut Shell) {
    // The one call site outside `conch-shell-core::exec`'s own
    // checkpoints that drives the deferred-signal-processing / reaping
    // machinery — see `Shell::process_pending_signals`' own docs for why
    // this (a safe point in ordinary control flow, not from inside a
    // signal handler) is exactly where that belongs.
    shell.process_pending_signals();

    for job in shell.job_table.iter() {
        if !job.notified && !matches!(job.state, JobState::Running) {
            eprintln!("[{}]+  {:<24}{}", job.id, job.state.label(), job.command);
        }
    }
    let ids: Vec<u32> = shell.job_table.iter().map(|job| job.id).collect();
    for id in ids {
        if let Some(job) = shell.job_table.get_mut(id) {
            job.notified = true;
        }
    }
    shell.purge_finished_notified_jobs();
}
