use std::process::ExitCode;

use conch_shell_core::{Shell, exec_program};
use conch_shell_parser::parse;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

fn main() -> ExitCode {
    let mut shell = Shell::new();
    conch_shell_builtins::register_all(&mut shell);

    let args: Vec<String> = std::env::args().skip(1).collect();

    let status = match args.first().map(String::as_str) {
        Some("-c") => {
            let Some(command) = args.get(1) else {
                eprintln!("conch: -c requires a command string");
                return ExitCode::from(2);
            };
            run_source(command, &mut shell)
        }
        Some(script_path) => match std::fs::read_to_string(script_path) {
            Ok(source) => run_source(&source, &mut shell),
            Err(err) => {
                eprintln!("conch: {script_path}: {err}");
                127
            }
        },
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
