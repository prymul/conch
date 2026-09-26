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
//! Command substitution *evaluation* (recursively running the shell
//! script inside `` $(...) ``/`` `...` ``), pathname expansion, word
//! splitting, and tilde expansion are Phase 2 *evaluation* scope and live
//! in `conch-shell-core`, not here — this crate's job is only to make
//! sure those constructs parse into a `conch-shell-lexer`
//! [`Word`]/[`WordSegment`] shape the evaluator can consume without
//! re-lexing (see `conch-shell-lexer`'s crate docs for that contract in
//! full).
//!
//! Full parameter-expansion *operator* parsing (POSIX 2.6.2's
//! `${parameter:-word}` and its siblings — see the [`parameter_expansion`]
//! module, re-exported at the crate root) and arithmetic-expansion
//! *grammar* parsing (POSIX 2.6.4's `$((expression))`, ISO C precedence —
//! see the [`arithmetic`] module, also re-exported) **do** live in this
//! crate, even though they're triggered by Phase 2 word expansion rather
//! than by [`parse`] itself: both operate on a raw string a
//! [`WordSegment`] already carries (`ComplexParameterExpansion`'s body,
//! `ArithmeticExpansion`'s body) with no shell state required to build
//! their respective trees — exactly the same "parsing, not evaluation"
//! boundary the rest of this crate draws. `conch-shell-core` calls these
//! entry points directly (not through [`parse`]) once it actually needs
//! to expand one of those segments; see each module's docs ([`parameter_expansion`],
//! [`arithmetic`]) for the precise integration contract, including — for
//! parameter expansion — a quoting-context flag that's easy to get
//! backwards without realizing it.
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
//! assert!(matches!(second.separator, Separator::Async(_))); // trailing `&`
//! let Command::Simple(cmd) = &second.and_or.first.commands[0] else {
//!     unreachable!()
//! };
//! assert_eq!(cmd.name.as_ref().unwrap().as_plain_literal(), Some("sleep"));
//! ```

mod arithmetic;
mod ast;
mod error;
mod parameter_expansion;
mod parser;

pub use arithmetic::{
    ArithAssignOp, ArithBinaryOp, ArithError, ArithExpr, ArithUnaryOp, IncrDecrOp,
    parse_arithmetic_body, parse_arithmetic_expr,
};
pub use ast::{
    AndOrList, Assignment, CaseArm, CaseClause, CaseTerminator, Command, CommandList,
    CommandListItem, CompoundCommand, CompoundCommandKind, ForClause, FunctionDefinition, IfClause,
    LogicalOp, Pipeline, Redirect, RedirectOperator, Separator, SimpleCommand, SubshellBody,
    UntilClause, WhileClause,
};
pub use conch_shell_lexer::{
    CommandSubstitution, Operator, Parameter, SpecialParameter, SubstitutionStyle, Word,
    WordSegment,
};
pub use error::ParseError;
pub use parameter_expansion::{
    NullMode, ParamExpansionError, ParameterExpansion, ParameterOperator, parse_parameter_expansion,
};
pub use parser::parse;

#[cfg(test)]
mod tests;
