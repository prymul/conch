//! Errors produced while parsing a token stream into an AST.

use conch_shell_lexer::{LexError, Span};

/// An error encountered while parsing shell input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Tokenization itself failed (unterminated quote/expansion, trailing
    /// backslash, ...) — see [`LexError`].
    #[error(transparent)]
    Lex(#[from] LexError),

    /// A token appeared where the grammar didn't allow it.
    #[error("unexpected {found} at byte {}, expected {expected}", .span.start)]
    UnexpectedToken {
        found: String,
        span: Span,
        expected: String,
    },

    /// Input ended where the grammar required another token.
    #[error("unexpected end of input, expected {expected}")]
    UnexpectedEof { expected: String },

    /// The input used a construct `conch-shell-lexer` recognizes (it's
    /// real POSIX/bash grammar) but this phase of the parser doesn't
    /// implement yet — e.g. `<<` here-documents or `(...)` subshells.
    /// Distinguished from [`ParseError::UnexpectedToken`] so callers (and
    /// error messages) can tell "this is invalid shell syntax" apart from
    /// "this is valid shell syntax conch doesn't support yet".
    #[error("{message} (at byte {})", .span.start)]
    UnsupportedConstruct { message: String, span: Span },
}

impl ParseError {
    /// Whether this error means "the input simply ran out before some
    /// still-open construct closed" (an unterminated quote/expansion
    /// site, or the grammar reaching end-of-input while still expecting
    /// another token) rather than a genuine syntax error found *within*
    /// the input actually given.
    ///
    /// This is the exact signal an interactive REPL needs to implement
    /// bash's `PS2` continuation prompt correctly: on a `true` result,
    /// read another line, append it (with a newline in between,
    /// reproducing the line break the user actually typed), and re-parse
    /// the combined input, rather than reporting the error immediately —
    /// see `conch`'s own binary crate for that loop. Consulting this
    /// method (rather than a second, hand-rolled "is this incomplete"
    /// check against the raw source text) is deliberate: it keeps the
    /// REPL's continuation logic and this crate's own error taxonomy from
    /// being able to silently drift apart as either evolves.
    ///
    /// Deliberately `false` for [`ParseError::UnsupportedConstruct`]: an
    /// unclosed `<<` here-document, for example, is a *hard* error today
    /// (here-documents aren't implemented at all yet — see this crate's
    /// own module docs), not a signal to keep reading more input, since
    /// there's no continuation logic here that could ever resolve it
    /// either way. Also `false` for [`ParseError::UnexpectedToken`]: that
    /// variant only ever fires when a *real* token was found somewhere
    /// the grammar didn't allow it, which more input could never fix.
    #[must_use]
    pub fn is_incomplete_input(&self) -> bool {
        matches!(
            self,
            ParseError::Lex(
                LexError::UnterminatedSingleQuote { .. }
                    | LexError::UnterminatedDoubleQuote { .. }
                    | LexError::UnterminatedParameterExpansion { .. }
                    | LexError::UnterminatedCommandSubstitution { .. }
                    | LexError::UnterminatedBackquote { .. }
                    | LexError::UnterminatedArithmeticExpansion { .. }
                    | LexError::TrailingBackslash { .. }
            ) | ParseError::UnexpectedEof { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unterminated_quote_is_incomplete() {
        let err = ParseError::Lex(LexError::UnterminatedDoubleQuote { start: 0 });
        assert!(err.is_incomplete_input());
    }

    #[test]
    fn trailing_backslash_is_incomplete() {
        let err = ParseError::Lex(LexError::TrailingBackslash { pos: 3 });
        assert!(err.is_incomplete_input());
    }

    #[test]
    fn unexpected_eof_is_incomplete() {
        let err = ParseError::UnexpectedEof {
            expected: "a command".to_string(),
        };
        assert!(err.is_incomplete_input());
    }

    #[test]
    fn unsupported_construct_is_not_incomplete() {
        let err = ParseError::UnsupportedConstruct {
            message: "here-documents are not yet supported".to_string(),
            span: Span::new(0, 2),
        };
        assert!(!err.is_incomplete_input());
    }

    #[test]
    fn unexpected_token_is_not_incomplete() {
        let err = ParseError::UnexpectedToken {
            found: "`;`".to_string(),
            span: Span::new(0, 1),
            expected: "a command".to_string(),
        };
        assert!(!err.is_incomplete_input());
    }
}
