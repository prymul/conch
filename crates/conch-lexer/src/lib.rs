//! POSIX/bash-compatible shell tokenizer.
//!
//! This crate turns raw shell input text into a flat stream of
//! [`Token`]s, per POSIX.1-2024 (Issue 8) Shell Command Language, section
//! 2.10.1 "Shell Grammar Lexical Conventions" and section 2.3 "Token
//! Recognition". It is the lowest layer of `conch`'s
//! lexer → parser → expansion-engine pipeline; `conch-shell-parser`
//! consumes its output and re-exports its types.
//!
//! # Scope
//!
//! Per the project's phased roadmap, this crate implements:
//! - words, with the full POSIX quoting mechanisms: single quotes,
//!   double quotes, and backslash escaping;
//! - the complete POSIX operator token set (`|`, `||`, `&`, `&&`, `;`,
//!   `;;`, `<`, `<<`, `<<-`, `<&`, `<>`, `>`, `>>`, `>&`, `>|`, `(`, `)`),
//!   recognized by longest match, even though `conch-shell-parser`'s
//!   Phase 1 grammar only *accepts* a subset of them today — see that
//!   crate's docs for which, and why lexing the full set now is the
//!   right layering;
//! - `IO_NUMBER` tokens (a digit run immediately preceding `<`/`>`);
//! - comments (`#` to end of line, only at word-start);
//! - parameter expansion (`$name`, `${name}`), command substitution
//!   (`$(...)`, `` `...` ``), and arithmetic expansion (`$((...))`) —
//!   recognized structurally (so word boundaries are always found
//!   correctly, including through arbitrarily nested quoting/expansions)
//!   but not evaluated; see [`WordSegment`] for exactly what is and isn't
//!   decoded at this phase.
//!
//! # The expansion contract: why [`Word`] isn't a `String`
//!
//! POSIX 2.6 "Word Expansions" defines expansion as an ordered pipeline:
//!
//! 1. Tilde expansion, parameter expansion, command substitution, and
//!    arithmetic expansion (conceptually one left-to-right pass).
//! 2. Field splitting (of the *results* of step 1, only where unquoted).
//! 3. Pathname expansion (again, only where unquoted).
//! 4. Quote removal — always last.
//!
//! Steps 2 and 3 only apply to text that was unquoted, which means an
//! expansion engine cannot correctly implement them against a flattened
//! string — by the time quoting has been collapsed to a `String`, the
//! information about *which parts were quoted* is gone. [`Word`] keeps
//! that information: it's a sequence of [`WordSegment`]s, and each
//! variant is tagged with its quoting context. See [`WordSegment`]'s docs
//! for the full contract, including which parts of a word this crate has
//! already resolved (backslash-escape removal, which is purely syntactic)
//! versus what's left for the expansion engine (everything that depends
//! on an expansion result: splitting, globbing, and dropping the
//! now-unneeded quote-segment wrappers).
//!
//! # What Phase 2 will build on this
//!
//! [`WordSegment::ComplexParameterExpansion`], [`WordSegment::CommandSubstitution`],
//! and [`WordSegment::ArithmeticExpansion`] all carry their body as a
//! **verbatim raw string**, not further parsed. This crate has already
//! done the hard, error-prone part (finding the correct boundary through
//! arbitrary nested quoting — see `lexer.rs`'s module docs for the
//! pathological cases this was checked against, empirically, against real
//! bash); Phase 2 can parse `ComplexParameterExpansion`'s body into POSIX
//! 2.6.2 operators, and recursively invoke `conch-shell-parser`'s own
//! entry point on a `CommandSubstitution`'s body, without needing any
//! changes here.

mod error;
mod lexer;
mod token;

pub use error::LexError;
pub use lexer::{Lexer, lex};
pub use token::{
    CommandSubstitution, Operator, Parameter, Span, SpecialParameter, SubstitutionStyle, Token,
    TokenKind, Word, WordSegment,
};

#[cfg(test)]
mod tests;
