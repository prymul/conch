//! Tab completion's pure candidate-generation core — [`CompletionState`],
//! [`command_candidates`]. `conch`'s binary crate holds the thin
//! [`rustyline::completion::Completer`] adapter that actually wires this
//! into a keypress (see that crate's own `completion.rs`/`quoting.rs`);
//! this lives here so `tests/conch-difftest` can call it in-process
//! against a real bash oracle (`compgen -c`) without a pty — `conch`'s
//! binary crate has no `[lib]` target, so nothing outside it could ever
//! reach a function that stayed there.
//!
//! Two genuinely different completion behaviors, matching bash's own
//! `COMP_CWORD == 0` (command position) vs. `> 0` (argument position)
//! distinction (GNU Bash Reference Manual, "Programmable Completion") —
//! see [`crate::word_scan::is_command_position`]:
//! - **Command position**: shell functions, builtins, aliases, reserved
//!   words, and `$PATH` executables — the same lookup
//!   [`crate::resolve_command`] uses for real dispatch, but as an
//!   *enumeration* of every match rather than one resolution. This is
//!   what [`command_candidates`] (below) computes.
//! - **Argument position**: ordinary quote-aware filename completion —
//!   delegated wholesale, on the adapter side, to `rustyline`'s own
//!   `FilenameCompleter` rather than reimplemented — nothing to do with
//!   `Shell` state at all, so there's no pure core for it to live here.

use std::collections::BTreeSet;

use crate::Shell;

/// A point-in-time snapshot of whatever `Shell` state command-position
/// tab completion needs — refreshed once per prompt (right before each
/// `Editor::readline` call, in `conch`'s own `main.rs`), not read from a
/// live `&Shell` during completion itself.
///
/// # Why a snapshot, not a live `&Shell`
///
/// rustyline's `Completer::complete(&self, ...)` only ever gets `&self`
/// — there's no channel back into `main.rs`'s own `&mut Shell` while
/// `Editor::readline` has exclusive control of the terminal for the
/// whole duration of one line-editing session. Since `main.rs`'s own
/// loop is fully synchronous — nothing else ever mutates `Shell` while a
/// `readline()` call is in progress — refreshing this snapshot once per
/// prompt is always exactly as accurate as a live read would have been
/// for the entire editing session that follows.
#[derive(Default, Clone)]
pub struct CompletionState {
    /// [`Shell::functions`]' keys.
    pub functions: Vec<String>,
    /// [`Shell::aliases`]' keys.
    pub aliases: Vec<String>,
    /// [`Shell::builtin_names`]'s output.
    pub builtins: Vec<String>,
    /// `$PATH`, verbatim — `$PATH` executables are deliberately *not*
    /// precomputed into a name list here the way functions/builtins/
    /// aliases are: unlike those (a handful of entries, cheap to snapshot
    /// whole), enumerating `$PATH` needs a fresh directory scan per
    /// completion *prefix* (see [`crate::list_path_executables`]), so
    /// only the raw search path is worth snapshotting.
    pub path: String,
}

impl CompletionState {
    /// Replaces every field with a fresh read from `shell`. See this
    /// type's own docs for when `conch`'s `main.rs` calls this.
    pub fn refresh(&mut self, shell: &Shell) {
        self.functions = shell.functions.keys().cloned().collect();
        self.aliases = shell.aliases.keys().cloned().collect();
        self.builtins = shell.builtin_names().map(str::to_string).collect();
        self.path = shell.get_var("PATH").unwrap_or_default().to_string();
    }
}

/// Every command-position completion candidate whose name starts with
/// `prefix`, gathered from `state`'s functions, aliases, builtins,
/// reserved words, and `$PATH` executables — deduplicated (a name
/// defined more than one way, e.g. a function that shadows a builtin, is
/// only listed once) and returned in sorted order (a stable, predictable
/// completion-list order — matching how real bash's own completion lists
/// alternatives alphabetically too).
///
/// Confirmed against real bash's own `compgen -c` (the closest
/// non-interactive equivalent to "what would Tab offer at command
/// position" — see `tests/conch-difftest/src/completion_oracle.rs`'s own
/// module docs): it includes reserved words too, not just
/// builtins/functions/aliases/executables — a bare `wh<TAB>` really does
/// offer `while`, not just any command literally named `wh...`.
#[must_use]
pub fn command_candidates(state: &CompletionState, prefix: &str) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for group in [&state.functions, &state.aliases, &state.builtins] {
        names.extend(
            group
                .iter()
                .filter(|name| name.starts_with(prefix))
                .cloned(),
        );
    }
    names.extend(
        crate::word_scan::RESERVED_WORDS
            .iter()
            .filter(|word| word.starts_with(prefix))
            .map(|word| (*word).to_string()),
    );
    names.extend(crate::list_path_executables(&state.path, prefix));
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> CompletionState {
        CompletionState {
            functions: vec!["greet".to_string(), "grow".to_string()],
            aliases: vec!["gs".to_string()],
            builtins: vec!["cd".to_string(), "grep_builtin_stub".to_string()],
            path: String::new(),
        }
    }

    #[test]
    fn merges_and_filters_by_prefix() {
        let mut names = command_candidates(&state(), "gr");
        names.sort();
        assert_eq!(
            names,
            vec![
                "greet".to_string(),
                "grep_builtin_stub".to_string(),
                "grow".to_string()
            ]
        );
    }

    #[test]
    fn empty_prefix_matches_everything() {
        // functions (2) + aliases (1) + builtins (2) + every reserved
        // word, no overlap.
        assert_eq!(
            command_candidates(&state(), "").len(),
            5 + crate::word_scan::RESERVED_WORDS.len()
        );
    }

    #[test]
    fn reserved_words_are_offered_as_command_candidates() {
        // Confirmed against real bash's own `compgen -c`: a plain
        // `wh<TAB>` offers `while`, not just commands literally named
        // `wh...` -- see this function's own doc comment.
        assert_eq!(
            command_candidates(&state(), "wh"),
            vec!["while".to_string()]
        );
    }

    #[test]
    fn duplicate_across_sources_is_reported_once() {
        let state = CompletionState {
            functions: vec!["cd".to_string()],
            aliases: vec![],
            builtins: vec!["cd".to_string()],
            path: String::new(),
        };
        assert_eq!(command_candidates(&state, "cd"), vec!["cd".to_string()]);
    }

    #[test]
    fn no_match_is_empty() {
        assert!(command_candidates(&state(), "zzz").is_empty());
    }
}
