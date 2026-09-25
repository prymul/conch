//! The abstract syntax tree: simple commands, pipelines,
//! `;`/`&&`/`||`/`&`-joined lists (Phase 1), and compound commands —
//! `if`/`for`/`while`/`until`/`case`, subshells, and brace groups (Phase
//! 3) — POSIX.1-2024 (Issue 8) Shell Command Language, section 2.10.2
//! "Shell Grammar Rules".
//!
//! Every type here maps directly onto a named production in that formal
//! grammar (named in each type's docs) so the mapping can be checked by
//! inspection. Nothing here evaluates short-circuit (`&&`/`||`) semantics,
//! background (`&`) execution, or a compound command's own control-flow
//! meaning (looping, branching, subshell isolation) — this crate only
//! represents the tree's *shape*; `conch-shell-core`'s executor is what
//! walks it and gives every node its runtime meaning.
//!
//! Functions (`name() { ...; }`), `local`, `return`, and positional
//! parameters (`$1`, `$@`, `$#`, `set`) are a deliberate Phase 3 follow-up,
//! not represented here yet — see [`ForClause::words`]'s docs for the one
//! place that already shows through (the `for name; do ...; done` form,
//! which POSIX defines as implicitly iterating `"$@"`).

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

/// POSIX `command`. `#[non_exhaustive]` so adding a future variant isn't
/// a breaking change for `conch-shell-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Command {
    Simple(SimpleCommand),
    /// POSIX `compound_command` (optionally followed by a
    /// `redirect_list`, folded into [`CompoundCommand::redirects`] —
    /// see that type's docs).
    Compound(CompoundCommand),
    /// POSIX `function_definition` (plus the bash `function` keyword
    /// extension — see [`FunctionDefinition`]'s docs).
    Function(FunctionDefinition),
}

/// A function definition — either POSIX `function_definition : fname '('
/// ')' linebreak function_body`, or the bash extension `function fname
/// [()] compound-command` (parens optional there). Both forms produce
/// this same shape; nothing downstream needs to know which syntax was
/// used to define it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDefinition {
    /// POSIX `fname : NAME` (rule 8) — validated the same way
    /// [`ForClause::name`] already is.
    pub name: String,
    /// POSIX `function_body : compound_command | compound_command
    /// redirect_list` — any compound command, not just a brace group
    /// (confirmed against real bash: `foo() (subshell-body)` is valid).
    pub body: CompoundCommand,
}

/// POSIX `compound_command`, together with any trailing `redirect_list`
/// (grammar: `command : compound_command | compound_command
/// redirect_list`) — folded in here the same way [`SimpleCommand`]
/// already folds its prefix/suffix redirects into one list, for the same
/// reason: redirect *execution* order doesn't depend on this distinction,
/// only on redirect-vs-redirect order, which `redirects`' `Vec` order
/// already preserves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundCommand {
    pub kind: CompoundCommandKind,
    pub redirects: Vec<Redirect>,
}

/// The seven POSIX `compound_command` alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompoundCommandKind {
    /// `{ compound_list }` — POSIX `brace_group`. Runs in the
    /// **current** shell environment: unlike [`Self::Subshell`], a
    /// variable assignment or `cd` inside a brace group is visible to
    /// whatever runs after it.
    BraceGroup(CommandList),
    /// `( compound_list )` — POSIX `subshell`. Runs in a **subshell**:
    /// POSIX requires real fork semantics here, so a variable assignment
    /// or `cd` inside must never leak back to the parent shell — see
    /// [`SubshellBody`]'s docs for why that means the *executor* needs
    /// this variant's raw source text, not just its parsed body.
    Subshell(SubshellBody),
    For(ForClause),
    Case(CaseClause),
    If(IfClause),
    While(WhileClause),
    Until(UntilClause),
}

/// A subshell's body, kept in two forms for two different consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubshellBody {
    /// The parsed body — validated eagerly, so a syntax error inside a
    /// subshell (`(if true; then)`) is reported immediately rather than
    /// deferred to whenever the executor actually runs it.
    pub body: CommandList,
    /// The exact original source text between the parens (not including
    /// them). `conch-shell-core::exec` uses *this*, not `body`, to
    /// actually run the subshell: POSIX requires real fork semantics
    /// (a variable assignment, `cd`, or `exit` inside must never affect
    /// the parent shell), which an in-process walk of `body` cannot
    /// provide — `cd`'s underlying `std::env::set_current_dir` and
    /// `exit`'s `std::process::exit` both mutate real OS-level process
    /// state no amount of restoring `Shell`'s own fields afterward can
    /// undo. The executor instead re-execs `conch -c <source>` as a
    /// genuine child process, exactly like it already does for command
    /// substitution (`` $(...) ``/`` `...` ``) — see that mechanism's
    /// doc comment in `conch-shell-core::expand` for the same reasoning
    /// applied there first.
    pub source: String,
}

/// POSIX `for_clause`:
///
/// ```text
/// for_clause : For name                                      do_group
///            | For name                       sequential_sep do_group
///            | For name linebreak in          sequential_sep do_group
///            | For name linebreak in wordlist sequential_sep do_group
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForClause {
    /// POSIX `name` (rule 5, XBD 3.216 "Name") — validated at parse time
    /// to consist solely of underscores, digits, and portable-charset
    /// alphabetics, not starting with a digit.
    pub name: String,
    /// `Some(words)` for the `in wordlist` form (including `in` with an
    /// explicitly *empty* wordlist — `for x in; do ...; done` iterates
    /// zero times); `None` for the no-`in`-clause form. POSIX defines
    /// the `None` case as implicitly `in "$@"` — a real positional-
    /// parameter list, not the same thing as `Some(vec![])`, which is
    /// why this is `Option`-shaped rather than collapsing the two.
    /// `conch-shell-core` doesn't yet have positional-parameter state
    /// (a deliberate follow-up — see the module docs), so `None` is
    /// currently a documented gap for the executor, not silently treated
    /// as an empty iteration.
    pub words: Option<Vec<Word>>,
    /// POSIX `do_group`'s inner `compound_list`.
    pub body: CommandList,
}

/// POSIX `case_clause`:
///
/// ```text
/// case_clause : Case WORD linebreak in linebreak case_list    Esac
///             | Case WORD linebreak in linebreak case_list_ns Esac
///             | Case WORD linebreak in linebreak              Esac
/// ```
///
/// The `case_list`/`case_list_ns`/empty alternatives all collapse into
/// one `Vec<CaseArm>` here (possibly empty), since the only thing that
/// actually varies is whether the *last* arm has a terminator — see
/// [`CaseTerminator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseClause {
    pub word: Word,
    pub arms: Vec<CaseArm>,
}

/// One `pattern_list ')' compound_list terminator` arm of a
/// [`CaseClause`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseArm {
    /// One or more `|`-separated patterns (POSIX `pattern_list`),
    /// matched using the same POSIX 2.13 pattern notation (`*`/`?`/
    /// `[...]`) pathname expansion uses — not regex; matching itself is
    /// `conch-shell-core`'s job. An optional leading `(` before the
    /// first pattern (`pattern_list : '(' WORD | ...`) is accepted by
    /// the parser but carries no meaning of its own — purely cosmetic
    /// symmetry with the closing `)` — so it isn't represented here.
    pub patterns: Vec<Word>,
    /// The arm's body. Can be empty (`case x in foo) ;; esac`) — POSIX
    /// allows a pattern with no commands before its terminator.
    pub body: CommandList,
    pub terminator: CaseTerminator,
}

/// How a [`CaseArm`] ends. POSIX's `;;` (`DSEMI`, "stop, do not test any
/// further pattern") is the only terminator this parser decodes; bash's
/// `;&`/`;;&` fall-through extensions are a documented gap — encountering
/// either is a parse error (not a silent mis-parse), matching how
/// `parameter_expansion.rs` treats an unrecognized bash-extension
/// operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseTerminator {
    /// `;;`.
    Break,
    /// No terminator at all — POSIX `case_item_ns`, only valid for the
    /// arm immediately preceding `esac`.
    None,
}

/// POSIX `if_clause`:
///
/// ```text
/// if_clause : If compound_list Then compound_list else_part Fi
///           | If compound_list Then compound_list           Fi
/// else_part : Elif compound_list Then compound_list
///           | Elif compound_list Then compound_list else_part
///           | Else compound_list
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfClause {
    /// Each `(condition, body)` pair — the leading `if condition then
    /// body`, followed by zero or more `elif condition then body`
    /// (`else_part`'s recursive `Elif` alternative, flattened into this
    /// `Vec` the same way [`AndOrList::rest`] flattens repeated `&&`/
    /// `||` rather than nesting). Always has at least one element.
    pub branches: Vec<(CommandList, CommandList)>,
    /// The trailing `else body`, if present (`else_part`'s `Else`
    /// alternative).
    pub else_branch: Option<CommandList>,
}

/// POSIX `while_clause : While compound_list do_group`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileClause {
    pub condition: CommandList,
    /// `do_group`'s inner `compound_list`.
    pub body: CommandList,
}

/// POSIX `until_clause : Until compound_list do_group`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntilClause {
    pub condition: CommandList,
    /// `do_group`'s inner `compound_list`.
    pub body: CommandList,
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
