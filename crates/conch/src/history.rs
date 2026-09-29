//! Persistent history's rustyline-`Editor`-specific glue — load once at
//! startup, append eagerly after each command. `$HISTFILE`/`$HISTSIZE`
//! resolution itself is pure and lives in `conch_shell_core::history`
//! (`history_file_path`/`history_size`) so `tests/conch-difftest` (and
//! any other consumer) can call it without a `rustyline` dependency; this
//! module is only the part that's inherently tied to
//! `rustyline::Editor`/`Helper`/`FileHistory`, which `conch-shell-core`
//! deliberately has no dependency on.
//!
//! # Why eager `append_history`, not `load_history`-once /
//! `save_history`-once at the loop's normal exit
//!
//! The obvious design — load once at startup, save once when
//! `run_interactive`'s loop ends — is silently wrong here: `exit`
//! (`conch-shell-builtins::Exit::run`) and `exec <builtin-name>`
//! (`conch-shell-builtins::exec_builtin::Exec::run`) both call
//! `std::process::exit` **unconditionally**, bypassing `run_interactive`'s
//! own loop entirely — a session ended either way would never reach a
//! deferred `save_history` call. The fix: `main.rs`'s own loop calls
//! [`append_history`] *eagerly*, right after each command is added to
//! history — an incremental per-command append (mirroring bash's own
//! `history -a` idiom) that completes *before* `exec_program` runs (and
//! so before either bypass could ever fire), sidestepping the bypass
//! entirely rather than trying to catch every exit site by hand.

use std::path::Path;

use rustyline::Helper;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;

/// Loads history from `path` into `editor`, if it exists. A missing file
/// is silently skipped (matching bash's own missing-`$HISTFILE`
/// behavior, e.g. a brand new install with no history yet) rather than
/// reported; any other I/O error (permissions, corrupt file, ...) is
/// reported once to stderr and otherwise ignored — persistent history is
/// a nice-to-have, not core shell correctness, so a broken history file
/// must never prevent the shell from starting.
pub fn load_history<H: Helper>(editor: &mut rustyline::Editor<H, FileHistory>, path: &Path) {
    match editor.load_history(path) {
        Ok(()) => {}
        Err(ReadlineError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!(
                "conch: warning: couldn't load history from {}: {err}",
                path.display()
            );
        }
    }
}

/// Appends any new history entries to `path` — see this module's own
/// docs for why `main.rs` calls this eagerly, once per command, rather
/// than once at the end of the interactive loop. Best-effort: matches
/// bash's own `history -a` not treating a write failure (disk full,
/// permissions, `$HOME` on a read-only filesystem, ...) as fatal to the
/// running shell. Also, incidentally, the file this writes to always ends
/// up mode `0600` — confirmed (security review) that this is rustyline
/// 18.0.1's own `FileHistory` behavior unconditionally (a umask override
/// plus an `fchmod` on every write), *as long as* every write to the
/// history path goes through this API and never a hand-rolled file write
/// instead — which is the case here.
pub fn append_history<H: Helper>(editor: &mut rustyline::Editor<H, FileHistory>, path: &Path) {
    let _ = editor.append_history(path);
}
