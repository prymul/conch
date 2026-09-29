//! Rustyline-free primitives shared by [`crate::completion`] and
//! [`crate::highlight`] for scanning a possibly-incomplete,
//! possibly-invalid-mid-edit shell command line — the kind rustyline's
//! `Completer`/`Highlighter` traits hand `conch`'s binary crate. Both
//! operate on the raw, in-progress `(line, pos)` a user is still typing,
//! not a token stream `conch-shell-lexer` could `lex()`: an open quote
//! mid-typing is exactly what that crate's `lex()` reports as an error,
//! not a valid partial token.
//!
//! Deliberately lives here, not in `conch`'s own binary crate: it has no
//! `rustyline` dependency at all (unlike the *other* half of this same
//! problem — word/quote-boundary extraction from a raw cursor position,
//! which does need `rustyline::completion::Quote` and stays in `conch`'s
//! own `src/quoting.rs` as the thin adapter's own concern), so it's free
//! to live alongside [`crate::completion::command_candidates`] and
//! [`crate::highlight::classify`] — the pure cores that actually need
//! it — without pulling a line-editing crate into this one.

/// POSIX/bash reserved words this crate's own parser recognizes — shared
/// by [`crate::highlight`] (keyword coloring) and [`crate::completion`]
/// (real bash's own `compgen -c`, confirmed empirically, includes
/// keywords alongside builtins/functions/aliases/`$PATH` executables at
/// command position — a plain `wh<TAB>` really does offer `while` as a
/// candidate, not just any command starting with `wh`).
///
/// Deliberately its own fixed, hardcoded list here rather than imported:
/// `conch_shell_parser::parser`'s own `reserved_word` function (the
/// authoritative list this is kept in sync with by hand) is a private
/// implementation detail of that crate, not exported. Low drift risk
/// despite the duplication: POSIX's shell reserved-word set is part of
/// the grammar itself and essentially never changes, unlike (say) the
/// builtin-name list [`crate::completion::CompletionState`] reads *live*
/// from `Shell` specifically to avoid this same risk where it's much
/// more likely to bite (bash-extension builtins get added far more often
/// than the grammar gains new reserved words).
pub const RESERVED_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function",
];

/// `rustyline`'s own default `rl_completer_word_break_characters`
/// (confirmed by reading `rustyline::completion`'s private
/// `default_break_chars` — not `pub`, so mirrored here rather than
/// called) — bash's own readline word-break set, which is also a
/// reasonable definition of "a shell metacharacter that always ends a
/// word" for conch's own grammar (whitespace, quote/escape characters,
/// and every operator character this crate's lexer recognizes).
#[must_use]
pub fn is_break_char(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t'
            | '\n'
            | '"'
            | '\\'
            | '\''
            | '`'
            | '@'
            | '$'
            | '>'
            | '<'
            | '='
            | ';'
            | '|'
            | '&'
            | '{'
            | '('
            | '\0'
    )
}

/// Byte offset of the start of the "current command" within
/// `line[..limit]` — i.e. the position right after the last *unquoted*
/// command separator (`;`, `|`, `&`, `(`, or a newline — `&&`/`||` are
/// each just two of those characters back to back, so no separate case
/// is needed) found before `limit`, or `0` if none.
///
/// Used to decide whether the word under the cursor is in *command
/// position* (the first word since the last separator) or *argument
/// position* (everything else) — the same distinction bash's own
/// programmable-completion machinery draws via `COMP_CWORD == 0` (GNU
/// Bash Reference Manual, "Programmable Completion").
///
/// Deliberately a narrower approximation than real bash's own
/// `COMP_WORDS`/`COMP_CWORD` tracking: this only recognizes the
/// *lexical* separators above, not reserved-word boundaries like
/// `then`/`do`/`else` (e.g. `if true; then <TAB>` is not recognized as
/// command position here, even though real bash's own completion does
/// treat the word right after `then` as a fresh command) — a documented,
/// narrower scope for this phase, not a claim of full `COMP_CWORD`
/// parity.
#[must_use]
fn last_command_boundary(line: &str, limit: usize) -> usize {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Normal,
        Single,
        Double,
        Escape,
        EscapeInDouble,
    }

    let mut mode = Mode::Normal;
    let mut boundary = 0;
    for (index, ch) in line[..limit.min(line.len())].char_indices() {
        mode = match (mode, ch) {
            (Mode::Double, '"') => Mode::Normal,
            (Mode::Double, '\\') => Mode::EscapeInDouble,
            (Mode::Double, _) => Mode::Double,
            (Mode::Escape, _) => Mode::Normal,
            (Mode::EscapeInDouble, _) => Mode::Double,
            (Mode::Normal, '"') => Mode::Double,
            (Mode::Normal, '\'') => Mode::Single,
            (Mode::Normal, '\\') => Mode::Escape,
            (Mode::Normal, ';' | '|' | '&' | '(' | '\n') => {
                boundary = index + ch.len_utf8();
                Mode::Normal
            }
            (Mode::Normal, _) => Mode::Normal,
            (Mode::Single, '\'') => Mode::Normal,
            (Mode::Single, _) => Mode::Single,
        };
    }
    boundary
}

/// Whether the word starting at byte offset `word_start` is in *command
/// position* — see [`last_command_boundary`]'s own docs for exactly what
/// that means and its documented narrower scope.
#[must_use]
pub fn is_command_position(line: &str, word_start: usize) -> bool {
    let boundary = last_command_boundary(line, word_start);
    line[boundary..word_start].trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_word_is_command_position() {
        assert!(is_command_position("ec", 0));
    }

    #[test]
    fn word_after_pipe_is_command_position() {
        let line = "echo hi | gr";
        assert!(is_command_position(line, 10));
    }

    #[test]
    fn word_after_semicolon_is_command_position() {
        let line = "echo hi; ec";
        assert!(is_command_position(line, 9));
    }

    #[test]
    fn argument_word_is_not_command_position() {
        let line = "echo hi wor";
        assert!(!is_command_position(line, 8));
    }

    #[test]
    fn separator_inside_quotes_does_not_count() {
        let line = "echo \"a; b\" wor";
        assert!(!is_command_position(line, 12));
    }

    #[test]
    fn word_after_double_ampersand_is_command_position() {
        let line = "true && ec";
        assert!(is_command_position(line, 8));
    }
}
