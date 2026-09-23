//! The tokenizer implementation.

use crate::error::LexError;
use crate::token::{
    CommandSubstitution, Operator, Parameter, Span, SpecialParameter, SubstitutionStyle, Token,
    TokenKind, Word, WordSegment,
};

/// Tokenizes `input` into a flat token stream.
///
/// This is the primary entry point of the crate; see the crate-level docs
/// for the overall contract.
///
/// # Errors
///
/// Returns [`LexError`] if `input` contains an unterminated quote,
/// parameter expansion, command substitution, arithmetic expansion, or a
/// trailing unescaped backslash with nothing left to escape.
pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(input).tokenize()
}

/// A streaming tokenizer over a `&str`.
///
/// Most callers should prefer the [`lex`] convenience function; this type
/// is exposed directly for callers that want to drive tokenization
/// themselves (e.g. an interactive REPL detecting "this input is
/// incomplete, show a continuation prompt").
pub struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

/// Which nested-inside-an-expansion boundary scan is running, purely to
/// select the right [`LexError`] variant if it never finds its terminator.
#[derive(Clone, Copy)]
enum BalanceContext {
    CommandSubstitution,
    Arithmetic,
}

impl BalanceContext {
    fn unterminated_error(self, start: usize) -> LexError {
        match self {
            BalanceContext::CommandSubstitution => {
                LexError::UnterminatedCommandSubstitution { start }
            }
            BalanceContext::Arithmetic => LexError::UnterminatedArithmeticExpansion { start },
        }
    }
}

impl<'a> Lexer<'a> {
    #[must_use]
    pub fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    /// Consumes the lexer, producing the full token stream.
    ///
    /// # Errors
    ///
    /// See [`lex`].
    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_blanks();
            let start = self.pos;
            match self.peek() {
                None => break,
                Some('\n') => {
                    self.bump();
                    tokens.push(Token::new(TokenKind::Newline, Span::new(start, self.pos)));
                }
                Some('#') => {
                    // POSIX 2.10.1: a '#' at the start of a word begins a
                    // comment extending to (but not including) the next
                    // newline. We are always at word-start here, since
                    // skip_blanks() just ran and every other branch of
                    // this loop consumes a complete token before looping
                    // back around.
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(c) if is_operator_char(c) => {
                    let op = self.lex_operator();
                    tokens.push(Token::new(
                        TokenKind::Operator(op),
                        Span::new(start, self.pos),
                    ));
                }
                Some(c) if c.is_ascii_digit() => {
                    if let Some(n) = self.try_lex_io_number() {
                        tokens.push(Token::new(
                            TokenKind::IoNumber(n),
                            Span::new(start, self.pos),
                        ));
                    } else {
                        let word = self.scan_word()?;
                        tokens.push(Token::new(
                            TokenKind::Word(word),
                            Span::new(start, self.pos),
                        ));
                    }
                }
                Some(_) => {
                    let word = self.scan_word()?;
                    tokens.push(Token::new(
                        TokenKind::Word(word),
                        Span::new(start, self.pos),
                    ));
                }
            }
        }
        Ok(tokens)
    }

    // ---- cursor primitives ----------------------------------------------

    /// The remaining unconsumed input. Note this borrows from `self.input`
    /// (lifetime `'a`), not from `&self`, so callers can read it and then
    /// separately mutate `self.pos` without a borrow conflict.
    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek2(&self) -> Option<char> {
        self.rest().chars().nth(1)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.bump();
        }
    }

    // ---- operators / IO_NUMBER --------------------------------------------

    /// Lexes one operator token starting at the current position by
    /// longest match, per POSIX 2.10.1. Panics if not positioned at an
    /// operator character; callers only call this after checking
    /// [`is_operator_char`].
    fn lex_operator(&mut self) -> Operator {
        let c = self
            .bump()
            .expect("lex_operator called only when peek() is an operator char");
        match c {
            '|' => {
                if self.peek() == Some('|') {
                    self.bump();
                    Operator::OrIf
                } else {
                    Operator::Pipe
                }
            }
            '&' => {
                if self.peek() == Some('&') {
                    self.bump();
                    Operator::AndIf
                } else {
                    Operator::Amp
                }
            }
            ';' => {
                if self.peek() == Some(';') {
                    self.bump();
                    Operator::DSemi
                } else {
                    Operator::Semi
                }
            }
            '(' => Operator::LParen,
            ')' => Operator::RParen,
            '<' => {
                if self.peek() == Some('<') {
                    self.bump();
                    if self.peek() == Some('-') {
                        self.bump();
                        Operator::DLessDash
                    } else {
                        Operator::DLess
                    }
                } else if self.peek() == Some('&') {
                    self.bump();
                    Operator::LessAnd
                } else if self.peek() == Some('>') {
                    self.bump();
                    Operator::LessGreat
                } else {
                    Operator::Less
                }
            }
            '>' => {
                if self.peek() == Some('>') {
                    self.bump();
                    Operator::DGreat
                } else if self.peek() == Some('&') {
                    self.bump();
                    Operator::GreatAnd
                } else if self.peek() == Some('|') {
                    self.bump();
                    Operator::Clobber
                } else {
                    Operator::Great
                }
            }
            other => unreachable!("is_operator_char guarded this branch, got {other:?}"),
        }
    }

    /// POSIX 2.10.1 rule 3: a run of digits immediately (no intervening
    /// blank) followed by `<` or `>` is an `IO_NUMBER` token rather than a
    /// `WORD`. Only ever attempted at word-start, so a word like `a2>b`
    /// correctly stays a single `WORD` "a2" (the digit run there doesn't
    /// start fresh at word-start) followed by a `>` operator.
    fn try_lex_io_number(&mut self) -> Option<u32> {
        let rest = self.rest();
        let digit_len = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digit_len == 0 {
            return None;
        }
        match rest.as_bytes().get(digit_len) {
            Some(b'<' | b'>') => {}
            _ => return None,
        }
        let value = rest[..digit_len].parse().ok()?;
        self.pos += digit_len;
        Some(value)
    }

    // ---- words -------------------------------------------------------------

    /// Scans a full `WORD` token: a maximal run of segments up to (but not
    /// including) the next blank, newline, operator character, or EOF.
    fn scan_word(&mut self) -> Result<Word, LexError> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        loop {
            match self.peek() {
                None => break,
                Some(' ' | '\t' | '\n') => break,
                Some(c) if is_operator_char(c) => break,
                Some('\'') => {
                    flush_literal(&mut literal, &mut segments);
                    segments.push(self.scan_single_quoted()?);
                }
                Some('"') => {
                    flush_literal(&mut literal, &mut segments);
                    segments.push(self.scan_double_quoted()?);
                }
                Some('`') => {
                    flush_literal(&mut literal, &mut segments);
                    segments.push(self.scan_backquote_segment()?);
                }
                Some('$') => {
                    // A `$` not followed by valid parameter/substitution
                    // syntax falls back to a literal "$" segment (see
                    // scan_dollar); merge that into the literal run in
                    // progress instead of flushing early, so e.g. "$.foo"
                    // stays one Literal segment rather than two adjacent
                    // ones.
                    match self.scan_dollar()? {
                        WordSegment::Literal(s) => literal.push_str(&s),
                        segment => {
                            flush_literal(&mut literal, &mut segments);
                            segments.push(segment);
                        }
                    }
                }
                Some('\\') => {
                    self.bump();
                    match self.peek() {
                        None => {
                            return Err(LexError::TrailingBackslash {
                                pos: self.pos.saturating_sub(1),
                            });
                        }
                        // POSIX 2.2.1: an escaped newline (line
                        // continuation) is removed entirely — it does not
                        // even act as a word separator.
                        Some('\n') => {
                            self.bump();
                        }
                        Some(other) => {
                            literal.push(other);
                            self.bump();
                        }
                    }
                }
                Some(c) => {
                    literal.push(c);
                    self.bump();
                }
            }
        }
        flush_literal(&mut literal, &mut segments);
        Ok(Word::new(segments))
    }

    /// `'...'` — POSIX 2.2.2. No escape processing at all; the content is
    /// taken verbatim by slicing the original input.
    fn scan_single_quoted(&mut self) -> Result<WordSegment, LexError> {
        let open_pos = self.pos;
        self.bump(); // opening '\''
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedSingleQuote { start: open_pos }),
                Some('\'') => {
                    let text = self.input[start..self.pos].to_string();
                    self.bump();
                    return Ok(WordSegment::SingleQuoted(text));
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// `"..."` — POSIX 2.2.3. Interpretively builds the nested segment
    /// list: literal text has the double-quote escape set (`$ \` " \` plus
    /// newline) resolved, and `$`/backquote still introduce nested
    /// expansion sites.
    fn scan_double_quoted(&mut self) -> Result<WordSegment, LexError> {
        let open_pos = self.pos;
        self.bump(); // opening '"'
        let mut segments = Vec::new();
        let mut literal = String::new();
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedDoubleQuote { start: open_pos }),
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    match self.peek() {
                        Some(c @ ('$' | '`' | '"' | '\\')) => {
                            literal.push(c);
                            self.bump();
                        }
                        Some('\n') => {
                            self.bump();
                        }
                        Some(other) => {
                            // Backslash retains special meaning inside
                            // double quotes only before $ ` " \ <newline>
                            // (POSIX 2.2.3); before anything else, both
                            // characters are kept literally.
                            literal.push('\\');
                            literal.push(other);
                            self.bump();
                        }
                        None => {
                            return Err(LexError::TrailingBackslash {
                                pos: self.pos.saturating_sub(1),
                            });
                        }
                    }
                }
                Some('$') => match self.scan_dollar()? {
                    WordSegment::Literal(s) => literal.push_str(&s),
                    segment => {
                        flush_literal(&mut literal, &mut segments);
                        segments.push(segment);
                    }
                },
                Some('`') => {
                    flush_literal(&mut literal, &mut segments);
                    segments.push(self.scan_backquote_segment()?);
                }
                Some(c) => {
                    literal.push(c);
                    self.bump();
                }
            }
        }
        flush_literal(&mut literal, &mut segments);
        Ok(WordSegment::DoubleQuoted(segments))
    }

    /// `` `...` `` as a word segment: finds the boundary via
    /// [`Self::skip_backquote_raw`] and slices the body verbatim.
    fn scan_backquote_segment(&mut self) -> Result<WordSegment, LexError> {
        let open_pos = self.pos;
        self.skip_backquote_raw()?;
        let body = self.input[open_pos + 1..self.pos - 1].to_string();
        Ok(WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::Backtick,
            body,
        }))
    }

    /// Dispatches on what follows a `$`: `${...}`, `$(...)`, `$((...))`,
    /// a bare parameter (`$name`, `$1`, `$?`, ...), or — if none of those
    /// match — a literal `$` (POSIX: a `$` not followed by valid parameter
    /// syntax has no special meaning).
    fn scan_dollar(&mut self) -> Result<WordSegment, LexError> {
        let dollar_pos = self.pos;
        self.bump(); // consume '$'
        match self.peek() {
            Some('{') => {
                self.bump(); // consume '{'
                self.scan_braced_parameter(dollar_pos)
            }
            Some('(') if self.peek2() == Some('(') => {
                self.bump();
                self.bump(); // consume "(("
                let body_start = self.pos;
                self.skip_balanced_parens(2, BalanceContext::Arithmetic, dollar_pos)?;
                let body_end = self.pos - 2;
                Ok(WordSegment::ArithmeticExpansion(
                    self.input[body_start..body_end].to_string(),
                ))
            }
            Some('(') => {
                self.bump(); // consume '('
                let body_start = self.pos;
                self.skip_balanced_parens(1, BalanceContext::CommandSubstitution, dollar_pos)?;
                let body_end = self.pos - 1;
                Ok(WordSegment::CommandSubstitution(CommandSubstitution {
                    style: SubstitutionStyle::DollarParen,
                    body: self.input[body_start..body_end].to_string(),
                }))
            }
            Some(_) => {
                if let Some((param, len)) = try_match_bare_parameter(self.rest(), false) {
                    self.pos += len;
                    Ok(WordSegment::Parameter(param))
                } else {
                    Ok(WordSegment::Literal("$".to_string()))
                }
            }
            None => Ok(WordSegment::Literal("$".to_string())),
        }
    }

    /// Called right after consuming `${`. Tries the trivial "bare name
    /// immediately followed by `}`" shape first (the only one Phase 1
    /// decodes); otherwise finds the true matching `}` and captures the
    /// raw body for Phase 2.
    fn scan_braced_parameter(&mut self, dollar_pos: usize) -> Result<WordSegment, LexError> {
        if let Some((param, len)) = try_match_bare_parameter(self.rest(), true)
            && self.rest().as_bytes().get(len) == Some(&b'}')
        {
            self.pos += len + 1;
            return Ok(WordSegment::Parameter(param));
        }
        let body_start = self.pos;
        self.skip_to_matching_brace(dollar_pos)?;
        let body_end = self.pos - 1;
        Ok(WordSegment::ComplexParameterExpansion(
            self.input[body_start..body_end].to_string(),
        ))
    }

    // ---- boundary-only scanners (used for nested skip-over and for
    // capturing $()/``/$(())/${} bodies verbatim) -----------------------

    fn skip_single_quoted_raw(&mut self) -> Result<(), LexError> {
        let open_pos = self.pos;
        self.bump();
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedSingleQuote { start: open_pos }),
                Some('\'') => {
                    self.bump();
                    return Ok(());
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// Confirmed against real bash (`echo "outer $(echo "a)b") end"'`
    /// prints `outer a)b end`): a nested `$(...)`/`${...}` inside a
    /// double-quoted region must be skipped atomically via
    /// [`Self::skip_nested_dollar_construct`], or a `"`/`)`/`}` inside it
    /// would be mistaken for the outer double quote's terminator.
    fn skip_double_quoted_raw(&mut self) -> Result<(), LexError> {
        let open_pos = self.pos;
        self.bump();
        loop {
            if self.skip_nested_dollar_construct()? {
                continue;
            }
            match self.peek() {
                None => return Err(LexError::UnterminatedDoubleQuote { start: open_pos }),
                Some('\\') if matches!(self.peek2(), Some('$' | '`' | '"' | '\\' | '\n')) => {
                    self.bump();
                    self.bump();
                }
                Some('"') => {
                    self.bump();
                    return Ok(());
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// POSIX 2.6.3: within backquoted command substitution, backslash
    /// retains its special meaning only before `` ` ``, `$`, or `\`.
    /// Deliberately does *not* atomically skip nested `$(...)`/`${...}`
    /// the way the other scanners do — POSIX explicitly documents that as
    /// producing "undefined results" for the old backquote form, so a
    /// simple first-unescaped-backquote search is spec-compliant.
    fn skip_backquote_raw(&mut self) -> Result<(), LexError> {
        let open_pos = self.pos;
        self.bump();
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedBackquote { start: open_pos }),
                Some('\\') if matches!(self.peek2(), Some('`' | '$' | '\\')) => {
                    self.bump();
                    self.bump();
                }
                Some('`') => {
                    self.bump();
                    return Ok(());
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// Finds the matching `)` for a `(`-balanced region, having already
    /// consumed `depth` opening parens (1 for `$(`, 2 for `$((`).
    /// Confirmed against real bash that bare nested parens (an actual
    /// subshell, e.g. `$(echo a; (echo b); echo c)`) must be genuinely
    /// depth-counted, unlike braces.
    fn skip_balanced_parens(
        &mut self,
        mut depth: i32,
        ctx: BalanceContext,
        open_pos: usize,
    ) -> Result<(), LexError> {
        while depth > 0 {
            if self.skip_nested_dollar_construct()? {
                continue;
            }
            match self.peek() {
                None => return Err(ctx.unterminated_error(open_pos)),
                Some('\\') => {
                    self.bump();
                    if self.peek().is_none() {
                        return Err(LexError::TrailingBackslash {
                            pos: self.pos.saturating_sub(1),
                        });
                    }
                    self.bump();
                }
                Some('\'') => self.skip_single_quoted_raw()?,
                Some('"') => self.skip_double_quoted_raw()?,
                Some('`') => self.skip_backquote_raw()?,
                Some('(') => {
                    self.bump();
                    depth += 1;
                }
                Some(')') => {
                    self.bump();
                    depth -= 1;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
        Ok(())
    }

    /// Finds the matching `}` for a `${`-opened parameter expansion.
    /// Confirmed against real bash that a *bare* `{`/`}` is **not**
    /// depth-tracked (`echo "${y:-}}"` with `y=Z` prints `Z}` — the
    /// expansion closes at the first unescaped `}`); only a nested
    /// `${`/`$(` construct is skipped atomically, via
    /// [`Self::skip_nested_dollar_construct`].
    fn skip_to_matching_brace(&mut self, open_pos: usize) -> Result<(), LexError> {
        loop {
            if self.skip_nested_dollar_construct()? {
                continue;
            }
            match self.peek() {
                None => return Err(LexError::UnterminatedParameterExpansion { start: open_pos }),
                Some('\\') => {
                    self.bump();
                    if self.peek().is_none() {
                        return Err(LexError::TrailingBackslash {
                            pos: self.pos.saturating_sub(1),
                        });
                    }
                    self.bump();
                }
                Some('\'') => self.skip_single_quoted_raw()?,
                Some('"') => self.skip_double_quoted_raw()?,
                Some('`') => self.skip_backquote_raw()?,
                Some('}') => {
                    self.bump();
                    return Ok(());
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// If positioned at `$(`, `$((`, or `${`, atomically skips the entire
    /// nested construct (recursing through these same scanners) and
    /// returns `true`; otherwise does nothing and returns `false`.
    ///
    /// This is what makes [`Self::skip_double_quoted_raw`],
    /// [`Self::skip_balanced_parens`], and [`Self::skip_to_matching_brace`]
    /// correct against pathological-but-real input like
    /// `$(echo ${x:-)} end)` (confirmed against real bash to print
    /// `... end)` intact — the `)` inside `${x:-)}` must not decrement the
    /// outer command substitution's paren depth).
    fn skip_nested_dollar_construct(&mut self) -> Result<bool, LexError> {
        if self.peek() != Some('$') {
            return Ok(false);
        }
        match self.peek2() {
            Some('(') => {
                let open_pos = self.pos;
                self.bump();
                self.bump();
                if self.peek() == Some('(') {
                    self.bump();
                    self.skip_balanced_parens(2, BalanceContext::Arithmetic, open_pos)?;
                } else {
                    self.skip_balanced_parens(1, BalanceContext::CommandSubstitution, open_pos)?;
                }
                Ok(true)
            }
            Some('{') => {
                let open_pos = self.pos;
                self.bump();
                self.bump();
                self.skip_to_matching_brace(open_pos)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

fn is_operator_char(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

fn flush_literal(literal: &mut String, segments: &mut Vec<WordSegment>) {
    if !literal.is_empty() {
        segments.push(WordSegment::Literal(std::mem::take(literal)));
    }
}

/// Tries to match a bare parameter (no operator suffix) at the start of
/// `s`: one of the single-character special parameters, a positional
/// parameter, or a `Name`. Returns the parameter and how many bytes of `s`
/// it consumed. Purely a lookahead — does not touch lexer state.
///
/// `allow_multi_digit` distinguishes the two contexts POSIX gives
/// different rules for: unbraced `$digit` only ever consumes exactly one
/// digit (`$12` is positional parameter 1 followed by literal `2`), while
/// braced `${digits}` consumes the whole run (`${10}`, `${11}`, ...).
fn try_match_bare_parameter(s: &str, allow_multi_digit: bool) -> Option<(Parameter, usize)> {
    let first = s.chars().next()?;
    if let Some(special) = special_parameter_for(first) {
        return Some((Parameter::Special(special), first.len_utf8()));
    }
    if first.is_ascii_digit() {
        return if allow_multi_digit {
            let digit_len = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            let digits = &s[..digit_len];
            if digits == "0" {
                Some((Parameter::Special(SpecialParameter::Zero), digit_len))
            } else {
                digits
                    .parse::<u32>()
                    .ok()
                    .map(|n| (Parameter::Positional(n), digit_len))
            }
        } else if first == '0' {
            Some((Parameter::Special(SpecialParameter::Zero), 1))
        } else {
            Some((Parameter::Positional(first.to_digit(10).unwrap()), 1))
        };
    }
    if first == '_' || first.is_ascii_alphabetic() {
        let name_len = s
            .find(|c: char| c != '_' && !c.is_ascii_alphanumeric())
            .unwrap_or(s.len());
        return Some((Parameter::Name(s[..name_len].to_string()), name_len));
    }
    None
}

fn special_parameter_for(c: char) -> Option<SpecialParameter> {
    Some(match c {
        '@' => SpecialParameter::At,
        '*' => SpecialParameter::Star,
        '#' => SpecialParameter::Hash,
        '?' => SpecialParameter::Question,
        '-' => SpecialParameter::Dash,
        '$' => SpecialParameter::Dollar,
        '!' => SpecialParameter::Bang,
        _ => return None,
    })
}
