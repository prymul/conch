//! The recursive-descent parser implementation, one function per
//! POSIX 2.10.2 grammar production it implements.

use std::collections::HashMap;

use conch_shell_lexer::{Operator, Span, Token, TokenKind, Word, WordSegment};

use crate::ast::{
    AndOrList, Assignment, CaseArm, CaseClause, CaseTerminator, Command, CommandList,
    CommandListItem, CompoundCommand, CompoundCommandKind, ForClause, FunctionDefinition, IfClause,
    LogicalOp, Pipeline, Redirect, RedirectOperator, Separator, SimpleCommand, SubshellBody,
    UntilClause, WhileClause,
};
use crate::error::ParseError;

/// Parses a complete `input` into a [`CommandList`], with no alias table
/// at all — equivalent to [`parse_with_aliases`] with an empty one. Most
/// callers want this: alias expansion is an interactive-shell-only bash
/// extension (see that function's own docs), and every non-interactive
/// entry point (`-c`, a script file, `eval`, `.`/`source`) should use
/// this plain form instead.
///
/// # Errors
///
/// Returns [`ParseError`] if `input` fails to tokenize (see
/// `conch-shell-lexer`), uses a construct outside Phase 1's grammar
/// (giving [`ParseError::UnsupportedConstruct`] when that construct is at
/// least *recognized*, real POSIX/bash grammar — see that variant's
/// docs), or is otherwise not valid POSIX shell grammar.
pub fn parse(input: &str) -> Result<CommandList, ParseError> {
    parse_with_aliases(input, &HashMap::new())
}

/// [`parse`], but consulting `aliases` (name → replacement text) the
/// same way real bash does: checked against the first word of *every*
/// simple command (POSIX 2.10.2 rule 1's own reserved-word-recognition
/// spot — [`Parser::parse_command`] — is exactly where this needs to
/// hook in too, since an alias's replacement can itself turn out to be a
/// reserved word, a nested alias, or introduce entirely new operator
/// tokens the original source never had, e.g. `alias ll='ls -la | less'`
/// splicing in a genuine pipe), never inside a quoted word or any other
/// word position (arguments, case patterns, ...) — reusing
/// [`Word::as_plain_literal`] is what gives this the same quote-awareness
/// [`reserved_word`] already relies on, for free.
///
/// # Known gap
///
/// This crate parses a *whole* input string up front, in one pass, with
/// no interleaved execution — unlike real bash, which reads and expands
/// one command at a time, so `alias foo=bar` immediately affects a
/// *later* command in the very same interactive input. Since `aliases`
/// here is a single, fixed snapshot for this entire `parse_with_aliases`
/// call, `alias foo=bar; foo` typed as one semicolon-joined line does
/// *not* see its own just-defined alias — a narrow, honestly-documented
/// gap. It doesn't come up across separate interactive lines, though:
/// the interactive REPL loop (`conch`'s own binary crate) already calls
/// this fresh, with the shell's then-current alias table, once per line
/// read, so `alias foo=bar` on one line and `foo` on the next already
/// works correctly.
///
/// # Errors
///
/// See [`parse`].
pub fn parse_with_aliases(
    input: &str,
    aliases: &HashMap<String, String>,
) -> Result<CommandList, ParseError> {
    let tokens = conch_shell_lexer::lex(input)?;
    Parser::new(input, tokens, aliases).parse_command_list()
}

struct Parser<'a> {
    /// The input, kept only so a construct whose *execution* needs its
    /// own verbatim source text (today: a subshell's body — see
    /// [`Self::parse_subshell`] and `conch-shell-core::exec`'s module
    /// docs for why a subshell must run in a genuinely separate process,
    /// not by walking a parsed AST in-process — and
    /// [`Self::parse_optional_separator`]'s `&`/async case) can slice it
    /// out by byte offset, the same way `conch-shell-lexer` already
    /// hands `conch-shell-core` the verbatim body of a
    /// `$(...)`/`` `...` `` command substitution.
    ///
    /// Owned (`String`), not a borrow of the caller's original input,
    /// specifically so [`Self::splice_alias_tokens`] can physically
    /// rewrite it in place whenever an alias is expanded — see that
    /// function's own docs for why every span-based slice of this field
    /// would otherwise risk mixing two different strings' byte offsets
    /// once alias expansion has spliced in tokens re-lexed from a wholly
    /// separate replacement string.
    source: String,
    /// A `Vec` + cursor index rather than the `Peekable<IntoIter<Token>>`
    /// this used to be — needed for [`Self::peek_nth`], which the POSIX
    /// `fname()` function-definition form requires: distinguishing
    /// `foo()` (a function definition) from an ordinary simple command
    /// named `foo` followed by a stray `(` needs to look two tokens
    /// ahead (`Word`, then immediately `(`, then `)`), which a
    /// single-token `Peekable` can't do. Every other cursor primitive
    /// keeps the exact same signature it always had, so this doesn't
    /// ripple out to the dozens of existing `peek`/`next` call sites —
    /// only this struct and the primitives themselves changed.
    tokens: Vec<Token>,
    pos: usize,
    /// See [`crate::parse_with_aliases`]'s own docs — consulted by
    /// [`Self::maybe_expand_alias`] only, at the one grammar spot
    /// ([`Self::parse_command`]) where a fresh simple command's first
    /// word is recognized.
    aliases: &'a HashMap<String, String>,
    /// Current `command`-grammar recursion depth — see
    /// [`MAX_COMMAND_DEPTH`].
    depth: u32,
    /// How many alias substitutions [`Self::maybe_expand_alias`] has
    /// actually performed so far, across this *entire* parse (every
    /// statement, not just the current one) — see [`MAX_ALIAS_EXPANSIONS`].
    alias_expansions: u32,
}

/// A conservative bound on how many levels deep `command` can recurse
/// through the mutually-recursive compound-command chain
/// (`parse_command` → `parse_subshell`/`parse_if_clause`/
/// `parse_while_clause`/`parse_until_clause`/`parse_for_clause`/
/// `parse_case_clause`/`parse_brace_group`/`parse_function_body` →
/// `parse_compound_list` → `parse_command_list_item` → `parse_and_or` →
/// `parse_pipeline` → `parse_command` again) before giving up, mirroring
/// this project's established recursion-guard precedent
/// (`conch-shell-core::expand`'s `MAX_ARITH_RECURSION`,
/// `conch-shell-builtins`' `MAX_EVAL_RECURSION`, and this crate's own
/// sibling `arithmetic::MAX_PAREN_DEPTH`): before this guard existed,
/// nothing tracked this chain's depth at all, so deeply nested input —
/// `((((((...))))))` (subshells, not arithmetic) or `{ { { ... } } }`
/// groups, chief among the shapes this recursion actually sees — could
/// overflow the stack instead of producing any [`ParseError`]. Every
/// entry into [`Parser::parse_command`] costs exactly one level here,
/// which is where the guard lives (see that function's own wrapper for
/// why it — not any of the individual compound-command parsers — is the
/// one right place to count from: it's the single, unavoidable
/// re-entry point every one of them funnels back through).
///
/// Empirically probed the same way as this crate's own sibling
/// `arithmetic::MAX_PAREN_DEPTH` (see that constant's docs for the
/// method): against this crate's `cargo test` default 2 MiB-per-thread
/// stack (debug build, the same profile CI's `cargo test --all-features`
/// uses), both a `((((...))))`-subshell and a `{ { { ... } } }`-brace-group
/// probe still parsed/errored cleanly at 120 levels deep, with no
/// overflow observed at any depth tried up to that point — this chain is
/// shorter per level (`parse_command` → one compound-command parser →
/// `parse_compound_list` → `parse_command_list_item` → `parse_and_or` →
/// `parse_pipeline` → `parse_command`, about 6 frames) than
/// `MAX_PAREN_DEPTH`'s ~17-function precedence-ladder pass, so it has
/// more headroom at the same numeric bound, not less. 40 is chosen well
/// below even that already-comfortable 120-deep observed-safe point,
/// since it's already far beyond any nesting depth a real,
/// non-adversarial script would ever use.
const MAX_COMMAND_DEPTH: u32 = 40;

/// A conservative bound on the *total* number of alias substitutions
/// [`Parser::maybe_expand_alias`] will perform across one whole parse
/// (every statement in the input, not just one), guarding against an
/// unbounded-memory denial-of-service a self-referential alias
/// containing a list operator can trigger.
///
/// # Why this can't be `maybe_expand_alias`'s own existing per-call guard
///
/// That function already refuses to expand the *same name* twice within
/// one of its own calls (`expanding: HashSet<String>`) — necessary, but
/// not sufficient, because `maybe_expand_alias` runs once per
/// [`Parser::parse_command`] call, and `parse_command` itself is called
/// repeatedly by *five separate, structurally identical sibling loops*:
/// [`Parser::parse_pipeline`]'s `|` loop, [`Parser::parse_and_or`]'s
/// `&&`/`||` loop, [`Parser::parse_compound_list`]/[`Parser::parse_command_list`]'s
/// `;`/newline-separated list loop, and [`Parser::parse_optional_separator`]'s
/// `&` case. An alias whose replacement text reintroduces its own name
/// *plus* a list operator (`alias c='c|c'`) survives its own call's
/// self-reference check (the replacement is only two tokens deep, no
/// immediate re-expansion within that one call) but hands the **next**
/// sibling-loop iteration a fresh, empty `HashSet` that happily expands
/// the exact same alias again, doubling the pending parse work each
/// round — confirmed empirically (this project's own fuzzing pass): `(
/// printf 'alias c="c|c"\nc\n'; sleep 5 ) | conch` reaches several
/// gigabytes of resident memory within a few seconds, with no way for
/// the OS to intervene on a system that doesn't enforce `ulimit -v`
/// (confirmed: it doesn't on this project's own macOS development
/// machine). Confirmed general, not `|`-specific: `;`, `&&`, and `||`
/// all reproduce the identical unbounded-growth pattern; `&` reproduces
/// a distinct pre-existing panic first (a separate, already-tracked
/// bug) that happens to mask it, but shares the identical root cause and
/// would need the exact same guard once that panic is fixed.
///
/// # Why the guard lives on `Parser` itself, not deeper in `maybe_expand_alias`
///
/// A counter reset once per [`Parser::new`] (i.e. once per whole parse —
/// this crate's callers, notably the interactive REPL, already construct
/// a fresh `Parser` per line/buffer, so this never accumulates across an
/// entire session) rather than once per `maybe_expand_alias` call is
/// exactly what closes the gap the per-call `HashSet` can't: it's
/// shared, cumulative state that persists *across* every one of the five
/// sibling loops above, so no matter which one (or which combination)
/// keeps re-entering `parse_command`, the total substitution count is
/// what eventually trips the bound — one guard at the one place every
/// list-operator shape already funnels through
/// ([`Parser::maybe_expand_alias`] itself), rather than five separate,
/// easy-to-miss per-operator fixes.
///
/// 1,000 is chosen to be far beyond what any real, non-adversarial
/// script would ever need (even a script that aliased *every single
/// command* it ever runs, across a very long input) while still being
/// low enough that a genuinely doubling-per-round pattern like
/// `alias c='c|c'` is stopped within a handful of milliseconds, long
/// before memory growth becomes observable at all.
const MAX_ALIAS_EXPANSIONS: u32 = 1000;

impl<'a> Parser<'a> {
    fn new(source: &str, tokens: Vec<Token>, aliases: &'a HashMap<String, String>) -> Self {
        Self {
            source: source.to_string(),
            tokens,
            pos: 0,
            aliases,
            depth: 0,
            alias_expansions: 0,
        }
    }

    // ---- cursor primitives ----------------------------------------------

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.peek().map(|t| &t.kind)
    }

    /// Looks `n` tokens past the current position without consuming
    /// anything (`peek_nth(0)` is equivalent to [`Self::peek`]). See
    /// [`Self::at_posix_function_definition`] for why this exists.
    fn peek_nth(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n)
    }

    fn peek_nth_kind(&self, n: usize) -> Option<&TokenKind> {
        self.peek_nth(n).map(|t| &t.kind)
    }

    fn next(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// POSIX `linebreak`: zero or more `NEWLINE` tokens.
    fn skip_newlines(&mut self) {
        while matches!(self.peek_kind(), Some(TokenKind::Newline)) {
            self.next();
        }
    }

    fn next_word(&mut self) -> Word {
        let tok = self
            .next()
            .expect("caller already confirmed peek() is Word");
        let TokenKind::Word(word) = tok.kind else {
            unreachable!("caller already confirmed peek() is Word")
        };
        word
    }

    // ---- grammar productions --------------------------------------------

    /// POSIX `complete_command`/`list`.
    fn parse_command_list(&mut self) -> Result<CommandList, ParseError> {
        self.skip_newlines();
        let mut items = Vec::new();
        while !self.at_eof() {
            let item = self.parse_command_list_item()?;
            let is_none = matches!(item.separator, Separator::None);
            items.push(item);
            if is_none {
                break;
            }
        }
        if !self.at_eof() {
            return Err(
                self.error_for_unexpected("a separator (';', '&', or newline) or end of input")
            );
        }
        Ok(CommandList { items })
    }

    /// Parses one [`CommandListItem`] — an `and_or` plus whatever
    /// [`Separator`] follows it — shared by [`Self::parse_command_list`]
    /// (the whole top-level program) and [`Self::parse_compound_list`]
    /// (every compound command's own body), which differ only in what
    /// tells them to stop looping for more items.
    ///
    /// Captures the item's own starting byte offset *before* parsing the
    /// `and_or` (via [`Self::peek`], which — since every caller has
    /// already skipped leading newlines/blank separators by this point —
    /// is exactly the first byte of the `and_or` itself, no leading
    /// whitespace) and hands it to [`Self::parse_optional_separator`],
    /// which needs it only in the `&` case (see [`Separator::Async`]'s
    /// docs for why).
    fn parse_command_list_item(&mut self) -> Result<CommandListItem, ParseError> {
        let item_start = self.peek().map_or(self.source.len(), |tok| tok.span.start);
        let and_or = self.parse_and_or()?;
        let separator = self.parse_optional_separator(item_start);
        Ok(CommandListItem { and_or, separator })
    }

    /// POSIX `separator`: `separator_op linebreak | newline_list`. Also
    /// consumes any further blank lines, matching `linebreak`'s
    /// "zero or more" and `newline_list`'s "one or more" both folding
    /// into "skip everything after the first separator token".
    ///
    /// `item_start` is the byte offset [`Self::parse_command_list_item`]
    /// captured just before parsing the `and_or` this separator follows
    /// — needed only for the `&` case, to slice out [`Separator::Async`]'s
    /// own raw source text (`self.source[item_start..amp_start]`, the
    /// same `open.end..close_start` slicing [`Self::parse_subshell`]
    /// already does, just with the `&` token's own start standing in for
    /// a subshell's closing `)`).
    fn parse_optional_separator(&mut self, item_start: usize) -> Separator {
        let separator = match self.peek() {
            Some(tok) if matches!(tok.kind, TokenKind::Operator(Operator::Semi)) => {
                self.next();
                Separator::Sequential
            }
            Some(tok) if matches!(tok.kind, TokenKind::Operator(Operator::Amp)) => {
                let amp_start = tok.span.start;
                self.next();
                // `trim_end` only: `item_start` is already exactly the
                // and_or's first byte (see parse_command_list_item's
                // docs), but `amp_start` is the `&` token's own start,
                // which includes whatever whitespace separates it from
                // the last real token (`sleep 1 &` would otherwise
                // capture a trailing space) -- harmless either way once
                // re-lexed by the child `conch -c` this text is handed
                // to, but trimmed here so the captured source (and any
                // diagnostic/`jobs` display built from it later) reads
                // cleanly.
                Separator::Async(self.source[item_start..amp_start].trim_end().to_string())
            }
            Some(tok) if matches!(tok.kind, TokenKind::Newline) => {
                self.next();
                Separator::Sequential
            }
            _ => Separator::None,
        };
        self.skip_newlines();
        separator
    }

    /// POSIX `and_or`.
    fn parse_and_or(&mut self) -> Result<AndOrList, ParseError> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::Operator(Operator::AndIf)) => LogicalOp::And,
                Some(TokenKind::Operator(Operator::OrIf)) => LogicalOp::Or,
                _ => break,
            };
            self.next();
            self.skip_newlines(); // `AND_IF linebreak pipeline` / `OR_IF linebreak pipeline`
            let pipeline = self.parse_pipeline()?;
            rest.push((op, pipeline));
        }
        Ok(AndOrList { first, rest })
    }

    /// POSIX `pipe_sequence` (the `pipeline : Bang pipe_sequence`
    /// alternative — negation — is out of scope; see [`Pipeline`]'s docs).
    ///
    /// [`Pipeline`]: crate::ast::Pipeline
    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut commands = vec![self.parse_command()?];
        while matches!(self.peek_kind(), Some(TokenKind::Operator(Operator::Pipe))) {
            self.next();
            self.skip_newlines(); // `pipe_sequence '|' linebreak command`
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands })
    }

    /// POSIX `command`: `simple_command | compound_command |
    /// compound_command redirect_list | function_definition` (the last
    /// alternative is a deliberate follow-up — see [`Command`]'s docs).
    ///
    /// This is the one place reserved-word recognition actually happens
    /// (POSIX 2.10.2 rule 1: "if the parser is in any state where only a
    /// reserved word could be the next correct token, proceed as
    /// above") — the start of a fresh `command` is exactly such a state.
    /// Everywhere else a `Word` is consumed (arguments, case patterns,
    /// case subjects, ...), its text is never checked against
    /// [`reserved_word`] at all, which is what correctly makes `echo if`
    /// just echo the word "if" instead of choking on it, and what makes
    /// `echo hi fi` (no separator before the second word) treat `fi` as
    /// a second argument rather than closing an enclosing `if` —
    /// confirmed against real bash for exactly this case.
    ///
    /// Also where [`MAX_COMMAND_DEPTH`]'s guard lives: every mutually
    /// recursive compound-command production funnels back through this
    /// exact function to parse its own nested body (directly, or via
    /// [`Self::parse_compound_list`]/[`Self::parse_pipeline`]/
    /// [`Self::parse_and_or`]), so incrementing/checking/decrementing
    /// [`Self::depth`] once here — rather than separately in each of
    /// [`Self::parse_subshell`], [`Self::parse_if_clause`], etc. — bounds
    /// the whole chain's total nesting with one counter. Deliberately a
    /// thin wrapper around [`Self::parse_command_inner`] (rather than
    /// folding the check directly into that function's body) so the
    /// depth counter is reliably decremented on every return path —
    /// including the many early `?`-propagated errors inside
    /// `parse_command_inner` — without needing a `Drop`-based guard that
    /// would otherwise have to hold `self` borrowed for that whole
    /// function body.
    ///
    /// [`Command`]: crate::ast::Command
    fn parse_command(&mut self) -> Result<Command, ParseError> {
        self.depth += 1;
        if self.depth > MAX_COMMAND_DEPTH {
            let span = self.peek().map_or_else(
                || Span::new(self.source.len(), self.source.len()),
                |tok| tok.span,
            );
            self.depth -= 1;
            return Err(ParseError::NestingTooDeep { span });
        }
        let result = self.parse_command_inner();
        self.depth -= 1;
        result
    }

    /// The actual `command` grammar production — see [`Self::parse_command`]
    /// for why the depth guard lives in a thin wrapper around this rather
    /// than inline here.
    fn parse_command_inner(&mut self) -> Result<Command, ParseError> {
        // Alias expansion (see `crate::parse_with_aliases`'s own docs)
        // happens *before* even the function-definition/subshell/
        // reserved-word checks just below: bash's own model expands
        // aliases textually, as the command is read, before any of that
        // structural recognition even applies — so an alias whose
        // replacement happens to start with `if`/`{`/... needs to be
        // seen as that reserved word/compound command by everything
        // that follows, not treated as a plain command name that merely
        // happens to be spelled "if".
        self.maybe_expand_alias()?;
        // Function definitions are checked *before* everything else in
        // this function, including the subshell/reserved-word compound-
        // command dispatch just below — both function-definition forms
        // are otherwise ambiguous with it: the bash `function` keyword
        // form starts with a reserved word the same way `if`/`for`/...
        // do, and the POSIX `fname()` form starts with an ordinary
        // `Word`, which `parse_simple_command` would otherwise happily
        // swallow as an ordinary command name (with the stray `()`
        // left over to confuse the next parse step — see
        // `at_posix_function_definition`'s docs for why 2-token
        // lookahead is what actually disambiguates this from a plain
        // command).
        if matches!(self.peek_reserved(), Some(ReservedWord::Function)) {
            return Ok(Command::Function(
                self.parse_function_definition_with_keyword()?,
            ));
        }
        if self.at_posix_function_definition() {
            return Ok(Command::Function(self.parse_function_definition_posix()?));
        }
        if matches!(
            self.peek_kind(),
            Some(TokenKind::Operator(Operator::LParen))
        ) {
            return Ok(Command::Compound(self.parse_subshell()?));
        }
        if let Some(reserved) = self.peek_reserved() {
            return match reserved {
                ReservedWord::LBrace => Ok(Command::Compound(self.parse_brace_group()?)),
                ReservedWord::If => Ok(Command::Compound(self.parse_if_clause()?)),
                ReservedWord::For => Ok(Command::Compound(self.parse_for_clause()?)),
                ReservedWord::While => Ok(Command::Compound(self.parse_while_clause()?)),
                ReservedWord::Until => Ok(Command::Compound(self.parse_until_clause()?)),
                ReservedWord::Case => Ok(Command::Compound(self.parse_case_clause()?)),
                // `Function` is fully handled above, before this match is
                // ever reached.
                ReservedWord::Function => unreachable!("handled above"),
                // Every other reserved word (`then`/`elif`/`else`/`fi`/
                // `do`/`done`/`esac`/`in`/`}`) can never legally *start*
                // a fresh command — matching real bash's "syntax error
                // near unexpected token" rather than misinterpreting it
                // as a literal command name.
                ReservedWord::Then
                | ReservedWord::Elif
                | ReservedWord::Else
                | ReservedWord::Fi
                | ReservedWord::Do
                | ReservedWord::Done
                | ReservedWord::Esac
                | ReservedWord::In
                | ReservedWord::RBrace => Err(self.error_for_unexpected("a command")),
            };
        }
        Ok(Command::Simple(self.parse_simple_command()?))
    }

    // ---- function definitions ---------------------------------------------

    /// Whether the parser is positioned at the POSIX `fname '(' ')'`
    /// shape: a plain `Word`, immediately (no intervening tokens, though
    /// blanks are fine — they're never tokens to begin with) followed by
    /// `(` then `)`. This is the one place in the whole grammar that
    /// needs more than one token of lookahead — everywhere else, seeing
    /// the current token alone is enough to know which production
    /// applies. Confirmed against real bash that blanks are tolerated
    /// throughout (`foo ( ) { ...; }` works exactly like `foo() {...}`),
    /// which this gets for free since whitespace was never tokenized in
    /// the first place.
    ///
    /// Deliberately does *not* also require the `Word` to already be a
    /// valid [`is_valid_name`] — [`Self::parse_function_definition_posix`]
    /// calls [`Self::expect_name`] itself once this returns `true`, which
    /// gives a clear "expected a name" error for something like `1x() { :; }`
    /// rather than silently falling through to parsing `1x` as an
    /// ordinary command name with a stray `()` left over.
    fn at_posix_function_definition(&self) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Word(_)))
            && matches!(
                self.peek_nth_kind(1),
                Some(TokenKind::Operator(Operator::LParen))
            )
            && matches!(
                self.peek_nth_kind(2),
                Some(TokenKind::Operator(Operator::RParen))
            )
    }

    /// POSIX `function_definition : fname '(' ')' linebreak function_body`.
    fn parse_function_definition_posix(&mut self) -> Result<FunctionDefinition, ParseError> {
        let name = self.expect_name()?;
        self.expect_operator(Operator::LParen, "'('")?;
        self.expect_operator(Operator::RParen, "')'")?;
        self.skip_newlines();
        let body = self.parse_function_body()?;
        Ok(FunctionDefinition { name, body })
    }

    /// bash extension: `function fname [()] compound-command` — the
    /// parens are optional here (unlike the POSIX form, where they're
    /// mandatory and the only thing that signals "this is a function
    /// definition" at all).
    fn parse_function_definition_with_keyword(&mut self) -> Result<FunctionDefinition, ParseError> {
        self.expect_reserved(ReservedWord::Function)?;
        let name = self.expect_name()?;
        if matches!(
            self.peek_kind(),
            Some(TokenKind::Operator(Operator::LParen))
        ) {
            self.expect_operator(Operator::LParen, "'('")?;
            self.expect_operator(Operator::RParen, "')'")?;
        }
        self.skip_newlines();
        let body = self.parse_function_body()?;
        Ok(FunctionDefinition { name, body })
    }

    /// POSIX `function_body : compound_command | compound_command
    /// redirect_list` — POSIX permits *any* compound command as a
    /// function's body, not just a brace group (confirmed against real
    /// bash: `foo() (echo subshell-body)` is valid, running the body in
    /// a subshell every time `foo` is called). Rule 9 (word expansion
    /// and assignment never apply while parsing a function body) is
    /// already true of this whole parser — it never evaluates anything,
    /// only builds a tree — so there's nothing extra to do for that part
    /// of the rule.
    fn parse_function_body(&mut self) -> Result<CompoundCommand, ParseError> {
        if matches!(
            self.peek_kind(),
            Some(TokenKind::Operator(Operator::LParen))
        ) {
            return self.parse_subshell();
        }
        match self.peek_reserved() {
            Some(ReservedWord::LBrace) => self.parse_brace_group(),
            Some(ReservedWord::If) => self.parse_if_clause(),
            Some(ReservedWord::For) => self.parse_for_clause(),
            Some(ReservedWord::While) => self.parse_while_clause(),
            Some(ReservedWord::Until) => self.parse_until_clause(),
            Some(ReservedWord::Case) => self.parse_case_clause(),
            _ => Err(self.error_for_unexpected("a compound command (a function's body)")),
        }
    }

    // ---- compound commands (POSIX 2.10.2's `compound_command`) ----------

    /// POSIX `brace_group : Lbrace compound_list Rbrace`.
    fn parse_brace_group(&mut self) -> Result<CompoundCommand, ParseError> {
        self.expect_reserved(ReservedWord::LBrace)?;
        let body = self.parse_compound_list()?;
        self.expect_reserved(ReservedWord::RBrace)?;
        Ok(CompoundCommand {
            kind: CompoundCommandKind::BraceGroup(body),
            redirects: self.parse_redirect_list()?,
        })
    }

    /// POSIX `subshell : '(' compound_list ')'`. `(`/`)` are already
    /// `Operator` tokens (POSIX 2.10.1), not reserved words.
    fn parse_subshell(&mut self) -> Result<CompoundCommand, ParseError> {
        let open = self.expect_operator_span(Operator::LParen, "'('")?;
        let body = self.parse_compound_list()?;
        // Captured *before* consuming the ')', via peek — expect_operator_span
        // would consume it first, which is one byte too late to use its
        // span as the body's end offset.
        let close_start = match self.peek() {
            Some(tok) if matches!(tok.kind, TokenKind::Operator(Operator::RParen)) => {
                tok.span.start
            }
            _ => return Err(self.error_for_unexpected("')'")),
        };
        self.next(); // ')'
        let source = self.source[open.end..close_start].to_string();
        Ok(CompoundCommand {
            kind: CompoundCommandKind::Subshell(SubshellBody { body, source }),
            redirects: self.parse_redirect_list()?,
        })
    }

    /// POSIX `if_clause`/`else_part` — see [`IfClause`]'s docs for the
    /// full grammar and how `elif` is flattened.
    ///
    /// [`IfClause`]: crate::ast::IfClause
    fn parse_if_clause(&mut self) -> Result<CompoundCommand, ParseError> {
        self.expect_reserved(ReservedWord::If)?;
        let mut branches = Vec::new();
        let condition = self.parse_compound_list()?;
        self.expect_reserved(ReservedWord::Then)?;
        let body = self.parse_compound_list()?;
        branches.push((condition, body));

        let mut else_branch = None;
        loop {
            match self.peek_reserved() {
                Some(ReservedWord::Elif) => {
                    self.next();
                    let condition = self.parse_compound_list()?;
                    self.expect_reserved(ReservedWord::Then)?;
                    let body = self.parse_compound_list()?;
                    branches.push((condition, body));
                }
                Some(ReservedWord::Else) => {
                    self.next();
                    else_branch = Some(self.parse_compound_list()?);
                    break;
                }
                _ => break,
            }
        }
        self.expect_reserved(ReservedWord::Fi)?;
        Ok(CompoundCommand {
            kind: CompoundCommandKind::If(IfClause {
                branches,
                else_branch,
            }),
            redirects: self.parse_redirect_list()?,
        })
    }

    /// POSIX `while_clause : While compound_list do_group`.
    fn parse_while_clause(&mut self) -> Result<CompoundCommand, ParseError> {
        self.expect_reserved(ReservedWord::While)?;
        let condition = self.parse_compound_list()?;
        let body = self.parse_do_group()?;
        Ok(CompoundCommand {
            kind: CompoundCommandKind::While(WhileClause { condition, body }),
            redirects: self.parse_redirect_list()?,
        })
    }

    /// POSIX `until_clause : Until compound_list do_group`.
    fn parse_until_clause(&mut self) -> Result<CompoundCommand, ParseError> {
        self.expect_reserved(ReservedWord::Until)?;
        let condition = self.parse_compound_list()?;
        let body = self.parse_do_group()?;
        Ok(CompoundCommand {
            kind: CompoundCommandKind::Until(UntilClause { condition, body }),
            redirects: self.parse_redirect_list()?,
        })
    }

    /// POSIX `do_group : Do compound_list Done`.
    fn parse_do_group(&mut self) -> Result<CommandList, ParseError> {
        self.expect_reserved(ReservedWord::Do)?;
        let body = self.parse_compound_list()?;
        self.expect_reserved(ReservedWord::Done)?;
        Ok(body)
    }

    /// POSIX `for_clause` — see [`ForClause`]'s docs for the full
    /// grammar and the `words: None` vs `Some(vec![])` distinction.
    ///
    /// [`ForClause`]: crate::ast::ForClause
    fn parse_for_clause(&mut self) -> Result<CompoundCommand, ParseError> {
        self.expect_reserved(ReservedWord::For)?;
        let name = self.expect_name()?;
        // POSIX rule 1: right after `name`, only a reserved word (`in`
        // or `do`) could be the next correct token, so both are
        // recognized here regardless of any preceding separator —
        // confirmed against real bash that `for x do echo $x; done`
        // (no `in` clause, no `;`/newline before `do`) is valid.
        self.skip_newlines();
        let words = if matches!(self.peek_reserved(), Some(ReservedWord::In)) {
            self.next();
            // Ordinary WORD tokens are consumed here with *no* further
            // reserved-word reinterpretation — confirmed against real
            // bash that a bare `do` immediately after wordlist items,
            // with no intervening `;`/newline, is swallowed as *another*
            // wordlist item rather than closing the clause: `for x in a
            // b do ...; done` is a syntax error, while `for x in a b;
            // do ...; done` succeeds. Only *after* a `sequential_sep` is
            // `do` back in the reserved-word-eligible position rule 1
            // describes.
            let mut words = Vec::new();
            while matches!(self.peek_kind(), Some(TokenKind::Word(_))) {
                words.push(self.next_word());
            }
            self.expect_sequential_sep()?;
            Some(words)
        } else {
            self.skip_optional_sequential_sep();
            None
        };
        let body = self.parse_do_group()?;
        Ok(CompoundCommand {
            kind: CompoundCommandKind::For(ForClause { name, words, body }),
            redirects: self.parse_redirect_list()?,
        })
    }

    /// POSIX `case_clause` — see [`CaseClause`]'s docs for the full
    /// grammar.
    ///
    /// [`CaseClause`]: crate::ast::CaseClause
    fn parse_case_clause(&mut self) -> Result<CompoundCommand, ParseError> {
        self.expect_reserved(ReservedWord::Case)?;
        let word = self.expect_word("a word to match against")?;
        self.skip_newlines();
        self.expect_reserved(ReservedWord::In)?;
        self.skip_newlines();
        let mut arms = Vec::new();
        while !matches!(self.peek_reserved(), Some(ReservedWord::Esac)) {
            arms.push(self.parse_case_arm()?);
        }
        self.next(); // Esac
        Ok(CompoundCommand {
            kind: CompoundCommandKind::Case(CaseClause { word, arms }),
            redirects: self.parse_redirect_list()?,
        })
    }

    /// POSIX `pattern_list ')' compound_list terminator` — one
    /// [`CaseArm`].
    ///
    /// [`CaseArm`]: crate::ast::CaseArm
    fn parse_case_arm(&mut self) -> Result<CaseArm, ParseError> {
        // POSIX `pattern_list : '(' WORD | ...` — an optional leading
        // '(' with no meaning of its own; see CaseArm::patterns' docs.
        if matches!(
            self.peek_kind(),
            Some(TokenKind::Operator(Operator::LParen))
        ) {
            self.next();
        }
        let mut patterns = vec![self.expect_word("a case pattern")?];
        while matches!(self.peek_kind(), Some(TokenKind::Operator(Operator::Pipe))) {
            self.next();
            patterns.push(self.expect_word("a case pattern")?);
        }
        self.expect_operator(Operator::RParen, "')'")?;
        self.skip_newlines();
        let body = self.parse_compound_list()?;
        let terminator = if matches!(self.peek_kind(), Some(TokenKind::Operator(Operator::DSemi))) {
            self.next();
            self.skip_newlines();
            CaseTerminator::Break
        } else if matches!(self.peek_reserved(), Some(ReservedWord::Esac)) {
            CaseTerminator::None
        } else {
            return Err(self.error_for_unexpected("';;' or 'esac'"));
        };
        Ok(CaseArm {
            patterns,
            body,
            terminator,
        })
    }

    // ---- shared compound-command plumbing --------------------------------

    /// POSIX `compound_list : linebreak term | linebreak term separator`
    /// — the body of every compound command. Structurally the same
    /// left-recursive `and_or` chain [`Self::parse_command_list`] parses
    /// for the whole input; the only difference is where it stops:
    /// [`Self::parse_command_list`] requires EOF, this stops at whichever
    /// token would end the *enclosing* compound command (a closing
    /// reserved word, `)`, or `;;`) — see [`Self::at_compound_list_end`].
    fn parse_compound_list(&mut self) -> Result<CommandList, ParseError> {
        self.skip_newlines();
        let mut items = Vec::new();
        while !self.at_compound_list_end() {
            let item = self.parse_command_list_item()?;
            let is_none = matches!(item.separator, Separator::None);
            items.push(item);
            if is_none {
                break;
            }
        }
        Ok(CommandList { items })
    }

    /// Whether the current position is where a [`Self::parse_compound_list`]
    /// must stop — end of input, a closing reserved word
    /// (`then`/`elif`/`else`/`fi`/`do`/`done`/`esac`/`}`), the subshell
    /// closer `)`, or a case arm's `;;` terminator. Every one of these is
    /// a token [`Self::parse_and_or`] itself could never start a command
    /// with, so checking for them up front (rather than, say, trying to
    /// parse a command and reacting to failure) is unambiguous.
    fn at_compound_list_end(&mut self) -> bool {
        if self.at_eof() {
            return true;
        }
        if matches!(
            self.peek_kind(),
            Some(TokenKind::Operator(Operator::RParen | Operator::DSemi))
        ) {
            return true;
        }
        matches!(
            self.peek_reserved(),
            Some(
                ReservedWord::Then
                    | ReservedWord::Elif
                    | ReservedWord::Else
                    | ReservedWord::Fi
                    | ReservedWord::Do
                    | ReservedWord::Done
                    | ReservedWord::Esac
                    | ReservedWord::RBrace
            )
        )
    }

    /// Every redirection trailing a compound command (POSIX `command :
    /// compound_command redirect_list`) — shares [`Self::parse_redirect`]
    /// with [`Self::parse_simple_command`]'s prefix/suffix redirects.
    fn parse_redirect_list(&mut self) -> Result<Vec<Redirect>, ParseError> {
        let mut redirects = Vec::new();
        loop {
            match self.peek_kind() {
                Some(TokenKind::IoNumber(_)) => redirects.push(self.parse_redirect()?),
                Some(TokenKind::Operator(op)) if is_redirect_operator(*op) => {
                    redirects.push(self.parse_redirect()?);
                }
                _ => break,
            }
        }
        Ok(redirects)
    }

    /// POSIX `sequential_sep : ';' linebreak | newline_list` — required
    /// in some grammar positions (see [`Self::expect_sequential_sep`]),
    /// optional in others (this one).
    fn skip_optional_sequential_sep(&mut self) {
        if matches!(self.peek_kind(), Some(TokenKind::Operator(Operator::Semi))) {
            self.next();
        }
        self.skip_newlines();
    }

    /// Like [`Self::skip_optional_sequential_sep`], but errors if no
    /// `;`/newline is present at all.
    fn expect_sequential_sep(&mut self) -> Result<(), ParseError> {
        match self.peek_kind() {
            Some(TokenKind::Operator(Operator::Semi)) => {
                self.next();
                self.skip_newlines();
                Ok(())
            }
            Some(TokenKind::Newline) => {
                self.skip_newlines();
                Ok(())
            }
            _ => Err(self.error_for_unexpected("';' or a newline")),
        }
    }

    /// Consumes the current token, requiring it to be a plain `WORD`
    /// (regardless of its text — never reserved-word-checked; see
    /// [`Self::parse_command`]'s docs for why that's correct here).
    fn expect_word(&mut self, what: &str) -> Result<Word, ParseError> {
        match self.peek_kind() {
            Some(TokenKind::Word(_)) => Ok(self.next_word()),
            _ => Err(self.error_for_unexpected(what)),
        }
    }

    /// Consumes the current token, requiring it to be exactly the given
    /// [`Operator`].
    fn expect_operator(&mut self, want: Operator, what: &str) -> Result<(), ParseError> {
        self.expect_operator_span(want, what).map(|_| ())
    }

    /// Like [`Self::expect_operator`], but also returns the consumed
    /// token's [`Span`] — needed wherever a caller must slice
    /// [`Self::source`] by byte offset (currently: only
    /// [`Self::parse_subshell`]).
    fn expect_operator_span(&mut self, want: Operator, what: &str) -> Result<Span, ParseError> {
        match self.peek() {
            Some(tok) if matches!(tok.kind, TokenKind::Operator(op) if op == want) => {
                let span = tok.span;
                self.next();
                Ok(span)
            }
            _ => Err(self.error_for_unexpected(what)),
        }
    }

    /// POSIX `name` (rule 5, XBD 3.216 "Name") — consumes the current
    /// token, requiring it to be a plain, unquoted literal word whose
    /// entire text is a valid shell name (`[A-Za-z_][A-Za-z0-9_]*`).
    fn expect_name(&mut self) -> Result<String, ParseError> {
        let valid_name = match self.peek_kind() {
            Some(TokenKind::Word(word)) => word
                .as_plain_literal()
                .filter(|text| is_valid_name(text))
                .map(str::to_string),
            _ => None,
        };
        match valid_name {
            Some(name) => {
                self.next();
                Ok(name)
            }
            None => Err(self.error_for_unexpected("a name")),
        }
    }

    /// If the current token is a plain, unquoted `Word` whose text is
    /// exactly one of the POSIX reserved words this parser recognizes,
    /// returns it — without consuming anything. See [`reserved_word`].
    fn peek_reserved(&mut self) -> Option<ReservedWord> {
        match self.peek_kind() {
            Some(TokenKind::Word(word)) => reserved_word(word),
            _ => None,
        }
    }

    /// [`Self::parse_command`]'s alias-expansion hook — see
    /// [`crate::parse_with_aliases`]'s own docs for the full grounding.
    /// Repeatedly checks whether the current token is a plain, unquoted
    /// `Word` ([`Word::as_plain_literal`], the exact same quote-aware
    /// check [`reserved_word`] already relies on) matching a known
    /// alias; if so, re-lexes that alias's replacement text and splices
    /// the resulting tokens in place of the one aliased word (so a
    /// replacement introducing a real operator, e.g. `alias ll='ls -la |
    /// less'`'s `|`, becomes a genuine pipe, not literal text), then
    /// checks again in case the *new* current token is itself an alias
    /// (chained aliases, `alias ll='la -l'; alias la='ls -a'`).
    ///
    /// Guards against infinite self-reference (`alias ls='ls -la'`) with
    /// a set of names already expanded *within this one call* — the
    /// moment a name would be expanded a second time in the same chain,
    /// expansion stops there and that occurrence is left as a plain,
    /// literal command name for the rest of parsing to handle normally
    /// (exactly matching real bash's own observed behavior for this
    /// classic case).
    ///
    /// Known gap, not implemented: bash also re-checks the word
    /// *following* an expansion whose replacement text ends in a blank
    /// (the classic `alias sudo='sudo '` trick, letting `sudo ll` expand
    /// both words) — a real, bash-documented behavior, but a narrower,
    /// more advanced one than plain single-word aliasing, and left as a
    /// documented gap given this phase's own scope/time constraints
    /// rather than attempted partially.
    ///
    /// Also enforces [`MAX_ALIAS_EXPANSIONS`] — see that constant's own
    /// docs for why a per-*call* guard (the `expanding` set below) is
    /// necessary but not sufficient on its own, and why the additional
    /// bound has to live here rather than in any one of this function's
    /// several callers.
    fn maybe_expand_alias(&mut self) -> Result<(), ParseError> {
        let mut expanding: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let Some(TokenKind::Word(word)) = self.peek_kind() else {
                return Ok(());
            };
            let Some(name) = word.as_plain_literal() else {
                return Ok(());
            };
            let Some(replacement) = self.aliases.get(name) else {
                return Ok(());
            };
            let name = name.to_string();
            let replacement = replacement.clone();
            if !expanding.insert(name) {
                return Ok(());
            }
            self.alias_expansions += 1;
            if self.alias_expansions > MAX_ALIAS_EXPANSIONS {
                let span = self.peek().map_or_else(
                    || Span::new(self.source.len(), self.source.len()),
                    |tok| tok.span,
                );
                return Err(ParseError::TooManyAliasExpansions { span });
            }
            let new_tokens = conch_shell_lexer::lex(&replacement)?;
            self.splice_alias_tokens(&replacement, new_tokens);
        }
    }

    /// Splices `new_tokens` (freshly re-lexed from `replacement`, so
    /// every one's `Span` is 0-based — relative to `replacement` alone,
    /// not `self.source`) in place of the single alias-name token
    /// currently at `self.pos`, while also physically rewriting
    /// `self.source` itself so that *every* token's `Span` — the
    /// newly-spliced ones, and every token already positioned after
    /// them — stays genuinely `self.source`-relative afterward, exactly
    /// as if the user had originally typed the replacement text right
    /// there instead of the alias name.
    ///
    /// # Why this exists
    ///
    /// Before this function existed, alias expansion only ever spliced
    /// into [`Self::tokens`], never touching [`Self::source`] at all —
    /// so a spliced-in token's `Span` stayed relative to `replacement`'s
    /// own coordinate space (starting at byte 0 of *that* string), while
    /// [`Self::source`] remained the original, unexpanded input. Any
    /// later code that slices `self.source` using such a span —
    /// [`Self::parse_optional_separator`]'s `&`/async case,
    /// [`Self::parse_subshell`]'s verbatim-body capture — was mixing two
    /// completely different strings' byte offsets. Confirmed two
    /// distinct, real symptoms from this (this project's own fuzzing
    /// pass): a panic (`byte range starts at 2 but ends at 1`, or an
    /// out-of-bounds start index, depending on the exact replacement
    /// text's length relative to the alias name it replaced) when the
    /// mismatched offsets happened to be invalid for `self.source`'s
    /// actual length, and — more insidiously — *silent, wrong* captured
    /// text when they happened to both be in-bounds by coincidence: a
    /// backgrounded aliased command (`alias a='echo hi'; a &`) captured
    /// `Separator::Async`'s re-exec source text as the literal
    /// pre-expansion name `"a"` instead of anything describing what
    /// actually runs (`echo hi`) — directly observable wrong behavior,
    /// not just a theoretical panic risk, since that captured text is
    /// what the backgrounded job's own child process actually executes.
    ///
    /// # Why rewriting `self.source` (rather than tagging each token
    /// with its own provenance) is the right fix here
    ///
    /// The alternative — giving every [`Token`]/[`Span`] a marker for
    /// *which* string it was lexed from, and having every span-based
    /// slice site check it before trusting `self.source[a..b]` — would
    /// work too, but touches a foundational, widely-shared type
    /// (`conch_shell_lexer::Span`) used throughout this crate and
    /// `conch-shell-lexer`'s own public API, and still leaves every
    /// *future* span-based slice site needing to remember to check that
    /// marker itself. Rewriting `self.source` in place instead restores
    /// the simpler, original invariant ("every token's span is genuinely
    /// `self.source`-relative") globally, for free, for every existing
    /// *and* future slice site — no marker, no per-call-site check, and
    /// no risk of a future addition forgetting one.
    fn splice_alias_tokens(&mut self, replacement: &str, new_tokens: Vec<Token>) {
        let old_span = self.tokens[self.pos].span;
        self.source
            .replace_range(old_span.start..old_span.end, replacement);
        // How much longer (positive) or shorter (negative) `replacement`
        // is than the alias-name text it's replacing -- every existing
        // token positioned after the replaced region shifts by exactly
        // this many bytes now that `self.source` has been rewritten.
        let delta = replacement.len() as isize - (old_span.end - old_span.start) as isize;
        let offset_new_tokens: Vec<Token> = new_tokens
            .into_iter()
            .map(|mut tok| {
                // `new_tokens` was lexed from `replacement` in isolation,
                // so its spans start at 0; `replacement` itself now
                // begins at `old_span.start` within the rewritten
                // `self.source`, so simple, unsigned offsetting (never
                // negative) is all that's needed here.
                tok.span.start += old_span.start;
                tok.span.end += old_span.start;
                tok
            })
            .collect();
        for tok in &mut self.tokens[self.pos + 1..] {
            tok.span.start = tok.span.start.wrapping_add_signed(delta);
            tok.span.end = tok.span.end.wrapping_add_signed(delta);
        }
        self.tokens
            .splice(self.pos..self.pos + 1, offset_new_tokens);
    }

    /// Consumes the current token, requiring [`Self::peek_reserved`] to
    /// be exactly `want`.
    fn expect_reserved(&mut self, want: ReservedWord) -> Result<(), ParseError> {
        if self.peek_reserved() == Some(want) {
            self.next();
            Ok(())
        } else {
            Err(self.error_for_unexpected(reserved_word_text(want)))
        }
    }

    /// POSIX `simple_command`.
    fn parse_simple_command(&mut self) -> Result<SimpleCommand, ParseError> {
        let mut assignments = Vec::new();
        let mut redirects = Vec::new();

        // cmd_prefix: io_redirect and ASSIGNMENT_WORD, interleaved, in
        // any order, until a plain WORD (the command name) or anything
        // else ends the prefix.
        loop {
            match self.peek_kind() {
                Some(TokenKind::IoNumber(_)) => {
                    redirects.push(self.parse_redirect()?);
                    continue;
                }
                Some(TokenKind::Operator(op)) if is_redirect_operator(*op) => {
                    redirects.push(self.parse_redirect()?);
                    continue;
                }
                _ => {}
            }
            let Some(TokenKind::Word(word)) = self.peek_kind() else {
                break;
            };
            let Some((name_end, value_start, is_append)) = assignment_name_end(word) else {
                break;
            };
            let word = self.next_word();
            assignments.push(split_assignment(word, name_end, value_start, is_append));
        }

        // cmd_name / cmd_word: the command name, if any. Its absence is
        // only valid when at least one assignment/redirect was already
        // consumed above (checked once the whole simple_command is
        // parsed, since a redirect can still follow a name-less command:
        // `>out` alone is valid, as is `FOO=1 >out`).
        let name = match self.peek_kind() {
            Some(TokenKind::Word(_)) => Some(self.next_word()),
            _ => None,
        };

        // cmd_suffix: io_redirect and plain WORD arguments. A word here
        // is *never* reinterpreted as an assignment, even if it has the
        // `NAME=value` shape — POSIX only recognizes ASSIGNMENT_WORD in
        // cmd_prefix position (confirmed against real bash: `echo
        // FOO=bar` prints `FOO=bar` rather than assigning `FOO`).
        let mut args = Vec::new();
        loop {
            match self.peek_kind() {
                Some(TokenKind::IoNumber(_)) => {
                    redirects.push(self.parse_redirect()?);
                }
                Some(TokenKind::Operator(op)) if is_redirect_operator(*op) => {
                    redirects.push(self.parse_redirect()?);
                }
                Some(TokenKind::Word(_)) => {
                    args.push(self.next_word());
                }
                _ => break,
            }
        }

        if name.is_none() && assignments.is_empty() && redirects.is_empty() {
            return Err(self.error_for_unexpected("a command"));
        }

        Ok(SimpleCommand {
            assignments,
            name,
            args,
            redirects,
        })
    }

    /// POSIX `io_redirect`, restricted to the `io_file` operators Phase 1
    /// implements (`<`, `>`, `>>`); see [`RedirectOperator`]'s docs for
    /// why the rest give a clear "not yet supported" error instead of a
    /// separate AST representation.
    ///
    /// [`RedirectOperator`]: crate::ast::RedirectOperator
    fn parse_redirect(&mut self) -> Result<Redirect, ParseError> {
        let fd = match self.peek_kind() {
            Some(TokenKind::IoNumber(n)) => {
                let n = *n;
                self.next();
                Some(n)
            }
            _ => None,
        };
        let operator = match self.peek_kind() {
            Some(TokenKind::Operator(Operator::Less)) => {
                self.next();
                RedirectOperator::Input
            }
            Some(TokenKind::Operator(Operator::Great)) => {
                self.next();
                RedirectOperator::Output
            }
            Some(TokenKind::Operator(Operator::DGreat)) => {
                self.next();
                RedirectOperator::Append
            }
            _ => {
                return Err(self.error_for_unexpected("a redirection operator ('<', '>', or '>>')"));
            }
        };
        let target = match self.peek_kind() {
            Some(TokenKind::Word(_)) => self.next_word(),
            _ => return Err(self.error_for_unexpected("a filename")),
        };
        Ok(Redirect {
            fd,
            operator,
            target,
        })
    }

    // ---- error construction -----------------------------------------------

    /// Builds a [`ParseError`] for "the current token isn't valid here",
    /// upgrading to [`ParseError::UnsupportedConstruct`] when the current
    /// token is a real, lexer-recognized POSIX/bash operator that's just
    /// not implemented in this phase (e.g. `<<`, `(`) — see that variant's
    /// docs for why that distinction matters.
    fn error_for_unexpected(&mut self, expected: &str) -> ParseError {
        match self.peek() {
            Some(tok) => {
                if let TokenKind::Operator(op) = tok.kind
                    && let Some(message) = describe_deferred_operator(op)
                {
                    return ParseError::UnsupportedConstruct {
                        message: message.to_string(),
                        span: tok.span,
                    };
                }
                ParseError::UnexpectedToken {
                    found: describe_token_kind(&tok.kind),
                    span: tok.span,
                    expected: expected.to_string(),
                }
            }
            None => ParseError::UnexpectedEof {
                expected: expected.to_string(),
            },
        }
    }
}

fn is_redirect_operator(op: Operator) -> bool {
    matches!(op, Operator::Less | Operator::Great | Operator::DGreat)
}

/// The POSIX 2.4 reserved words this parser recognizes, plus one bash
/// extension (`function`, for `function fname [()] compound-command` —
/// see [`Parser::parse_function_definition_with_keyword`]). `!`
/// (pipeline negation) and bash's other extensions (`[[`, `]]`,
/// `select`) are still deliberately excluded, matching how `!` was
/// already out of scope for [`Pipeline`] before this phase.
///
/// [`Pipeline`]: crate::ast::Pipeline
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservedWord {
    If,
    Then,
    Elif,
    Else,
    Fi,
    For,
    While,
    Until,
    Do,
    Done,
    Case,
    Esac,
    In,
    LBrace,
    RBrace,
    Function,
}

/// Matches `word` against the reserved-word set — POSIX 2.10.2 rule 1:
/// "When the TOKEN is exactly a reserved word, the token identifier for
/// that reserved word shall result." [`Word::as_plain_literal`] is what
/// gives this the "exactly" and "TOKEN" parts of that rule for free: it
/// only returns `Some` for a word that is a single unquoted literal
/// segment, so a quoted `'if'`/`"if"` (POSIX: "quoting characters...
/// retained in the token[, so] quoted strings cannot be recognized as
/// reserved words") or a word with any other content glued on (`ifx`,
/// `{a,b}`) correctly never matches.
///
/// This function alone does *not* decide whether reserved-word
/// recognition should even be attempted at the current parser position —
/// see [`Parser::parse_command`]'s docs for that half of rule 1.
fn reserved_word(word: &Word) -> Option<ReservedWord> {
    match word.as_plain_literal()? {
        "if" => Some(ReservedWord::If),
        "then" => Some(ReservedWord::Then),
        "elif" => Some(ReservedWord::Elif),
        "else" => Some(ReservedWord::Else),
        "fi" => Some(ReservedWord::Fi),
        "for" => Some(ReservedWord::For),
        "while" => Some(ReservedWord::While),
        "until" => Some(ReservedWord::Until),
        "do" => Some(ReservedWord::Do),
        "done" => Some(ReservedWord::Done),
        "case" => Some(ReservedWord::Case),
        "esac" => Some(ReservedWord::Esac),
        "in" => Some(ReservedWord::In),
        "{" => Some(ReservedWord::LBrace),
        "}" => Some(ReservedWord::RBrace),
        "function" => Some(ReservedWord::Function),
        _ => None,
    }
}

fn reserved_word_text(word: ReservedWord) -> &'static str {
    match word {
        ReservedWord::If => "'if'",
        ReservedWord::Then => "'then'",
        ReservedWord::Elif => "'elif'",
        ReservedWord::Else => "'else'",
        ReservedWord::Fi => "'fi'",
        ReservedWord::For => "'for'",
        ReservedWord::While => "'while'",
        ReservedWord::Until => "'until'",
        ReservedWord::Do => "'do'",
        ReservedWord::Done => "'done'",
        ReservedWord::Case => "'case'",
        ReservedWord::Esac => "'esac'",
        ReservedWord::In => "'in'",
        ReservedWord::LBrace => "'{'",
        ReservedWord::RBrace => "'}'",
        ReservedWord::Function => "'function'",
    }
}

/// POSIX `name` (rule 5, XBD 3.216 "Name"): starts with a letter or
/// underscore, followed by any number of letters, digits, or
/// underscores. Used to validate `for`'s loop variable at parse time.
fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Operators `conch-shell-lexer` tokenizes as real POSIX/bash grammar
/// that this parser doesn't implement yet. `DSemi`/`LParen`/`RParen` are
/// deliberately *not* listed here any more now that case statements and
/// subshells are implemented (Phase 3) — a `)`/`;;` in some other,
/// genuinely invalid position should get an ordinary "unexpected token"
/// message, not a stale "not yet supported" one.
fn describe_deferred_operator(op: Operator) -> Option<&'static str> {
    match op {
        Operator::DLess => Some("'<<' (here-document) is not yet supported"),
        Operator::DLessDash => {
            Some("'<<-' (here-document with leading-tab stripping) is not yet supported")
        }
        Operator::LessAnd => Some("'<&' (fd-duplicating input redirection) is not yet supported"),
        Operator::GreatAnd => Some("'>&' (fd-duplicating output redirection) is not yet supported"),
        Operator::LessGreat => Some("'<>' (open for read+write) is not yet supported"),
        Operator::Clobber => {
            Some("'>|' (clobber-override output redirection) is not yet supported")
        }
        Operator::Pipe
        | Operator::OrIf
        | Operator::Amp
        | Operator::AndIf
        | Operator::Semi
        | Operator::DSemi
        | Operator::Less
        | Operator::Great
        | Operator::DGreat
        | Operator::LParen
        | Operator::RParen => None,
    }
}

fn describe_token_kind(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Word(w) => match w.as_plain_literal() {
            Some(text) => format!("word {text:?}"),
            None => "a quoted/expansion word".to_string(),
        },
        TokenKind::IoNumber(n) => format!("IO number '{n}'"),
        TokenKind::Operator(op) => format!("operator '{}'", operator_text(*op)),
        TokenKind::Newline => "newline".to_string(),
    }
}

fn operator_text(op: Operator) -> &'static str {
    match op {
        Operator::Pipe => "|",
        Operator::OrIf => "||",
        Operator::Amp => "&",
        Operator::AndIf => "&&",
        Operator::Semi => ";",
        Operator::DSemi => ";;",
        Operator::Less => "<",
        Operator::Great => ">",
        Operator::DLess => "<<",
        Operator::DLessDash => "<<-",
        Operator::LessAnd => "<&",
        Operator::GreatAnd => ">&",
        Operator::DGreat => ">>",
        Operator::LessGreat => "<>",
        Operator::Clobber => ">|",
        Operator::LParen => "(",
        Operator::RParen => ")",
    }
}

/// If `word` has the `ASSIGNMENT_WORD` shape (POSIX 2.10.1: an unquoted,
/// literal `Name` immediately followed by an unquoted, literal `=`, as
/// the word's *actual typed characters* — not merely after quote
/// removal) **or** the bash-extension `Name+=` append-assignment shape
/// (confirmed against real bash: `x=foo; x+=bar` leaves `x` as
/// `foobar`, not `bar` — see [`Assignment::is_append`]'s own docs),
/// returns `(name_end, value_start, is_append)`: `name_end` is the byte
/// offset within the word's first segment's text right where `Name`
/// ends (i.e. where the `=`/`+=` operator itself begins), `value_start`
/// is the byte offset right after that operator, and `is_append`
/// distinguishes which of the two operators was found.
///
/// Requires the `Name` and operator to fall entirely within a single
/// leading [`WordSegment::Literal`] — i.e. genuinely unquoted — which is
/// why `'FOO'=bar` is correctly rejected (confirmed against real bash:
/// `'FOO'=bar` runs a command literally named `FOO=bar` rather than
/// assigning `FOO`): its first segment is a `SingleQuoted`, not a
/// `Literal`.
fn assignment_name_end(word: &Word) -> Option<(usize, usize, bool)> {
    let Some(WordSegment::Literal(text)) = word.segments.first() else {
        return None;
    };
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if first != '_' && !first.is_ascii_alphabetic() {
        return None;
    }
    for (idx, c) in chars {
        if c == '=' {
            return Some((idx, idx + 1, false));
        }
        if c == '+' && text.as_bytes().get(idx + 1) == Some(&b'=') {
            return Some((idx, idx + 2, true));
        }
        if c != '_' && !c.is_ascii_alphanumeric() {
            return None;
        }
    }
    None
}

/// Splits `word` at the `(name_end, value_start, is_append)` produced by
/// [`assignment_name_end`] into an [`Assignment`].
fn split_assignment(
    mut word: Word,
    name_end: usize,
    value_start: usize,
    is_append: bool,
) -> Assignment {
    let first_text = match &word.segments[0] {
        WordSegment::Literal(text) => text.clone(),
        _ => unreachable!("assignment_name_end only returns Some for a leading Literal segment"),
    };
    let name = first_text[..name_end].to_string();
    let remainder = first_text[value_start..].to_string();
    if remainder.is_empty() {
        word.segments.remove(0);
    } else {
        word.segments[0] = WordSegment::Literal(remainder);
    }
    Assignment {
        name,
        value: word,
        is_append,
    }
}
