//! Tab completion's thin [`rustyline::completion::Completer`] adapter.
//! The pure candidate-generation core (`CompletionState`,
//! `command_candidates`) lives in `conch_shell_core::completion` instead
//! — see that module's own docs for why (in short: `tests/conch-difftest`
//! needs to call it in-process against a real bash oracle, which is only
//! possible for something reachable from a real library crate; this
//! binary crate has no `[lib]` target).
//!
//! Two genuinely different completion behaviors, matching bash's own
//! `COMP_CWORD == 0` (command position) vs. `> 0` (argument position)
//! distinction (GNU Bash Reference Manual, "Programmable Completion") —
//! see [`conch_shell_core::is_command_position`]:
//! - **Command position**: `conch_shell_core::command_candidates`
//!   (functions/builtins/aliases/reserved words/`$PATH` executables).
//! - **Argument position**: ordinary quote-aware filename completion —
//!   delegated wholesale to `rustyline`'s own
//!   [`rustyline::completion::FilenameCompleter`] rather than
//!   reimplemented here (see [`crate::helper::ConchHelper`]'s own docs
//!   for why that's a deliberate reuse decision, not a shortcut).
//!
//! # Re-quoting a chosen candidate for insertion
//!
//! Security review finding: backslash-escaping a fixed blocklist of
//! "break characters" (rustyline's own `escape()`, used for the
//! *unquoted*-insertion case) can miss one, and — more concretely — can't
//! even correctly *represent* some characters at all in unquoted context
//! (a literal embedded newline specifically: `\` immediately followed by
//! a real newline is POSIX line-continuation, i.e. "join with the next
//! line," not "insert a literal newline character," so backslash-escaping
//! a newline the way a blocklist-based escaper would silently corrupts a
//! name containing one). [`escaped_candidate`] instead wraps the
//! unquoted-insertion case in `'...'`, escaping an embedded `'` the
//! POSIX-standard way (close the quote, an escaped literal quote, reopen:
//! `'\''`) — this project's own established technique for "safely embed
//! an arbitrary string as literal shell text" (mirrors
//! `tests/conch-difftest`'s own `shell_single_quote` helper, used for the
//! identical problem on the oracle side). This is unconditionally safe
//! for *every* character, including one no fixed blocklist would think to
//! escape.
//!
//! Quote-context-awareness matters here too: a chosen candidate is only
//! ever single-quote-wrapped when insertion is happening *unquoted*.
//! Already inside an open `'...'`/`"..."` (`quote: Quote::Single`/
//! `Quote::Double`, from [`crate::quoting::word_at_cursor`]), wrapping in
//! a *fresh* `'...'` would be wrong — it would either break out of the
//! already-open quote or nest incorrectly rather than extending it — so
//! those two cases keep their own, quote-appropriate escaping instead
//! (none at all inside `'...'`, matching POSIX single-quote semantics and
//! `rustyline::completion::FilenameCompleter`'s own identical behavior
//! there; the narrower double-quote-special-character set — `"`, `$`,
//! `` ` ``, `\` — inside `"..."`, per POSIX 2.2.3, not this crate's
//! general unquoted break-character set, which would otherwise escape
//! characters that aren't actually special inside double quotes at all).

use rustyline::Context;
use rustyline::completion::{Completer, Pair, Quote, escape};

use crate::helper::ConchHelper;
use crate::quoting::word_at_cursor;

/// POSIX 2.2.3's double-quote escape set: only these four characters are
/// ever special after a backslash inside `"..."`. Reuses the *rule*
/// (mirroring `rustyline::completion::FilenameCompleter`'s own,
/// identically-scoped private `double_quotes_special_chars`, not
/// exported) rather than this crate's general
/// [`conch_shell_core::is_break_char`] set, which includes several
/// characters (`@`, `>`, `<`, `;`, `|`, `&`, `{`, `(`, ...) that have no
/// special meaning inside double quotes at all — escaping one of those
/// there would insert a spurious literal backslash into the completed
/// text instead of protecting anything.
fn is_double_quote_special(c: char) -> bool {
    matches!(c, '"' | '$' | '\\' | '`')
}

/// Wraps `s` in `'...'` for safe insertion into an *unquoted* position on
/// the line, escaping an embedded `'` the POSIX-standard way — see this
/// module's own docs for why this is preferred over backslash-escaping a
/// blocklist of break characters. Leaves a plain identifier (the
/// overwhelmingly common shape for a function/builtin/alias/keyword/
/// `$PATH`-executable name) untouched rather than wrapping it in quotes
/// that add visual noise for no safety benefit.
fn single_quote_wrap(s: &str) -> String {
    if s.is_empty() || !s.chars().any(conch_shell_core::is_break_char) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Re-escapes `name` for insertion back into the line at `quote`'s
/// context — see this module's own docs for the full reasoning per
/// quote kind.
fn escaped_candidate(name: String, quote: Quote) -> Pair {
    let replacement = match quote {
        Quote::Single => name.clone(),
        Quote::Double => escape(name.clone(), Some('\\'), is_double_quote_special, quote),
        Quote::None => single_quote_wrap(&name),
    };
    Pair {
        display: name,
        replacement,
    }
}

impl Completer for ConchHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let word = word_at_cursor(line, pos);
        if conch_shell_core::is_command_position(line, word.start) {
            let state = self.completion_state.borrow();
            let candidates = conch_shell_core::command_candidates(&state, &word.text)
                .into_iter()
                .map(|name| escaped_candidate(name, word.quote))
                .collect();
            Ok((word.start, candidates))
        } else {
            self.filename_completer.complete_path(line, pos)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_identifier_is_inserted_unquoted() {
        let pair = escaped_candidate("echo".to_string(), Quote::None);
        assert_eq!(pair.replacement, "echo");
    }

    #[test]
    fn unquoted_insertion_of_a_name_with_a_space_is_single_quote_wrapped() {
        let pair = escaped_candidate("my file".to_string(), Quote::None);
        assert_eq!(pair.replacement, "'my file'");
    }

    #[test]
    fn unquoted_insertion_of_an_embedded_newline_is_preserved_correctly() {
        // A backslash-escaped newline would be POSIX line-continuation
        // (silently dropped/joined), not a literal newline -- single-quote
        // wrapping is the only unquoted-context-safe way to represent
        // this at all.
        let pair = escaped_candidate("a\nb".to_string(), Quote::None);
        assert_eq!(pair.replacement, "'a\nb'");
    }

    #[test]
    fn unquoted_insertion_escapes_an_embedded_single_quote() {
        let pair = escaped_candidate("it's".to_string(), Quote::None);
        assert_eq!(pair.replacement, r"'it'\''s'");
    }

    #[test]
    fn already_single_quoted_context_inserts_raw_text() {
        let pair = escaped_candidate("a b".to_string(), Quote::Single);
        assert_eq!(pair.replacement, "a b");
    }

    #[test]
    fn already_double_quoted_context_only_escapes_double_quote_specials() {
        // '@' is a break character in this crate's general unquoted set
        // but has no special meaning inside double quotes -- must not
        // get a spurious backslash.
        let pair = escaped_candidate("user@host".to_string(), Quote::Double);
        assert_eq!(pair.replacement, "user@host");
    }

    #[test]
    fn already_double_quoted_context_escapes_a_literal_double_quote() {
        let pair = escaped_candidate("a\"b".to_string(), Quote::Double);
        assert_eq!(pair.replacement, "a\\\"b");
    }
}
