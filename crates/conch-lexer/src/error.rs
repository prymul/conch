//! Errors produced while tokenizing input.

/// An error encountered while lexing shell input.
///
/// Every variant carries the byte offset where the unterminated construct
/// *opened*, since that is almost always more useful for a diagnostic than
/// the offset where the lexer gave up (typically end-of-input).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LexError {
    /// `'...` with no closing `'`.
    #[error("unterminated single-quoted string (opened at byte {start})")]
    UnterminatedSingleQuote { start: usize },

    /// `"...` with no closing `"`.
    #[error("unterminated double-quoted string (opened at byte {start})")]
    UnterminatedDoubleQuote { start: usize },

    /// `${...` with no closing `}`.
    #[error("unterminated `${{...}}` parameter expansion (opened at byte {start})")]
    UnterminatedParameterExpansion { start: usize },

    /// `$(...` with no closing `)`.
    #[error("unterminated `$(...)` command substitution (opened at byte {start})")]
    UnterminatedCommandSubstitution { start: usize },

    /// `` `... `` with no closing backquote.
    #[error("unterminated backquoted command substitution (opened at byte {start})")]
    UnterminatedBackquote { start: usize },

    /// `$((...` with no closing `))`.
    #[error("unterminated `$((...))` arithmetic expansion (opened at byte {start})")]
    UnterminatedArithmeticExpansion { start: usize },

    /// A `\` as the very last character of the input, with no following
    /// character to escape and no newline to form a line continuation.
    #[error("backslash at end of input with no character to escape (byte {pos})")]
    TrailingBackslash { pos: usize },
}
