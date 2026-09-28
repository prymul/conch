//! Persistent history's pure `$HISTFILE`/`$HISTSIZE` resolution —
//! [`history_file_path`], [`history_size`]. The rustyline-`Editor`-
//! specific load/append glue (`conch`'s own binary crate calls this
//! eagerly, once per command — see that crate's own `history.rs` module
//! docs for the full "why," including the `Exit`/`exec <builtin>`
//! bypass this design sidesteps) stays there: it depends on
//! `rustyline::Editor`/`Helper`/`FileHistory` directly, which this crate
//! deliberately has no dependency on (see `crate::completion`/
//! `crate::word_scan`'s own module docs for the same layering reason).
//!
//! # `$HISTFILESIZE` is out of scope
//!
//! Real bash additionally trims the *on-disk* history file to
//! `$HISTFILESIZE` lines specifically when the shell exits. Implementing
//! that faithfully would need its own hook at every process-exit site —
//! exactly the "multiple independent bypass points" problem the eager-
//! append design (`conch`'s own binary crate) exists to sidestep, not
//! something to reintroduce for a narrower, rarely-configured variable.
//! Left unimplemented, documented here rather than silently absent:
//! conch's history file simply grows without a separate on-disk trim
//! (bounded only by [`history_size`]'s own `$HISTSIZE` cap, applied as
//! entries are added).

use std::path::PathBuf;

use crate::Shell;

/// Where persistent history is read from/written to: `$HISTFILE` if set
/// (matching bash's own `$HISTFILE`-honoring behavior, per this
/// project's established policy of tracking bash's actual behavior over
/// inventing conch-specific conventions), else `~/.conch_history` (bash's
/// own convention is `~/.bash_history`; conch's default is the same
/// shape, under its own name, not literally bash's file).
///
/// Returns `None` — silently skipping history persistence entirely,
/// exactly matching real bash's own behavior for `HISTFILE=""` — when
/// `$HISTFILE` is explicitly set to the empty string, *or* when it's
/// unset and `$HOME` can't be determined either (no directory to place
/// the default file in).
#[must_use]
pub fn history_file_path(shell: &Shell) -> Option<PathBuf> {
    match shell.get_var("HISTFILE") {
        Some("") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => shell
            .get_var("HOME")
            .map(|home| PathBuf::from(home).join(".conch_history")),
    }
}

/// `$HISTSIZE`, parsed — the in-memory/on-disk history entry cap (see
/// `rustyline::Config::max_history_size`). Unset or unparseable falls
/// back to *effectively unlimited* (`usize::MAX`), matching real bash's
/// own actual default — confirmed empirically (`env -i bash --norc
/// --noprofile` with `$HISTSIZE` left unset does not silently cap
/// history at some small built-in number) — deliberately *not*
/// rustyline's own unrelated built-in default of 100, which would be a
/// real, silent behavioral divergence from bash if left alone.
#[must_use]
pub fn history_size(shell: &Shell) -> usize {
    shell
        .get_var("HISTSIZE")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honors_explicit_histfile() {
        let mut shell = Shell::new();
        shell
            .env_vars
            .insert("HISTFILE".to_string(), "/tmp/custom_hist".to_string());
        assert_eq!(
            history_file_path(&shell),
            Some(PathBuf::from("/tmp/custom_hist"))
        );
    }

    #[test]
    fn falls_back_to_home_conch_history() {
        let mut shell = Shell::new();
        shell.env_vars.remove("HISTFILE");
        shell
            .env_vars
            .insert("HOME".to_string(), "/home/conch".to_string());
        assert_eq!(
            history_file_path(&shell),
            Some(PathBuf::from("/home/conch/.conch_history"))
        );
    }

    #[test]
    fn empty_histfile_disables_persistence() {
        let mut shell = Shell::new();
        shell.env_vars.insert("HISTFILE".to_string(), String::new());
        shell
            .env_vars
            .insert("HOME".to_string(), "/home/conch".to_string());
        assert_eq!(history_file_path(&shell), None);
    }

    #[test]
    fn no_histfile_and_no_home_disables_persistence() {
        let mut shell = Shell::new();
        shell.env_vars.remove("HISTFILE");
        shell.env_vars.remove("HOME");
        shell.shell_vars.remove("HOME");
        assert_eq!(history_file_path(&shell), None);
    }

    #[test]
    fn histsize_unset_is_effectively_unbounded() {
        let mut shell = Shell::new();
        shell.env_vars.remove("HISTSIZE");
        assert_eq!(history_size(&shell), usize::MAX);
    }

    #[test]
    fn histsize_honors_a_set_value() {
        let mut shell = Shell::new();
        shell
            .env_vars
            .insert("HISTSIZE".to_string(), "200".to_string());
        assert_eq!(history_size(&shell), 200);
    }

    #[test]
    fn unparseable_histsize_falls_back_to_unbounded() {
        let mut shell = Shell::new();
        shell
            .env_vars
            .insert("HISTSIZE".to_string(), "not-a-number".to_string());
        assert_eq!(history_size(&shell), usize::MAX);
    }
}
