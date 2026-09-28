mod completion;
mod helper;
mod highlight;
mod history;
mod quoting;

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;

use conch_shell_core::{CompletionState, JobState, Shell, exec_program};
use conch_shell_parser::{parse, parse_with_aliases};
use helper::ConchHelper;
use rustyline::error::ReadlineError;
use rustyline::history::{DefaultHistory, History as _};
use rustyline::{CompletionType, Config, Editor};

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

/// Builds this session's own `rustyline::Config` — see each option's own
/// inline comment for the specific real-bash behavior it's matching
/// (and, for `history_ignore_dups`, the one place rustyline's own
/// default would otherwise silently diverge from bash's).
fn build_editor_config(shell: &Shell) -> Config {
    Config::builder()
        // Real bash/readline UX: complete to the longest common prefix
        // and list ambiguous matches on a second Tab. Confirmed by
        // reading rustyline 18.0.1's own `complete_line`:
        // `CompletionType::List` is what implements exactly that;
        // the crate's own *default*, `Circular`, instead cycles through
        // candidates one at a time on repeated Tab presses (closer to
        // zsh/emacs `M-/` than to bash's own default Tab behavior) and
        // would be a real, visible UX divergence from bash if left
        // alone.
        .completion_type(CompletionType::List)
        // Real bash's own default (`$HISTCONTROL` unset): every line is
        // saved to history, including immediate duplicates. rustyline's
        // own default is the opposite (`IgnoreConsecutive`), which would
        // silently diverge from bash if left alone. `$HISTCONTROL` isn't
        // implemented this phase at all (not scoped) — this is simply
        // bash's own unconfigured default, not a partial implementation
        // of the variable.
        .history_ignore_dups(false)
        .expect("a bool is always a valid history_ignore_dups setting")
        // `$HISTSIZE` — see `conch_shell_core::history_size`'s own docs
        // for why "unset" means effectively unbounded here, matching
        // bash, not rustyline's own unrelated built-in default of 100.
        .max_history_size(conch_shell_core::history_size(shell))
        .expect("conch_shell_core::history_size() never returns a value rustyline itself rejects")
        .build()
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

    // Interactive-only, single-file startup sourcing — see
    // `conch_shell_core::source_startup_file`'s own docs for exactly
    // which of real bash's several startup files this collapses into
    // one, why, and the `Shell::is_interactive` gate it enforces itself
    // (rather than trusting every call site to remember to check first).
    // Deliberately runs *after* job control's own terminal-ownership
    // setup above (matching bash's own ordering: an interactive shell
    // claims the terminal before it starts running any of the user's own
    // startup commands, which may themselves spawn foreground jobs) but
    // *before* the line editor itself is constructed below, so
    // `~/.conchrc` can freely set `$PS1`/`$HISTFILE`/aliases/functions/
    // etc. and have every one of them already in effect for the very
    // first prompt.
    conch_shell_core::source_startup_file(shell);

    let history_path = conch_shell_core::history_file_path(shell);
    let config = build_editor_config(shell);

    let mut editor: Editor<ConchHelper, DefaultHistory> = match Editor::with_config(config) {
        Ok(editor) => editor,
        Err(err) => {
            eprintln!("conch: failed to start line editor: {err}");
            return 1;
        }
    };

    let completion_state = Rc::new(RefCell::new(CompletionState::default()));
    editor.set_helper(Some(ConchHelper::new(Rc::clone(&completion_state))));

    if let Some(path) = &history_path {
        history::load_history(&mut editor, path);
    }

    loop {
        report_finished_jobs(shell);
        // See `conch_shell_core::CompletionState`'s own docs for why this
        // is a once-per-prompt snapshot refresh rather than a live read
        // from inside `Completer::complete` itself.
        completion_state.borrow_mut().refresh(shell);

        let ps1_template = conch_shell_core::ps1_template(shell);
        let history_len = editor.history().len();
        let ps1 = conch_shell_core::expand_prompt(shell, &ps1_template, history_len);

        let mut buffer = match editor.readline(&ps1) {
            Ok(line) => line,
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
        };

        if buffer.trim().is_empty() {
            continue;
        }

        // Multi-line continuation (`PS2`) — see `helper`'s own module
        // docs for why this is a hand-rolled loop rather than rustyline's
        // own `Validator`/`ValidationResult::Incomplete` mechanism.
        // Keeps reading and appending lines under `PS2` for as long as
        // `conch_shell_parser::ParseError::is_incomplete_input` reports
        // "ran out of input mid-construct," rather than a duplicate ad
        // hoc "is this incomplete" check — see that method's own docs.
        let list = 'continuation: loop {
            match parse_with_aliases(&buffer, &shell.aliases) {
                Ok(list) => break 'continuation Some(list),
                Err(err) if err.is_incomplete_input() => {
                    let ps2_template = conch_shell_core::ps2_template(shell);
                    let history_len = editor.history().len();
                    let ps2 = conch_shell_core::expand_prompt(shell, &ps2_template, history_len);
                    match editor.readline(&ps2) {
                        Ok(next_line) => {
                            buffer.push('\n');
                            buffer.push_str(&next_line);
                        }
                        Err(ReadlineError::Interrupted) => {
                            // Ctrl-C mid-continuation: abandon the whole
                            // partially-typed command, same as Ctrl-C at
                            // the primary prompt abandons a single line.
                            break 'continuation None;
                        }
                        Err(ReadlineError::Eof) => {
                            // Ctrl-D (or real EOF) while still inside an
                            // unterminated construct — confirmed against
                            // real bash: this reports the same "ran out
                            // of input" error the construct's own
                            // incompleteness already describes and
                            // returns to the primary prompt, rather than
                            // exiting the shell outright (a *second*,
                            // genuine EOF at the now-fresh primary prompt
                            // is what actually ends the session, on the
                            // very next loop iteration).
                            eprintln!("conch: {err}");
                            break 'continuation None;
                        }
                        Err(err) => {
                            eprintln!("conch: {err}");
                            break 'continuation None;
                        }
                    }
                }
                Err(err) => {
                    eprintln!("conch: {err}");
                    break 'continuation None;
                }
            }
        };

        let Some(list) = list else {
            continue;
        };

        // Eager history persistence — see `history`'s own module docs
        // for why this happens right here, *before* `exec_program` runs,
        // rather than once at this loop's own end: `exit` and
        // `exec <builtin-name>` both terminate the process from inside
        // `exec_program` unconditionally, which would bypass a deferred
        // save entirely.
        let _ = editor.add_history_entry(buffer.as_str());
        if let Some(path) = &history_path {
            history::append_history(&mut editor, path);
        }

        shell.last_status = exec_program(&list, shell);
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
