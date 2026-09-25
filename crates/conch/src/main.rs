use std::process::ExitCode;

use conch_shell_core::{Shell, exec_program};
use conch_shell_parser::parse;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

fn main() -> ExitCode {
    let mut shell = Shell::new();
    conch_shell_builtins::register_all(&mut shell);
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
    match parse(source) {
        Ok(list) => exec_program(&list, shell),
        Err(err) => {
            eprintln!("conch: {err}");
            2
        }
    }
}

fn run_interactive(shell: &mut Shell) -> i32 {
    // A POSIX-mandated *fatal* expansion error (`${var:?word}` on an
    // unset parameter) exits the whole process in non-interactive mode
    // but only returns to the prompt here — see `conch-shell-core`'s
    // `report_expand_error` for the full rationale.
    shell.is_interactive = true;

    let mut editor = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(err) => {
            eprintln!("conch: failed to start line editor: {err}");
            return 1;
        }
    };

    loop {
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

    shell.last_status
}
