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

/// Re-lexes `input` as one word's worth of segments, using the same
/// quoting/escaping/expansion-site rules [`lex`] uses inside an ordinary
/// (unquoted-context) `WORD` token — POSIX 2.2's quoting mechanisms, and
/// `$`/backquote expansion sites — except the word never ends early at a
/// blank, newline, or operator character: the only boundary is the end
/// of `input`.
///
/// # What this is for
///
/// `conch-shell-parser`'s Phase 2 parameter-expansion parser uses this to
/// re-lex the `word` operand of `${parameter:-word}` and its sibling
/// POSIX 2.6.2 operators, *after* this crate has already found the
/// operand's correct extent (captured verbatim in
/// [`WordSegment::ComplexParameterExpansion`]) — the operand can itself
/// contain quoting and further expansion sites (e.g. `${x:-$OTHER}`,
/// `${x:-'a b' c}`), so it needs the same segment-aware re-lexing an
/// ordinary word gets, not a flat string.
///
/// Use this specific entry point when the enclosing `${...}` is **not**
/// itself nested inside an outer `"..."` — confirmed against real bash
/// that unquoted `${x:-'a b' c}` keeps `'a b'` together as one field (real
/// single-quoting) while splitting on the unquoted space before `c`. See
/// [`lex_double_quoted_body`] for the different ruleset bash uses when
/// the enclosing `${...}` *is* nested inside `"..."` — getting this
/// distinction wrong is exactly the kind of "almost-right" quoting bug
/// this crate exists to avoid, so callers must pick deliberately rather
/// than defaulting to one or the other; see that function's docs for the
/// worked example that tells them apart.
///
/// # Errors
///
/// Returns [`LexError`] if `input` contains an unterminated quote or a
/// trailing unescaped backslash. In practice this should never happen for
/// a body this crate already bounded correctly (the same quote/backslash
/// scanning rules apply both times), but the error is still surfaced
/// rather than panicking, in case of a genuine inconsistency.
pub fn lex_word_body(input: &str) -> Result<Word, LexError> {
    Lexer::new(input).scan_word_until(|_| false)
}

/// Re-lexes `input` as double-quoted-*style* content — literal text with
/// the double-quote backslash-escape set (`$ \` " \` plus newline)
/// resolved, and `$`/backquote still introducing nested expansion sites —
/// running to the end of `input` rather than stopping at a closing `"`.
///
/// # What this is for
///
/// Same purpose as [`lex_word_body`] — re-lexing a `${parameter:-word}`-
/// shaped operand for `conch-shell-parser`'s Phase 2 parameter-expansion
/// parser — but for the case where the enclosing `${...}` **is** nested
/// inside an outer `"..."`. Confirmed against real bash that this is a
/// genuinely different ruleset, not just the same one applied in a
/// quoted position: POSIX 2.2.3's "a single-quote loses its special
/// meaning within double-quotes" turns out to reach straight through
/// `${...}` nesting. For example:
///
/// - `echo ${x:-'a b'}` (unquoted, unset `x`) prints `a b` — the `'...'`
///   really quotes, and quote removal strips it.
/// - `echo "${x:-'a b'}"` (nested in `"..."`, unset `x`) prints `'a b'`
///   *with the quote characters still in the output* — inside the outer
///   double quotes, `'` is just an ordinary character.
///
/// A nested `"..."` inside the operand is unaffected by this distinction
/// (POSIX 2.6.2/2.6.3's own "tokenizing rules applied recursively" let
/// double quotes always re-open inside `${...}`, even here) — e.g.
/// `echo "${x:-"y"}"` still prints `y`, not `"y"`.
///
/// # Errors
///
/// See [`lex_word_body`].
pub fn lex_double_quoted_body(input: &str) -> Result<Vec<WordSegment>, LexError> {
    Lexer::new(input).scan_double_quoted_style_until(|_| false)
}

/// Recognizes one `$`-led expansion site — `$name`, `${...}`, `$(...)`,
/// or `$((...))` — starting at the beginning of `input`, or, if what
/// follows the `$` isn't valid parameter/substitution syntax, a literal
/// `"$"` (POSIX: a `$` not followed by valid syntax has no special
/// meaning). Returns the recognized segment and how many bytes of
/// `input` it consumed.
///
/// This is the exact recognition [`lex`], [`lex_word_body`], and
/// [`lex_double_quoted_body`] already use for the `$` case, exposed
/// standalone for `conch-shell-parser`'s arithmetic-expansion body
/// scanner (`$((...))`'s content), which needs the identical
/// `$`-expansion recognition but under a *third* set of quoting rules for
/// everything else — POSIX 2.6.4: the arithmetic body is "treated as if
/// it were in double-quotes, except that a double-quote inside the
/// expression is not treated specially," and confirmed against real bash
/// that a literal `'` never quotes there either (unlike both
/// [`lex_word_body`] and [`lex_double_quoted_body`]) — so it re-lexes the
/// body with its own loop rather than reusing either of those.
///
/// # Panics
///
/// Panics if `input` does not start with `$`; callers only call this
/// after checking that themselves (mirroring [`Lexer`]'s internal
/// `lex_operator`, which has the same "caller already checked" contract).
///
/// # Errors
///
/// Returns [`LexError`] if the `$` opens a `${...}`, `$(...)`, or
/// `$((...))` construct that never finds its matching terminator.
pub fn lex_dollar_expansion(input: &str) -> Result<(WordSegment, usize), LexError> {
    assert_eq!(
        input.chars().next(),
        Some('$'),
        "lex_dollar_expansion requires input starting with '$'"
    );
    let mut lexer = Lexer::new(input);
    let segment = lexer.scan_dollar()?;
    Ok((segment, lexer.pos))
}

/// Recognizes one `` `...` `` backquoted command substitution starting at
/// the beginning of `input`. Returns the segment and how many bytes of
/// `input` it consumed. See [`lex_dollar_expansion`] for why this is
/// exposed standalone.
///
/// # Panics
///
/// Panics if `input` does not start with `` ` ``; see
/// [`lex_dollar_expansion`]'s docs for the same contract.
///
/// # Errors
///
/// Returns [`LexError::UnterminatedBackquote`] if no closing `` ` `` is
/// found.
pub fn lex_backquote_expansion(input: &str) -> Result<(WordSegment, usize), LexError> {
    assert_eq!(
        input.chars().next(),
        Some('`'),
        "lex_backquote_expansion requires input starting with '`'"
    );
    let mut lexer = Lexer::new(input);
    let segment = lexer.scan_backquote_segment()?;
    Ok((segment, lexer.pos))
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
        self.scan_word_until(|c| matches!(c, ' ' | '\t' | '\n') || is_operator_char(c))
    }

    /// Scans word segments — quotes, `$`/backquote expansion sites, and
    /// backslash-escaped literal text, per POSIX 2.2/2.6.2/2.6.3/2.6.4 —
    /// until `is_boundary` reports `true` for the next unconsumed
    /// character, or end of input, whichever comes first.
    ///
    /// Factored out of [`Self::scan_word`] (which stops at a blank,
    /// newline, or operator character, per POSIX `WORD` token
    /// recognition) so [`lex_word_body`] can reuse the *exact* same
    /// quoting/escaping/expansion-site recognition to re-lex a `${...}`
    /// operand word for Phase 2's parameter-expansion parser
    /// (`conch-shell-parser`) — there, the only real boundary is
    /// end-of-string: the operand was already captured to its correct
    /// end by this crate's brace-matching (see [`Self::skip_to_matching_brace`]),
    /// so nothing inside it — blanks, `;`, `|`, ... — should stop the scan
    /// early the way it would for an ordinary `WORD` token.
    fn scan_word_until(&mut self, is_boundary: impl Fn(char) -> bool) -> Result<Word, LexError> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        loop {
            match self.peek() {
                None => break,
                Some(c) if is_boundary(c) => break,
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
        let segments = self.scan_double_quoted_style_until(|c| c == '"')?;
        if self.peek().is_none() {
            return Err(LexError::UnterminatedDoubleQuote { start: open_pos });
        }
        self.bump(); // closing '"'
        Ok(WordSegment::DoubleQuoted(segments))
    }

    /// Scans double-quoted-*style* content — literal text with the
    /// double-quote backslash-escape set (`$ \` " \` plus newline)
    /// resolved, and `$`/backquote still introducing nested expansion
    /// sites — until `is_boundary` reports `true` for the next
    /// unconsumed character, or end of input.
    ///
    /// Factored out of [`Self::scan_double_quoted`] (which stops at the
    /// closing `"`) so [`lex_double_quoted_body`] can reuse the same
    /// escape/expansion-site recognition to re-lex a `${...}` operand
    /// word that is itself nested inside an outer `"..."` — confirmed
    /// against real bash that this is a genuinely different ruleset from
    /// [`Self::scan_word_until`]/[`lex_word_body`] (POSIX 2.2.3: a
    /// single-quote has no special meaning inside double quotes, and that
    /// reaches straight through `${...}` nesting — see [`lex_double_quoted_body`]'s
    /// docs for the worked example). Does *not* itself decide whether
    /// `is_boundary` matching means "found a real closing quote" versus
    /// "ran out of input"; callers that care (like
    /// [`Self::scan_double_quoted`]) check `self.peek()` afterward.
    fn scan_double_quoted_style_until(
        &mut self,
        is_boundary: impl Fn(char) -> bool,
    ) -> Result<Vec<WordSegment>, LexError> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        loop {
            match self.peek() {
                None => break,
                Some(c) if is_boundary(c) => break,
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
                // A bare, unescaped '"' that `is_boundary` doesn't already
                // treat as this call's own terminator (i.e. only reachable
                // from `lex_double_quoted_body`, never from
                // `scan_double_quoted` itself, since that call's
                // `is_boundary` is `|c| c == '"'` and the arm above always
                // wins first) opens a genuinely nested double-quoted
                // segment — confirmed against real bash
                // (`echo "${x:-"y"}"` prints `y`, not `"y"`): POSIX
                // 2.6.2/2.6.3's "tokenizing rules applied recursively" let
                // `"..."` re-open inside `${...}` even when the whole
                // thing is already nested inside an outer `"..."`, unlike
                // a nested `'...'` (see this function's docs).
                Some('"') => {
                    flush_literal(&mut literal, &mut segments);
                    segments.push(self.scan_double_quoted()?);
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
        Ok(segments)
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
                if let Some((param, len)) = match_bare_parameter(self.rest(), false) {
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
        if let Some((param, len)) = match_bare_parameter(self.rest(), true)
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
///
/// Exposed as `pub` (beyond this crate's own `$name`/`${name}`
/// recognition) for `conch-shell-parser`'s Phase 2 parameter-expansion
/// parser, which needs the *identical* parameter-name recognition to
/// implement POSIX 2.6.2's `${#parameter}` string-length form: that form
/// requires the text after the `#` to match a bare parameter and *nothing
/// else* (the whole remainder, to the closing `}`) — confirmed against
/// real bash (`${#x#l}` is a syntax error, not length-of-`x` followed by
/// a stray `#l`) — which this same function's return value (how many
/// bytes it consumed) makes easy to check: the length form applies iff
/// the match consumes the entire remaining string.
pub fn match_bare_parameter(s: &str, allow_multi_digit: bool) -> Option<(Parameter, usize)> {
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
