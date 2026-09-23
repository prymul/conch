//! POSIX/bash-compatible shell parser: Phase 1 scope.
//!
//! This crate turns `conch-shell-lexer`'s token stream into an AST, per
//! POSIX.1-2024 (Issue 8) Shell Command Language, section 2.10.2 "Shell
//! Grammar Rules". See [`ast`] for the full grammar-to-type mapping.
//!
//! # Scope
//!
//! Per the project's phased roadmap, this crate implements:
//! - simple commands, with leading `NAME=value` assignments recognized
//!   only in the grammar's `cmd_prefix` position (see
//!   [`ast::SimpleCommand`]'s docs for why that position-sensitivity
//!   matters and is checked against real bash behavior);
//! - pipelines (`cmd1 | cmd2 | ...`);
//! - lists joined by `;`, `&&`, `||`, and a trailing `&`, preserving
//!   *which* operator joins each pair rather than evaluating
//!   short-circuit/background semantics — that's `conch-shell-core`'s
//!   job, walking this tree;
//! - basic redirection (`<`, `>`, `>>`, with an optional `IO_NUMBER` fd
//!   prefix like `2>`).
//!
//! Compound commands (`if`/`for`/`while`/`case`, subshells, brace groups),
//! functions, and every redirection form beyond the three above (`<<`,
//! `<<-`, `<&`, `>&`, `<>`, `>|`) are recognized by the lexer as real
//! POSIX/bash grammar but rejected here with a specific
//! [`ParseError::UnsupportedConstruct`] naming which later phase adds
//! them — see [`ast::Command`] and [`ast::RedirectOperator`].
//!
//! Full parameter-expansion operators, command substitution, arithmetic
//! evaluation, pathname expansion, and word splitting are Phase 2 scope
//! (expansion, not grammar) and live in `conch-shell-core`; this crate's
//! job is only to make sure those constructs parse into a
//! `conch-shell-lexer` [`Word`]/[`WordSegment`] shape Phase 2 can evaluate
//! without re-lexing — see `conch-shell-lexer`'s crate docs for that
//! contract in full.
//!
//! # Example
//!
//! ```
//! use conch_shell_parser::{Command, LogicalOp, Separator, parse};
//!
//! let list = parse("echo hi && echo bye; sleep 1 &").unwrap();
//! assert_eq!(list.items.len(), 2);
//!
//! let first = &list.items[0];
//! assert_eq!(first.and_or.rest[0].0, LogicalOp::And);
//! assert_eq!(first.separator, Separator::Sequential);
//!
//! let second = &list.items[1];
//! assert_eq!(second.separator, Separator::Async); // trailing `&`
//! let Command::Simple(cmd) = &second.and_or.first.commands[0] else {
//!     unreachable!()
//! };
//! assert_eq!(cmd.name.as_ref().unwrap().as_plain_literal(), Some("sleep"));
//! ```

mod ast;
mod error;
mod parser;

pub use ast::{
    AndOrList, Assignment, Command, CommandList, CommandListItem, LogicalOp, Pipeline, Redirect,
    RedirectOperator, Separator, SimpleCommand,
};
pub use conch_shell_lexer::{
    CommandSubstitution, Operator, Parameter, SpecialParameter, SubstitutionStyle, Word,
    WordSegment,
};
pub use error::ParseError;
pub use parser::parse;

#[cfg(test)]
mod tests;
