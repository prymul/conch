//! The recursive-descent parser implementation, one function per
//! POSIX 2.10.2 grammar production it implements.

use conch_shell_lexer::{Operator, Token, TokenKind, Word, WordSegment};

use crate::ast::{
    AndOrList, Assignment, Command, CommandList, CommandListItem, LogicalOp, Pipeline, Redirect,
    RedirectOperator, Separator, SimpleCommand,
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
    Parser::new(tokens).parse_command_list()
}

struct Parser {
    tokens: std::iter::Peekable<std::vec::IntoIter<Token>>,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens: tokens.into_iter().peekable(),
        }
    }

    // ---- cursor primitives ----------------------------------------------

    fn peek(&mut self) -> Option<&Token> {
        self.tokens.peek()
    }

    fn peek_kind(&mut self) -> Option<&TokenKind> {
        self.tokens.peek().map(|t| &t.kind)
    }

    fn next(&mut self) -> Option<Token> {
        self.tokens.next()
    }

    fn at_eof(&mut self) -> bool {
        self.tokens.peek().is_none()
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

    /// POSIX `command`. Only the `simple_command` alternative is
    /// implemented in Phase 1 — see [`Command`]'s docs for the rest.
    ///
    /// [`Command`]: crate::ast::Command
    fn parse_command(&mut self) -> Result<Command, ParseError> {
        Ok(Command::Simple(self.parse_simple_command()?))
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

/// Operators `conch-shell-lexer` tokenizes as real POSIX/bash grammar
/// that this parser doesn't implement yet, paired with which later phase
/// is expected to add them.
fn describe_deferred_operator(op: Operator) -> Option<&'static str> {
    match op {
        Operator::DSemi => {
            Some("';;' (case-statement terminator) is not yet supported (planned for Phase 3)")
        }
        Operator::DLess => Some("'<<' (here-document) is not yet supported (planned for Phase 2)"),
        Operator::DLessDash => Some(
            "'<<-' (here-document with leading-tab stripping) is not yet supported (planned for Phase 2)",
        ),
        Operator::LessAnd => Some(
            "'<&' (fd-duplicating input redirection) is not yet supported (planned for Phase 2)",
        ),
        Operator::GreatAnd => Some(
            "'>&' (fd-duplicating output redirection) is not yet supported (planned for Phase 2)",
        ),
        Operator::LessGreat => {
            Some("'<>' (open for read+write) is not yet supported (planned for Phase 2)")
        }
        Operator::Clobber => Some(
            "'>|' (clobber-override output redirection) is not yet supported (planned for Phase 2)",
        ),
        Operator::LParen => Some("'(' (subshell) is not yet supported (planned for Phase 3)"),
        Operator::RParen => Some("')' (subshell) is not yet supported (planned for Phase 3)"),
        Operator::Pipe
        | Operator::OrIf
        | Operator::Amp
        | Operator::AndIf
        | Operator::Semi
        | Operator::Less
        | Operator::Great
        | Operator::DGreat => None,
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
