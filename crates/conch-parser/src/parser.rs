//! The recursive-descent parser implementation, one function per
//! POSIX 2.10.2 grammar production it implements.

use conch_shell_lexer::{Operator, Span, Token, TokenKind, Word, WordSegment};

use crate::ast::{
    AndOrList, Assignment, CaseArm, CaseClause, CaseTerminator, Command, CommandList,
    CommandListItem, CompoundCommand, CompoundCommandKind, ForClause, FunctionDefinition, IfClause,
    LogicalOp, Pipeline, Redirect, RedirectOperator, Separator, SimpleCommand, SubshellBody,
    UntilClause, WhileClause,
};
use crate::error::ParseError;

/// Parses a complete `input` into a [`CommandList`].
///
/// # Errors
///
/// Returns [`ParseError`] if `input` fails to tokenize (see
/// `conch-shell-lexer`), uses a construct outside Phase 1's grammar
/// (giving [`ParseError::UnsupportedConstruct`] when that construct is at
/// least *recognized*, real POSIX/bash grammar — see that variant's
/// docs), or is otherwise not valid POSIX shell grammar.
pub fn parse(input: &str) -> Result<CommandList, ParseError> {
    let tokens = conch_shell_lexer::lex(input)?;
    Parser::new(input, tokens).parse_command_list()
}

struct Parser<'a> {
    /// The original, un-tokenized input — kept only so a construct whose
    /// *execution* needs its own verbatim source text (today: a
    /// subshell's body — see [`Self::parse_subshell`] and
    /// `conch-shell-core::exec`'s module docs for why a subshell must run
    /// in a genuinely separate process, not by walking a parsed AST
    /// in-process) can slice it out by byte offset, the same way
    /// `conch-shell-lexer` already hands `conch-shell-core` the verbatim
    /// body of a `$(...)`/`` `...` `` command substitution.
    source: &'a str,
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
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, tokens: Vec<Token>) -> Self {
        Self {
            source,
            tokens,
            pos: 0,
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
            let and_or = self.parse_and_or()?;
            let separator = self.parse_optional_separator();
            let is_none = matches!(separator, Separator::None);
            items.push(CommandListItem { and_or, separator });
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

    /// POSIX `separator`: `separator_op linebreak | newline_list`. Also
    /// consumes any further blank lines, matching `linebreak`'s
    /// "zero or more" and `newline_list`'s "one or more" both folding
    /// into "skip everything after the first separator token".
    fn parse_optional_separator(&mut self) -> Separator {
        let separator = match self.peek_kind() {
            Some(TokenKind::Operator(Operator::Semi)) => {
                self.next();
                Separator::Sequential
            }
            Some(TokenKind::Operator(Operator::Amp)) => {
                self.next();
                Separator::Async
            }
            Some(TokenKind::Newline) => {
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
    /// [`Command`]: crate::ast::Command
    fn parse_command(&mut self) -> Result<Command, ParseError> {
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
            let and_or = self.parse_and_or()?;
            let separator = self.parse_optional_separator();
            let is_none = matches!(separator, Separator::None);
            items.push(CommandListItem { and_or, separator });
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
            let Some(eq_end) = assignment_name_end(word) else {
                break;
            };
            let word = self.next_word();
            assignments.push(split_assignment(word, eq_end));
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
/// removal), returns the byte offset within the word's first segment's
/// text of the character right after that `=`.
///
/// Requires the `Name` and `=` to fall entirely within a single leading
/// [`WordSegment::Literal`] — i.e. genuinely unquoted — which is why
/// `'FOO'=bar` is correctly rejected (confirmed against real bash:
/// `'FOO'=bar` runs a command literally named `FOO=bar` rather than
/// assigning `FOO`): its first segment is a `SingleQuoted`, not a
/// `Literal`.
fn assignment_name_end(word: &Word) -> Option<usize> {
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
            return Some(idx + 1);
        }
        if c != '_' && !c.is_ascii_alphanumeric() {
            return None;
        }
    }
    None
}

/// Splits `word` at `eq_end` (a byte offset produced by
/// [`assignment_name_end`]) into an [`Assignment`].
fn split_assignment(mut word: Word, eq_end: usize) -> Assignment {
    let first_text = match &word.segments[0] {
        WordSegment::Literal(text) => text.clone(),
        _ => unreachable!("assignment_name_end only returns Some for a leading Literal segment"),
    };
    let name = first_text[..eq_end - 1].to_string();
    let remainder = first_text[eq_end..].to_string();
    if remainder.is_empty() {
        word.segments.remove(0);
    } else {
        word.segments[0] = WordSegment::Literal(remainder);
    }
    Assignment { name, value: word }
}
