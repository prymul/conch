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
