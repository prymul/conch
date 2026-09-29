//! The `rustyline`-coupled half of this crate's tolerant line scanner —
//! word/quote-boundary extraction from a raw cursor position. The other
//! half (rustyline-free: `is_break_char`, `is_command_position`,
//! `RESERVED_WORDS`) lives in `conch_shell_core::word_scan` instead, so
//! it's reachable from `tests/conch-difftest` without pulling in a line-
//! editing crate there — see that module's own docs.
//!
//! Both halves exist for the same underlying reason: rustyline's
//! `Completer`/`Highlighter` traits hand this crate the raw, in-progress
//! `(line, pos)` a user is still typing, not a token stream
//! `conch-shell-lexer` could `lex()` (an open quote mid-typing is exactly
//! what that crate's `lex()` reports as an error, not a valid partial
//! token).
//!
//! Quote *character* semantics (`'`/`"`/`\`) mirror
//! `rustyline::completion`'s own (private) `find_unclosed_quote` state
//! machine — [`find_unclosed_quote`] below is a from-scratch
//! reimplementation of the same algorithm (that function isn't `pub`, so
//! it can't be called directly), but reuses rustyline's own *public*
//! [`rustyline::completion::Quote`] enum,
//! [`rustyline::completion::extract_word`], and
//! [`rustyline::completion::unescape`] so downstream code has one
//! quoting vocabulary, not two.

use conch_shell_core::is_break_char;
use rustyline::completion::Quote;

/// Scans `s` left to right and reports the last quote character that was
/// never closed, if any — i.e. whether byte offset `s.len()` (typically
/// `&line[..pos]`, a cursor position) sits inside an open `'...`/`"...`.
///
/// # Examples
///
/// - `find_unclosed_quote("ls /etc")` → `None`
/// - `find_unclosed_quote("ls \"User Information")` → `Some((3, Quote::Double))`
/// - `find_unclosed_quote("ls \"/User Information\" /etc")` → `None` (the
///   quote closes before the end)
#[must_use]
pub fn find_unclosed_quote(s: &str) -> Option<(usize, Quote)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Normal,
        Single,
        Double,
        Escape,
        EscapeInDouble,
    }

    let mut mode = Mode::Normal;
    let mut quote_index = 0;
    for (index, ch) in s.char_indices() {
        mode = match (mode, ch) {
            (Mode::Double, '"') => Mode::Normal,
            (Mode::Double, '\\') => Mode::EscapeInDouble,
            (Mode::Double, _) => Mode::Double,
            (Mode::Escape, _) => Mode::Normal,
            (Mode::EscapeInDouble, _) => Mode::Double,
            (Mode::Normal, '"') => {
                quote_index = index;
                Mode::Double
            }
            (Mode::Normal, '\'') => {
                quote_index = index;
                Mode::Single
            }
            (Mode::Normal, '\\') => Mode::Escape,
            (Mode::Normal, _) => Mode::Normal,
            (Mode::Single, '\'') => Mode::Normal,
            (Mode::Single, _) => Mode::Single,
        };
    }

    match mode {
        Mode::Double | Mode::EscapeInDouble => Some((quote_index, Quote::Double)),
        Mode::Single => Some((quote_index, Quote::Single)),
        _ => None,
    }
}

/// The word under the cursor, per [`find_unclosed_quote`] (if the cursor
/// is inside an open quote) or [`rustyline::completion::extract_word`]
/// (otherwise), with backslash-escapes already resolved
/// ([`rustyline::completion::unescape`]) the same way
/// `rustyline::completion::FilenameCompleter` resolves them before
/// matching against real filenames.
pub struct WordAtCursor {
    /// Byte offset (into the original `line`) where the word starts —
    /// i.e. where a completion candidate's replacement text should be
    /// spliced in from.
    pub start: usize,
    /// The word's text, with quoting/escaping already removed — what a
    /// completion candidate should actually be matched (by prefix)
    /// against.
    pub text: String,
    /// Which quote context `start..pos` is inside, if any — governs how
    /// a chosen candidate must be re-escaped before insertion (see
    /// [`rustyline::completion::escape`]).
    pub quote: Quote,
}

/// Finds the word under the cursor at `(line, pos)` — see
/// [`WordAtCursor`]'s own docs for exactly what's reported.
#[must_use]
pub fn word_at_cursor(line: &str, pos: usize) -> WordAtCursor {
    if let Some((idx, quote)) = find_unclosed_quote(&line[..pos]) {
        let start = idx + 1;
        let esc = if quote == Quote::Double {
            Some('\\')
        } else {
            None
        };
        let text = rustyline::completion::unescape(&line[start..pos], esc).into_owned();
        WordAtCursor { start, text, quote }
    } else {
        let (start, raw) =
            rustyline::completion::extract_word(line, pos, Some('\\'), is_break_char);
        let text = rustyline::completion::unescape(raw, Some('\\')).into_owned();
        WordAtCursor {
            start,
            text,
            quote: Quote::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_quote_is_unclosed_for_plain_text() {
        assert_eq!(find_unclosed_quote("ls /etc"), None);
    }

    #[test]
    fn open_double_quote_is_reported() {
        assert_eq!(
            find_unclosed_quote("ls \"User Information"),
            Some((3, Quote::Double))
        );
    }

    #[test]
    fn closed_double_quote_is_not_reported() {
        assert_eq!(find_unclosed_quote("ls \"/User Information\" /etc"), None);
    }

    #[test]
    fn open_single_quote_is_reported() {
        assert_eq!(find_unclosed_quote("echo 'a b"), Some((5, Quote::Single)));
    }

    #[test]
    fn escaped_double_quote_inside_double_quote_does_not_close_it() {
        assert_eq!(
            find_unclosed_quote("echo \"a \\\" b"),
            Some((5, Quote::Double))
        );
    }

    #[test]
    fn quote_inside_single_quotes_has_no_special_meaning() {
        // A `"` inside `'...'` is just a literal character (POSIX 2.2.2:
        // *nothing* is special inside single quotes) -- must not be
        // mistaken for opening a nested double-quoted region.
        assert_eq!(find_unclosed_quote("echo 'a \" b'"), None);
    }

    #[test]
    fn word_at_cursor_unquoted() {
        let word = word_at_cursor("ls /usr/loc", 11);
        assert_eq!(word.start, 3);
        assert_eq!(word.text, "/usr/loc");
        assert_eq!(word.quote, Quote::None);
    }

    #[test]
    fn word_at_cursor_inside_open_double_quote() {
        let line = "ls \"/usr/local dir";
        let word = word_at_cursor(line, line.len());
        assert_eq!(word.start, 4);
        assert_eq!(word.text, "/usr/local dir");
        assert_eq!(word.quote, Quote::Double);
    }

    #[test]
    fn word_at_cursor_unescapes_backslash_space() {
        let line = "ls /User\\ Information";
        let word = word_at_cursor(line, line.len());
        assert_eq!(word.text, "/User Information");
        assert_eq!(word.quote, Quote::None);
    }
}
