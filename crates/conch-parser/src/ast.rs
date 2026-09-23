//! The Phase 1 abstract syntax tree: simple commands, pipelines, and
//! `;`/`&&`/`||`/`&`-joined lists — POSIX.1-2024 (Issue 8) Shell Command
//! Language, section 2.10.2 "Shell Grammar Rules".
//!
//! Every type here maps directly onto a named production in that formal
//! grammar (named in each type's docs) so the mapping can be checked by
//! inspection. Nothing here evaluates short-circuit (`&&`/`||`) semantics
//! or background (`&`) execution — this crate only represents *which*
//! operator joins each pair; `conch-shell-core`'s executor is what walks
//! this tree and gives those operators their runtime meaning.

use conch_shell_lexer::Word;

/// A parsed complete input: zero or more [`CommandListItem`]s.
///
/// Corresponds to POSIX `complete_command`/`list`, flattened from the
/// grammar's left-recursive `list : list separator_op and_or | and_or`
/// into a `Vec` — each item already carries the separator that followed
/// it, which is equivalent information and easier for a tree-walking
/// executor to consume.
///
/// Empty input (or input that is only blank lines/comments) parses to an
/// empty `CommandList`, matching e.g. `$()` evaluating to nothing and an
/// empty line at a prompt running no command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandList {
    pub items: Vec<CommandListItem>,
}

/// One `and_or` from a [`CommandList`], together with the separator that
/// terminated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandListItem {
    pub and_or: AndOrList,
    pub separator: Separator,
}

/// How a [`CommandListItem`] was terminated — POSIX `separator_op`
/// (`;` or `&`) or a bare newline, or nothing (only valid for the last
/// item, at end of input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    /// `;` or a bare newline: run to completion before the next item.
    Sequential,
    /// `&`: per POSIX, this marks the preceding pipeline to run
    /// asynchronously (in the background). Phase 1 only *represents*
    /// this — `conch-shell-core` doesn't implement backgrounding until
    /// Phase 4 job control, per the project roadmap.
    Async,
    /// No trailing separator (end of input).
    None,
}

/// POSIX `and_or`: a left-associative chain of [`Pipeline`]s joined by
/// `&&`/`||`, preserving which operator joins each pair (short-circuit
/// evaluation is the executor's job, not this tree's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOrList {
    pub first: Pipeline,
    pub rest: Vec<(LogicalOp, Pipeline)>,
}

/// `&&` or `||`, per POSIX `AND_IF`/`OR_IF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    /// `&&`
    And,
    /// `||`
    Or,
}

/// POSIX `pipe_sequence`: one or more [`Command`]s joined by `|`, in
/// left-to-right pipeline order. Always has at least one command.
///
/// Does not represent a leading `!` (POSIX `pipeline : pipe_sequence |
/// Bang pipe_sequence`, i.e. pipeline negation) — `!` isn't in Phase 1's
/// operator scope, so it currently lexes as an ordinary word rather than
/// being recognized as the reserved word it is; see `conch-shell-lexer`'s
/// docs for the operator/reserved-word distinction this relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

/// POSIX `command`. Phase 1 only implements the `simple_command`
/// alternative; `compound_command` (subshells, `if`/`for`/`while`/`case`,
/// brace groups) and `function_definition` are Phase 3. `#[non_exhaustive]`
/// so adding those variants later isn't a breaking change for
/// `conch-shell-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Command {
    Simple(SimpleCommand),
}

/// POSIX `simple_command`, i.e. `cmd_prefix cmd_word cmd_suffix` and its
/// four other grammar alternatives (any of `cmd_prefix`/`cmd_word`/
/// `cmd_suffix` may be empty, except at least one of `cmd_prefix`/
/// `cmd_word` must be present).
///
/// `cmd_prefix`'s redirections and `cmd_suffix`'s redirections are
/// collapsed into one ordered `redirects` list here rather than kept
/// split by prefix/suffix position: redirect *execution* order only
/// depends on other redirects (a later one on the same fd overrides an
/// earlier one), never on its position relative to assignments or
/// arguments, so this loses no POSIX-meaningful information while being
/// simpler for the executor to walk. `assignments`, by contrast, keeps
/// its prefix-only position and original order, since `FOO=1 FOO=2 cmd`
/// (last wins) *does* depend on assignment-vs-assignment order — and
/// since POSIX only recognizes `ASSIGNMENT_WORD` in `cmd_prefix`: a
/// `cmd_suffix` word that merely looks like `NAME=value` is just an
/// ordinary argument (confirmed against real bash: `echo FOO=bar` prints
/// `FOO=bar` rather than setting `FOO`), which is why `args` is plain
/// [`Word`]s with no assignment reinterpretation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimpleCommand {
    /// Leading `NAME=value` assignments, in original left-to-right order.
    pub assignments: Vec<Assignment>,
    /// The command name (POSIX `cmd_name`/`cmd_word`). `None` iff this
    /// simple command is assignments/redirects only, e.g. a bare
    /// `FOO=bar` line — valid per the `cmd_prefix` alternative of
    /// `simple_command`.
    pub name: Option<Word>,
    /// Arguments after the command name (POSIX `cmd_suffix`'s `WORD`s).
    pub args: Vec<Word>,
    /// Every redirection, prefix and suffix, in original left-to-right
    /// order.
    pub redirects: Vec<Redirect>,
}

/// One `NAME=value` shell-variable assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub name: String,
    pub value: Word,
}

/// One `io_redirect`. Phase 1 implements the `<`, `>`, and `>>` operators
/// (with an optional leading `IO_NUMBER` fd prefix, e.g. `2>file`); see
/// [`RedirectOperator`] for what's deferred to Phase 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// The explicit file descriptor, e.g. the `2` in `2>file`. `None`
    /// means the operator's default (0 for input, 1 for output/append).
    pub fd: Option<u32>,
    pub operator: RedirectOperator,
    pub target: Word,
}

/// The redirection operators Phase 1 implements. `conch-shell-lexer`
/// already tokenizes the full POSIX operator set (`<<`, `<<-`, `<&`,
/// `>&`, `<>`, `>|`); this parser gives those tokens a clear
/// "not yet supported" [`crate::ParseError::UnsupportedConstruct`] error
/// rather than mis-parsing them, so there's no separate "unsupported
/// operator" variant here to keep in sync with the lexer's full set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOperator {
    /// `<`
    Input,
    /// `>`
    Output,
    /// `>>`
    Append,
}
