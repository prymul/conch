//! Phase 2: POSIX 2.6.4 arithmetic expansion (`$((expression))`) parsing.
//!
//! # Why this is a genuine two-phase parse, not one
//!
//! POSIX 2.6.4 describes arithmetic expansion as two sequential steps:
//!
//! 1. "The shell shall expand all tokens in the expression for parameter
//!    expansion, command substitution, and quote removal" — i.e. the raw
//!    body is first expanded **as text**, exactly like the inside of a
//!    double-quoted string (with one exception — see below).
//! 2. "Next, the shell shall treat this as an arithmetic expression and
//!    substitute the value" — i.e. *only after* step 1's textual
//!    expansion is complete does the result get tokenized and evaluated
//!    against the ISO C arithmetic grammar.
//!
//! This ordering is not a minor technicality — confirmed against real
//! bash, an expansion's *result* can inject entire operators into the
//! surrounding expression, not just a single operand value:
//!
//! ```text
//! echo $(( $(echo "1+2") * 3 ))   ->  7   (parses as "1+2 * 3" = 1+(2*3))
//! x="1+2"; echo $(( x * 3 ))      ->  9   (bare `x` is a grammar-level
//!                                          variable reference, recursively
//!                                          re-evaluated as arithmetic —
//!                                          a completely different bash
//!                                          feature, not textual splicing)
//! ```
//!
//! So a single-pass parser that tries to build one final expression tree
//! directly from the raw `$((...))` body is unsound in general: it cannot
//! know, at parse time, whether a `$(...)`'s eventual output will be a
//! plain number or something that injects further operators. This module
//! therefore exposes two entry points matching the two POSIX steps:
//!
//! - [`parse_arithmetic_body`] — step 1's *structure* (not its
//!   evaluation): decodes the raw `$((...))` body into a [`Word`] whose
//!   segments are literal text interspersed with expansion sites
//!   (`$name`, `${...}`, `` $(...) ``/`` `...` ``, nested `$((...))`).
//!   Deliberately reuses [`Word`]/[`WordSegment`] rather than inventing a
//!   parallel type: the evaluator already has a `Word` → expanded-`String`
//!   pipeline (tilde/parameter/command/arithmetic expansion, quote
//!   removal) for ordinary words, and running that *same* pipeline over
//!   this `Word` is exactly step 1. The one thing that makes this a
//!   different scan from an ordinary word or `${...}` operand rather than
//!   a call to an existing `conch-shell-lexer` entry point: POSIX's "as if
//!   in double-quotes, except a double-quote inside is not treated
//!   specially" is a *third*, distinct quoting ruleset — confirmed against
//!   real bash: a `'` never quotes here (unlike an ordinary word), and an
//!   *unescaped* `"` is silently deleted rather than pairing up to quote
//!   anything (unlike an ordinary double-quoted string, or even a
//!   `${...}` operand nested in one) — see the "Grounding: quoting inside
//!   `$((...))`" section below for the worked examples this was checked
//!   against.
//! - [`parse_arithmetic_expr`] — step 2: tokenizes and parses an
//!   **already fully textually-expanded** arithmetic expression string
//!   (i.e. what you get after expanding every segment of
//!   [`parse_arithmetic_body`]'s `Word` and concatenating the results)
//!   into an [`ArithExpr`] tree, per the ISO C operator set bash extends
//!   (see "Operator precedence" below). This step needs no shell state at
//!   all — it's pure grammar — which is why it belongs here rather than
//!   in the evaluator; only *walking* the resulting tree (variable
//!   lookup, applying operators, assignment side effects, bash's
//!   recursive "a variable whose value isn't numeric is itself evaluated
//!   as arithmetic" rule) is the evaluator's job.
//!
//! Expected evaluator integration, for a segment
//! `WordSegment::ArithmeticExpansion(body)` (top-level, or recursively for
//! a nested one found inside [`parse_arithmetic_body`]'s own output):
//!
//! ```text
//! let source_word = parse_arithmetic_body(&body)?;      // step 1 structure
//! let source_text = expand_word_single(&source_word)?;  // step 1 evaluation (existing pipeline)
//! let expr = parse_arithmetic_expr(&source_text)?;      // step 2 structure
//! let value: i64 = evaluate(&expr)?;                    // step 2 evaluation (new)
//! ```
//!
//! # Grounding: quoting inside `$((...))`
//!
//! All of the following were confirmed against real bash (5.3) while
//! designing this module, since POSIX's prose ("as if double-quoted,
//! except a double-quote is not treated specially") under-specifies the
//! exact character-level behavior:
//!
//! - `echo $(( 1 + \"2\" ))` — an *escaped* `\"` survives as a literal
//!   `"` character in the text handed to step 2 (which then fails to
//!   parse it as a valid token — expected, since arithmetic doesn't use
//!   `"` for anything). Same double-quote backslash-escape set as an
//!   ordinary double-quoted string (`$` `` ` `` `"` `\` and
//!   backslash-newline line continuation); anything else keeps *both*
//!   the backslash and the following character literally.
//! - `echo $(( 1 + "x)" + 2 ))` — an *unescaped* `"` is deleted with no
//!   trace (not paired with a later `"` the way an ordinary double-quoted
//!   string would be): the text handed to step 2 is `1 + x) + 2` — note
//!   the `)` that was inside the quotes is now a bare, unmatched
//!   parenthesis, which is exactly why bash reports a syntax error for
//!   this input rather than silently treating `"x)"` as a
//!   parenthesis-protecting quoted region.
//! - `echo $(( 1 + 'x)' ))` — `conch-shell-lexer`'s *outer* boundary-find
//!   for where `$((...))` ends still honors `'...'` as protecting an
//!   embedded `)` (this is `conch-shell-lexer`'s job, already correct;
//!   see its module docs) — but once this module receives the extracted
//!   body, the `'` characters are just ordinary literal characters with
//!   no effect on step 2's tokenization at all (they end up as invalid
//!   tokens, matching bash: "operand expected").
//! - `echo $(( `echo 3` + 1 ))` and `echo $(( $(echo 3) + 1 ))` both work
//!   (4) — backquote and `$(...)` command substitution are both
//!   recognized expansion sites, same as anywhere else.
//! - A raw, un-escaped newline inside the body, or a backslash-newline
//!   line continuation, are both fine (`$((\n1+2\n))` and
//!   `$((1 +\` + newline + `2))` both evaluate to 3) — the former because
//!   whitespace (including newlines) is insignificant between step 2's
//!   tokens; the latter because backslash-newline is always a line
//!   continuation, removed entirely, matching the double-quote rule.
//!
//! # Operator precedence
//!
//! From the GNU Bash Reference Manual (5.3), §6.5 "Shell Arithmetic" —
//! "The operators and their precedence, associativity, and values are the
//! same as in the C language" — listed highest to lowest precedence
//! (operators on the same line share a precedence level):
//!
//! ```text
//! id++ id--              postfix increment/decrement (bash extension)
//! ++id --id               prefix increment/decrement (bash extension)
//! - +                     unary minus/plus
//! ! ~                     logical/bitwise negation
//! **                      exponentiation (bash extension; right-assoc)
//! * / %                   multiplication, division, remainder
//! + -                     addition, subtraction
//! << >>                   bitwise shifts
//! <= >= < >               comparison
//! == !=                   equality
//! &                       bitwise AND
//! ^                       bitwise XOR
//! |                       bitwise OR
//! &&                      logical AND
//! ||                      logical OR
//! ?:                      ternary conditional (right-assoc)
//! = *= /= %= += -= <<= >>= &= ^= |=   assignment (right-assoc)
//! ,                       comma (lowest)
//! ```
//!
//! POSIX 2.6.4 requires only the ISO C constant/operator subset (no
//! `sizeof`, no `++`/`--`, no `**`); every operator marked "bash
//! extension" above is accepted here as a deliberate bash-mode choice,
//! matching this project's general policy of tracking bash behavior
//! rather than strict POSIX-only — see `CLAUDE.md`/crate docs elsewhere
//! in this project for that framing. Confirmed right-associative via real
//! bash: `echo $((2**3**2))` is `512` (= `2**(3**2)`, not `(2**3)**2` =
//! `64`) and `echo $((x=y=5))` assigns both.
//!
//! # Integer width and overflow
//!
//! Bash manual §6.5: "Evaluation is done in the largest fixed-width
//! integers available, with no check for overflow, though division by 0
//! is trapped and flagged as an error." Confirmed against real bash
//! (5.3, 64-bit): `echo $((9223372036854775807+1))` prints
//! `-9223372036854775808` (`i64::MIN`) — two's-complement wraparound on a
//! 64-bit signed integer, not a panic or saturation. [`ArithExpr::Number`]
//! therefore stores `i64`, and this parser's own numeric-literal decoding
//! (needed for `Number`, independent of any shell state) reproduces the
//! same wraparound: `echo $((99999999999999999999))` (a 20-digit decimal
//! literal) prints `7766279631452241919`, which is exactly
//! `99999999999999999999 mod 2**64` reinterpreted as a signed 64-bit
//! value — i.e. bash accumulates digits with *unsigned* 64-bit wrapping
//! arithmetic and only reinterprets the final bit pattern as signed
//! (confirmed further by `echo $((18446744073709551615))`, exactly
//! `u64::MAX`, printing `-1`). This parser's [`decode_number`] mirrors
//! that exact accumulation (`u64::wrapping_mul`/`wrapping_add`, then an
//! `as i64` bit-reinterpreting cast) rather than erroring or saturating on
//! an oversized literal. Actual expression *evaluation* (applying `+`,
//! `*`, etc. with the same wrapping semantics, and turning division by
//! zero into an error) is the evaluator's job, not this parser's — but it
//! should use the same wrapping-`i64` model throughout for consistency
//! with how literals are already decoded here.
//!
//! Integer literals otherwise follow ISO C plus one bash extension
//! (Bash manual §6.5): a leading `0` is octal, a leading `0x`/`0X` is
//! hexadecimal, and otherwise (bash extension, confirmed empirically:
//! `echo $((2#101))` is `5`, `echo $((36#z))` and `echo $((36#Z))` are
//! both `35`, `echo $((37#Z))` errors "value too great for base") the
//! form `base#n` gives `n` in the arithmetic `base` (2 to 64 inclusive;
//! digits beyond 9 are, in order, lowercase `a`-`z` (10-35), uppercase
//! `A`-`Z` (36-61, or interchangeable with lowercase when `base <= 36`),
//! `@` (62), and `_` (63)).

use conch_shell_lexer::{
    LexError, Word, WordSegment, lex_backquote_expansion, lex_dollar_expansion,
};

// ============================================================================
// AST
// ============================================================================

/// A parsed POSIX 2.6.4 (plus bash-extension) arithmetic expression —
/// the result of [`parse_arithmetic_expr`]. Evaluating this against shell
/// state (variable lookup/assignment, applying operators with wrapping
/// `i64` semantics, division-by-zero as an error) is the evaluator's job;
/// see the module docs for the expected integration shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArithExpr {
    /// An integer literal, already decoded to its wrapped 64-bit value —
    /// see the module docs' "Integer width and overflow" section.
    Number(i64),
    /// A bare identifier. Per POSIX 2.6.4 and the bash manual: "shell
    /// variables may also be referenced by name without using the
    /// parameter expansion syntax... A shell variable that is null or
    /// unset evaluates to 0 when referenced by name," and (bash) a
    /// variable whose value isn't itself numeric is recursively
    /// re-evaluated as an arithmetic expression — all evaluator-side
    /// semantics; this parser only records the name.
    Variable(String),
    /// A unary prefix operator.
    Unary {
        op: ArithUnaryOp,
        operand: Box<ArithExpr>,
    },
    /// `id++` / `id--` / `++id` / `--id` (bash extension — POSIX doesn't
    /// require these). `target` is always a bare identifier: `(x+1)++`
    /// and similar are rejected at parse time
    /// ([`ArithError::NotAnIdentifier`]), matching that these operators
    /// require an lvalue.
    IncrDecr {
        op: IncrDecrOp,
        target: String,
        /// `true` for `++id`/`--id`, `false` for `id++`/`id--`.
        prefix: bool,
    },
    /// A left/right binary operator application.
    Binary {
        op: ArithBinaryOp,
        lhs: Box<ArithExpr>,
        rhs: Box<ArithExpr>,
    },
    /// `cond ? if_true : if_false`. Per the ISO C grammar bash follows,
    /// `if_true` is parsed at the *full expression* level (i.e. may
    /// itself contain an unparenthesized top-level comma or assignment —
    /// confirmed against real bash: `echo $((1 ? 2,3 : 4))` is `3`, and
    /// `x=0; echo $((1 ? x=5 : 0))` assigns `x`), while `if_false`
    /// recurses at the conditional level itself, which is what makes
    /// `a?b:c?d:e` chain correctly without parentheses (confirmed:
    /// `echo $((0 ? 1 : 2 ? 3 : 4))` is `3`).
    Conditional {
        cond: Box<ArithExpr>,
        if_true: Box<ArithExpr>,
        if_false: Box<ArithExpr>,
    },
    /// `target op= value` (or plain `target = value`). `target` is
    /// always a bare identifier (same restriction, and same reasoning,
    /// as [`Self::IncrDecr`]). Right-associative — confirmed against
    /// real bash: `x=1; y=2; echo $((x=y=5))` sets *both* to 5.
    Assign {
        op: ArithAssignOp,
        target: String,
        value: Box<ArithExpr>,
    },
    /// `first , second` — evaluates to `second`'s value (evaluator's
    /// job), but both sides' side effects (assignments) happen.
    Comma {
        first: Box<ArithExpr>,
        second: Box<ArithExpr>,
    },
}

/// Unary prefix operators (`- + ! ~`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithUnaryOp {
    /// `+x` — unary plus (a POSIX/C no-op numerically, but still a real
    /// grammar position: it requires its operand to be a genuine
    /// arithmetic expression).
    Plus,
    /// `-x` — arithmetic negation.
    Minus,
    /// `!x` — logical negation (`x == 0` ? `1` : `0`).
    LogicalNot,
    /// `~x` — bitwise complement.
    BitNot,
}

/// `++`/`--`, prefix or postfix (bash extension — see [`ArithExpr::IncrDecr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncrDecrOp {
    Increment,
    Decrement,
}

/// Binary operators, POSIX 2.6.4 plus the bash-extension `**` — see the
/// module docs' precedence table for where each sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithBinaryOp {
    /// `**` — exponentiation (bash extension).
    Power,
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    ShiftLeft,
    ShiftRight,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Eq,
    NotEq,
    BitAnd,
    BitXor,
    BitOr,
    LogicalAnd,
    LogicalOr,
}

/// Assignment operators (`=` and the compound `op=` forms). There is no
/// `**=` — confirmed against the bash manual's own operator list, which
/// omits it even though `**` itself is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithAssignOp {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    ShiftLeftAssign,
    ShiftRightAssign,
    BitAndAssign,
    BitXorAssign,
    BitOrAssign,
}

/// An error encountered while parsing an arithmetic expansion body or
/// expression.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArithError {
    /// A `\` at the very end of a `$((...))` body, with no following
    /// character to escape and no newline to form a line continuation.
    #[error(
        "backslash at end of arithmetic expansion body with no character to escape (byte {pos})"
    )]
    TrailingBackslash { pos: usize },
    /// A character that can't start any valid arithmetic token (e.g. a
    /// bare `'`, which — unlike in an ordinary word — never quotes
    /// anything inside `$((...))`; see the module docs).
    #[error("invalid character {found:?} in arithmetic expression (byte {pos})")]
    UnexpectedChar { found: char, pos: usize },
    /// The expression ended where a token was still required.
    #[error("unexpected end of arithmetic expression, expected {expected}")]
    UnexpectedEnd { expected: String },
    /// A token appeared where the grammar didn't allow it.
    #[error("unexpected token in arithmetic expression, expected {expected} (byte {pos})")]
    UnexpectedToken { expected: String, pos: usize },
    /// `++`/`--`/an assignment operator was applied to something other
    /// than a bare identifier (e.g. `5++`, `(x+1)=2`).
    #[error("`{op}` requires an identifier operand (byte {pos})")]
    NotAnIdentifier { op: &'static str, pos: usize },
    /// A `base#...` numeric literal's base wasn't a valid integer in
    /// `2..=64` (bash manual §6.5: "the optional base is a decimal
    /// number between 2 and 64").
    #[error("invalid arithmetic base {base:?} (byte {pos}); base must be between 2 and 64")]
    InvalidBase { base: String, pos: usize },
    /// A digit in a numeric literal was too large for that literal's
    /// base (e.g. `2#12`, or `37#Z` — `Z` needs base >= 62 once base > 36
    /// makes upper/lowercase letters distinct digits; see the module
    /// docs).
    #[error("digit {digit:?} is not valid in base {base} (byte {pos})")]
    DigitOutOfRange { digit: char, base: u32, pos: usize },
    /// Re-lexing a `$`/backquote expansion site inside the body failed;
    /// see [`LexError`].
    #[error(transparent)]
    Lex(#[from] LexError),
}

// ============================================================================
// Phase 1: raw body -> Word (structure only; no expansion is evaluated)
// ============================================================================

/// Decodes the raw body of a [`WordSegment::ArithmeticExpansion`] into a
/// [`Word`] whose segments are literal arithmetic-syntax text interspersed
/// with expansion sites — POSIX 2.6.4's "the shell shall expand all
/// tokens in the expression for parameter expansion, command
/// substitution, and quote removal" step, structurally. See the module
/// docs for why this can't just return a flat, already-expanded `String`
/// (that would require running shell state this parser doesn't have) and
/// for the "as if double-quoted, except a double-quote isn't treated
/// specially" quoting rules this reproduces, empirically confirmed
/// against real bash to be a distinct ruleset from both an ordinary word
/// and a `${...}` operand.
///
/// The returned `Word` only ever contains [`WordSegment::Literal`],
/// [`WordSegment::Parameter`], [`WordSegment::ComplexParameterExpansion`],
/// [`WordSegment::CommandSubstitution`], and
/// [`WordSegment::ArithmeticExpansion`] segments — never
/// [`WordSegment::SingleQuoted`]/[`WordSegment::DoubleQuoted`], since
/// neither quote character opens a real quoted region here (a `'` is
/// always literal; an unescaped `"` is always deleted). It's meant to be
/// handed directly to the same `Word` → expanded-`String` pipeline the
/// evaluator already has for ordinary words (e.g.
/// `conch-shell-core`'s `expand_word_single`) to complete POSIX 2.6.4's
/// first step, before calling [`parse_arithmetic_expr`] on the result.
///
/// # Errors
///
/// Returns [`ArithError::TrailingBackslash`] for a trailing unescaped
/// backslash, or [`ArithError::Lex`] if a `$`/backquote expansion site
/// inside the body never finds its matching terminator (in practice this
/// should never happen for a body `conch-shell-lexer` already bounded
/// correctly, but the error is still surfaced rather than panicking).
///
/// [`WordSegment::ArithmeticExpansion`]: conch_shell_lexer::WordSegment::ArithmeticExpansion
pub fn parse_arithmetic_body(body: &str) -> Result<Word, ArithError> {
    let mut scanner = BodyScanner::new(body);
    let mut segments = Vec::new();
    let mut literal = String::new();
    loop {
        match scanner.peek() {
            None => break,
            Some('\\') => {
                scanner.bump();
                match scanner.peek() {
                    Some(c @ ('$' | '`' | '"' | '\\')) => {
                        literal.push(c);
                        scanner.bump();
                    }
                    Some('\n') => {
                        scanner.bump();
                    }
                    Some(other) => {
                        // Same rule as inside an ordinary double-quoted
                        // string: backslash retains special meaning only
                        // before $ ` " \ <newline>; before anything else
                        // both characters stay literal. Confirmed against
                        // real bash: `echo $((1\+2))` reports the error
                        // token as literally `\+2`, backslash included.
                        literal.push('\\');
                        literal.push(other);
                        scanner.bump();
                    }
                    None => {
                        return Err(ArithError::TrailingBackslash {
                            pos: scanner.pos.saturating_sub(1),
                        });
                    }
                }
            }
            // An *unescaped* '"' is simply deleted -- see the module
            // docs' grounding section. It is never paired with a later
            // '"' the way an ordinary double-quoted string would be.
            Some('"') => {
                scanner.bump();
            }
            Some('$') => {
                let (segment, len) = lex_dollar_expansion(scanner.rest())?;
                match segment {
                    WordSegment::Literal(s) => literal.push_str(&s),
                    other => {
                        flush_literal(&mut literal, &mut segments);
                        segments.push(other);
                    }
                }
                scanner.pos += len;
            }
            Some('`') => {
                let (segment, len) = lex_backquote_expansion(scanner.rest())?;
                flush_literal(&mut literal, &mut segments);
                segments.push(segment);
                scanner.pos += len;
            }
            Some(c) => {
                literal.push(c);
                scanner.bump();
            }
        }
    }
    flush_literal(&mut literal, &mut segments);
    Ok(Word::new(segments))
}

fn flush_literal(literal: &mut String, segments: &mut Vec<WordSegment>) {
    if !literal.is_empty() {
        segments.push(WordSegment::Literal(std::mem::take(literal)));
    }
}

/// A minimal cursor over `&str`, used only by [`parse_arithmetic_body`].
/// Deliberately not `conch_shell_lexer::Lexer` itself — this scan applies
/// a genuinely different (and simpler) set of character-level rules; see
/// the module docs.
struct BodyScanner<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> BodyScanner<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }
}

// ============================================================================
// Phase 2: already-expanded text -> ArithExpr
// ============================================================================

/// Tokenizes and parses `source` — an arithmetic expression **after**
/// POSIX 2.6.4's parameter/command/arithmetic-expansion-and-quote-removal
/// step has already run (i.e. the fully expanded text of a
/// [`parse_arithmetic_body`] result) — into an [`ArithExpr`], per the
/// precedence table in the module docs.
///
/// An empty (or all-whitespace) `source` parses to `ArithExpr::Number(0)`
/// rather than erroring — confirmed against real bash: `echo $(())`
/// prints `0`, and this also covers the common case of an unset variable
/// or empty command substitution being the entire body (e.g.
/// `unset y; echo $(($y))` also prints `0`, since `$y` expands to the
/// empty string before this step ever runs).
///
/// # Errors
///
/// Returns [`ArithError`] for any lexical or grammatical problem in
/// `source` — an invalid character, a malformed numeric literal, a
/// misplaced/missing operator or operand, unbalanced parentheses,
/// trailing tokens after a complete expression, or `++`/`--`/an
/// assignment applied to something other than a bare identifier.
/// Division by zero and similar runtime-only problems are **not**
/// reported here (there's no evaluation yet at this stage) — that's the
/// evaluator's job when it walks the resulting tree.
pub fn parse_arithmetic_expr(source: &str) -> Result<ArithExpr, ArithError> {
    let tokens = tokenize(source)?;
    if tokens.is_empty() {
        return Ok(ArithExpr::Number(0));
    }
    let source_len = source.len();
    let mut parser = Parser {
        tokens,
        idx: 0,
        source_len,
    };
    let expr = parser.parse_comma()?;
    if !parser.at_eof() {
        return Err(ArithError::UnexpectedToken {
            expected: "end of expression".to_string(),
            pos: parser.peek_pos(),
        });
    }
    Ok(expr)
}

// ---- tokenizer --------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Number(i64),
    Ident(String),
    LParen,
    RParen,
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    Percent,
    Bang,
    Tilde,
    Amp,
    AmpAmp,
    Pipe,
    PipePipe,
    Caret,
    Shl,
    Shr,
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    NotEq,
    Question,
    Colon,
    Comma,
    Assign,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    ShlEq,
    ShrEq,
    AmpEq,
    CaretEq,
    PipeEq,
    PlusPlus,
    MinusMinus,
}

fn tokenize(source: &str) -> Result<Vec<(Tok, usize)>, ArithError> {
    let mut scanner = ArithScanner::new(source);
    let mut tokens = Vec::new();
    loop {
        scanner.skip_whitespace();
        let start = scanner.pos;
        let Some(c) = scanner.peek() else { break };
        let tok = if c.is_ascii_digit() {
            let text = scanner.scan_number_token();
            Tok::Number(decode_number(text, start)?)
        } else if c == '_' || c.is_ascii_alphabetic() {
            Tok::Ident(scanner.scan_ident())
        } else {
            scanner.scan_punct(start)?
        };
        tokens.push((tok, start));
    }
    Ok(tokens)
}

struct ArithScanner<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> ArithScanner<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    /// Scans the maximal run of characters that could plausibly form a
    /// numeric literal in any of its forms (`123`, `010`, `0x1F`,
    /// `16#FF`, `2#101`, ...) — [`decode_number`] does the actual
    /// interpretation (and validation) afterward. Only called when
    /// `self.peek()` is already known to be an ASCII digit.
    fn scan_number_token(&mut self) -> &'a str {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('#') {
            // `base#n` (bash extension): the digit portion can include
            // letters/'@'/'_' (bash manual §6.5's digit-beyond-9 table).
            self.bump();
            while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '@')
            {
                self.bump();
            }
        } else if matches!(self.peek(), Some('x' | 'X')) && &self.input[start..self.pos] == "0" {
            self.bump();
            while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                self.bump();
            }
        }
        &self.input[start..self.pos]
    }

    /// Only called when `self.peek()` is already known to be `_` or an
    /// ASCII letter.
    fn scan_ident(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c == '_' || c.is_ascii_alphanumeric()) {
            self.bump();
        }
        self.input[start..self.pos].to_string()
    }

    fn scan_punct(&mut self, start: usize) -> Result<Tok, ArithError> {
        let c = self
            .bump()
            .expect("scan_punct only called when peek() is Some");
        let tok = match c {
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '?' => Tok::Question,
            ':' => Tok::Colon,
            ',' => Tok::Comma,
            '~' => Tok::Tilde,
            '+' => match self.peek() {
                Some('+') => {
                    self.bump();
                    Tok::PlusPlus
                }
                Some('=') => {
                    self.bump();
                    Tok::PlusEq
                }
                _ => Tok::Plus,
            },
            '-' => match self.peek() {
                Some('-') => {
                    self.bump();
                    Tok::MinusMinus
                }
                Some('=') => {
                    self.bump();
                    Tok::MinusEq
                }
                _ => Tok::Minus,
            },
            '*' => match self.peek() {
                Some('*') => {
                    self.bump();
                    Tok::StarStar
                }
                Some('=') => {
                    self.bump();
                    Tok::StarEq
                }
                _ => Tok::Star,
            },
            '/' => match self.peek() {
                Some('=') => {
                    self.bump();
                    Tok::SlashEq
                }
                _ => Tok::Slash,
            },
            '%' => match self.peek() {
                Some('=') => {
                    self.bump();
                    Tok::PercentEq
                }
                _ => Tok::Percent,
            },
            '<' => match self.peek() {
                Some('<') => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        Tok::ShlEq
                    } else {
                        Tok::Shl
                    }
                }
                Some('=') => {
                    self.bump();
                    Tok::Le
                }
                _ => Tok::Lt,
            },
            '>' => match self.peek() {
                Some('>') => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        Tok::ShrEq
                    } else {
                        Tok::Shr
                    }
                }
                Some('=') => {
                    self.bump();
                    Tok::Ge
                }
                _ => Tok::Gt,
            },
            '=' => match self.peek() {
                Some('=') => {
                    self.bump();
                    Tok::EqEq
                }
                _ => Tok::Assign,
            },
            '!' => match self.peek() {
                Some('=') => {
                    self.bump();
                    Tok::NotEq
                }
                _ => Tok::Bang,
            },
            '&' => match self.peek() {
                Some('&') => {
                    self.bump();
                    Tok::AmpAmp
                }
                Some('=') => {
                    self.bump();
                    Tok::AmpEq
                }
                _ => Tok::Amp,
            },
            '|' => match self.peek() {
                Some('|') => {
                    self.bump();
                    Tok::PipePipe
                }
                Some('=') => {
                    self.bump();
                    Tok::PipeEq
                }
                _ => Tok::Pipe,
            },
            '^' => match self.peek() {
                Some('=') => {
                    self.bump();
                    Tok::CaretEq
                }
                _ => Tok::Caret,
            },
            other => {
                return Err(ArithError::UnexpectedChar {
                    found: other,
                    pos: start,
                });
            }
        };
        Ok(tok)
    }
}

/// Decodes a numeric-literal token's raw text (as scanned by
/// [`ArithScanner::scan_number_token`]) into its wrapped 64-bit value —
/// see the module docs' "Integer width and overflow" section for the
/// exact accumulation semantics this reproduces.
fn decode_number(text: &str, pos: usize) -> Result<i64, ArithError> {
    let value: u64 =
        if let Some(digits) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            // Confirmed against real bash: `echo $((0x))` (no digits after
            // the prefix) prints 0 rather than erroring.
            parse_digits(digits, 16, pos, true)?
        } else if let Some(hash_idx) = text.find('#') {
            let base_str = &text[..hash_idx];
            let digits = &text[hash_idx + 1..];
            let base: u32 = base_str.parse().map_err(|_| ArithError::InvalidBase {
                base: base_str.to_string(),
                pos,
            })?;
            if !(2..=64).contains(&base) {
                return Err(ArithError::InvalidBase {
                    base: base.to_string(),
                    pos,
                });
            }
            // Unlike the 0x case: confirmed against real bash that `echo
            // $((2#))` (no digits after the base marker) errors rather than
            // defaulting to 0.
            parse_digits(digits, base, pos, false)?
        } else if text.len() > 1 && text.starts_with('0') {
            parse_digits(&text[1..], 8, pos, true)?
        } else {
            parse_digits(text, 10, pos, true)?
        };
    Ok(value as i64)
}

/// Accumulates `digits` (already known to contain only characters that
/// passed [`ArithScanner::scan_number_token`]'s coarse scan) as an
/// unsigned integer in the given `base`, using wrapping 64-bit arithmetic
/// throughout — matching real bash's behavior on an over-wide literal
/// (see the module docs).
fn parse_digits(
    digits: &str,
    base: u32,
    pos: usize,
    empty_is_zero: bool,
) -> Result<u64, ArithError> {
    if digits.is_empty() {
        if empty_is_zero {
            return Ok(0);
        }
        return Err(ArithError::UnexpectedEnd {
            expected: "at least one digit after the arithmetic base prefix".to_string(),
        });
    }
    let mut value: u64 = 0;
    for c in digits.chars() {
        let Some(d) = digit_value(c, base) else {
            return Err(ArithError::DigitOutOfRange {
                digit: c,
                base,
                pos,
            });
        };
        if d >= base {
            return Err(ArithError::DigitOutOfRange {
                digit: c,
                base,
                pos,
            });
        }
        value = value
            .wrapping_mul(u64::from(base))
            .wrapping_add(u64::from(d));
    }
    Ok(value)
}

/// Maps a single digit character to its value, per the bash manual §6.5
/// digit table: `0`-`9` are 0-9, lowercase `a`-`z` are 10-35, uppercase
/// `A`-`Z` are 36-61 *except* when `base <= 36` ("lowercase and uppercase
/// letters may be used interchangeably to represent numbers between 10
/// and 35" in that case — confirmed against real bash: `${36#Z}` is `35`,
/// the same as `${36#z}`, not the 61 the unconditional mapping would
/// give), `@` is 62, and `_` is 63. Returns `None` for any other
/// character (the caller still needs to separately check the returned
/// value against `base`).
fn digit_value(c: char, base: u32) -> Option<u32> {
    match c {
        '0'..='9' => Some(c as u32 - '0' as u32),
        'a'..='z' => Some(c as u32 - 'a' as u32 + 10),
        'A'..='Z' if base <= 36 => Some(c as u32 - 'A' as u32 + 10),
        'A'..='Z' => Some(c as u32 - 'A' as u32 + 36),
        '@' => Some(62),
        '_' => Some(63),
        _ => None,
    }
}

// ---- recursive-descent / precedence-climbing parser --------------------

struct Parser {
    tokens: Vec<(Tok, usize)>,
    idx: usize,
    source_len: usize,
}

impl Parser {
    fn at_eof(&self) -> bool {
        self.idx >= self.tokens.len()
    }

    fn peek_tok(&self) -> Option<&Tok> {
        self.tokens.get(self.idx).map(|(t, _)| t)
    }

    fn peek_pos(&self) -> usize {
        self.tokens
            .get(self.idx)
            .map_or(self.source_len, |(_, pos)| *pos)
    }

    fn expect(&mut self, want: &Tok, what: &str) -> Result<(), ArithError> {
        match self.peek_tok() {
            Some(t) if t == want => {
                self.idx += 1;
                Ok(())
            }
            Some(_) => Err(ArithError::UnexpectedToken {
                expected: what.to_string(),
                pos: self.peek_pos(),
            }),
            None => Err(ArithError::UnexpectedEnd {
                expected: what.to_string(),
            }),
        }
    }

    // ---- precedence chain, highest to lowest ---------------------------

    fn parse_primary(&mut self) -> Result<ArithExpr, ArithError> {
        let pos = self.peek_pos();
        match self.peek_tok() {
            Some(Tok::Number(_)) => {
                let Some((Tok::Number(n), _)) = self.tokens.get(self.idx).cloned() else {
                    unreachable!()
                };
                self.idx += 1;
                Ok(ArithExpr::Number(n))
            }
            Some(Tok::Ident(_)) => {
                let Some((Tok::Ident(name), _)) = self.tokens.get(self.idx).cloned() else {
                    unreachable!()
                };
                self.idx += 1;
                Ok(ArithExpr::Variable(name))
            }
            Some(Tok::LParen) => {
                self.idx += 1;
                let inner = self.parse_comma()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(inner)
            }
            Some(_) => Err(ArithError::UnexpectedToken {
                expected: "an operand".to_string(),
                pos,
            }),
            None => Err(ArithError::UnexpectedEnd {
                expected: "an operand".to_string(),
            }),
        }
    }

    fn parse_postfix(&mut self) -> Result<ArithExpr, ArithError> {
        let pos = self.peek_pos();
        let primary = self.parse_primary()?;
        match self.peek_tok() {
            Some(Tok::PlusPlus) => {
                let target = require_identifier(primary, "id++", pos)?;
                self.idx += 1;
                Ok(ArithExpr::IncrDecr {
                    op: IncrDecrOp::Increment,
                    target,
                    prefix: false,
                })
            }
            Some(Tok::MinusMinus) => {
                let target = require_identifier(primary, "id--", pos)?;
                self.idx += 1;
                Ok(ArithExpr::IncrDecr {
                    op: IncrDecrOp::Decrement,
                    target,
                    prefix: false,
                })
            }
            _ => Ok(primary),
        }
    }

    fn parse_unary(&mut self) -> Result<ArithExpr, ArithError> {
        let pos = self.peek_pos();
        match self.peek_tok() {
            Some(Tok::PlusPlus) => {
                self.idx += 1;
                let target = require_identifier(self.parse_unary()?, "++id", pos)?;
                Ok(ArithExpr::IncrDecr {
                    op: IncrDecrOp::Increment,
                    target,
                    prefix: true,
                })
            }
            Some(Tok::MinusMinus) => {
                self.idx += 1;
                let target = require_identifier(self.parse_unary()?, "--id", pos)?;
                Ok(ArithExpr::IncrDecr {
                    op: IncrDecrOp::Decrement,
                    target,
                    prefix: true,
                })
            }
            Some(Tok::Plus) => {
                self.idx += 1;
                Ok(ArithExpr::Unary {
                    op: ArithUnaryOp::Plus,
                    operand: Box::new(self.parse_unary()?),
                })
            }
            Some(Tok::Minus) => {
                self.idx += 1;
                Ok(ArithExpr::Unary {
                    op: ArithUnaryOp::Minus,
                    operand: Box::new(self.parse_unary()?),
                })
            }
            Some(Tok::Bang) => {
                self.idx += 1;
                Ok(ArithExpr::Unary {
                    op: ArithUnaryOp::LogicalNot,
                    operand: Box::new(self.parse_unary()?),
                })
            }
            Some(Tok::Tilde) => {
                self.idx += 1;
                Ok(ArithExpr::Unary {
                    op: ArithUnaryOp::BitNot,
                    operand: Box::new(self.parse_unary()?),
                })
            }
            _ => self.parse_postfix(),
        }
    }

    /// `**` — confirmed right-associative against real bash
    /// (`2**3**2` == `2**(3**2)` == 512, not `(2**3)**2` == 64), and
    /// binds tighter than `*`/`/`/`%` but looser than unary `+ - ! ~`
    /// (confirmed: `echo $((-2**2))` == 4, i.e. `(-2)**2`).
    fn parse_power(&mut self) -> Result<ArithExpr, ArithError> {
        let base = self.parse_unary()?;
        if matches!(self.peek_tok(), Some(Tok::StarStar)) {
            self.idx += 1;
            let exp = self.parse_power()?;
            Ok(ArithExpr::Binary {
                op: ArithBinaryOp::Power,
                lhs: Box::new(base),
                rhs: Box::new(exp),
            })
        } else {
            Ok(base)
        }
    }

    fn parse_left_assoc(
        &mut self,
        next: fn(&mut Self) -> Result<ArithExpr, ArithError>,
        op_for_tok: fn(&Tok) -> Option<ArithBinaryOp>,
    ) -> Result<ArithExpr, ArithError> {
        let mut lhs = next(self)?;
        while let Some(op) = self.peek_tok().and_then(op_for_tok) {
            self.idx += 1;
            let rhs = next(self)?;
            lhs = ArithExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_power, |t| match t {
            Tok::Star => Some(ArithBinaryOp::Mul),
            Tok::Slash => Some(ArithBinaryOp::Div),
            Tok::Percent => Some(ArithBinaryOp::Rem),
            _ => None,
        })
    }

    fn parse_additive(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_multiplicative, |t| match t {
            Tok::Plus => Some(ArithBinaryOp::Add),
            Tok::Minus => Some(ArithBinaryOp::Sub),
            _ => None,
        })
    }

    fn parse_shift(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_additive, |t| match t {
            Tok::Shl => Some(ArithBinaryOp::ShiftLeft),
            Tok::Shr => Some(ArithBinaryOp::ShiftRight),
            _ => None,
        })
    }

    fn parse_relational(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_shift, |t| match t {
            Tok::Lt => Some(ArithBinaryOp::Less),
            Tok::Le => Some(ArithBinaryOp::LessEq),
            Tok::Gt => Some(ArithBinaryOp::Greater),
            Tok::Ge => Some(ArithBinaryOp::GreaterEq),
            _ => None,
        })
    }

    fn parse_equality(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_relational, |t| match t {
            Tok::EqEq => Some(ArithBinaryOp::Eq),
            Tok::NotEq => Some(ArithBinaryOp::NotEq),
            _ => None,
        })
    }

    fn parse_bit_and(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_equality, |t| match t {
            Tok::Amp => Some(ArithBinaryOp::BitAnd),
            _ => None,
        })
    }

    fn parse_bit_xor(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_bit_and, |t| match t {
            Tok::Caret => Some(ArithBinaryOp::BitXor),
            _ => None,
        })
    }

    fn parse_bit_or(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_bit_xor, |t| match t {
            Tok::Pipe => Some(ArithBinaryOp::BitOr),
            _ => None,
        })
    }

    fn parse_logical_and(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_bit_or, |t| match t {
            Tok::AmpAmp => Some(ArithBinaryOp::LogicalAnd),
            _ => None,
        })
    }

    fn parse_logical_or(&mut self) -> Result<ArithExpr, ArithError> {
        self.parse_left_assoc(Self::parse_logical_and, |t| match t {
            Tok::PipePipe => Some(ArithBinaryOp::LogicalOr),
            _ => None,
        })
    }

    /// `cond ? if_true : if_false` — see [`ArithExpr::Conditional`]'s
    /// docs for the (confirmed-against-real-bash) precedence of each
    /// branch.
    fn parse_conditional(&mut self) -> Result<ArithExpr, ArithError> {
        let cond = self.parse_logical_or()?;
        if matches!(self.peek_tok(), Some(Tok::Question)) {
            self.idx += 1;
            let if_true = self.parse_comma()?;
            self.expect(&Tok::Colon, "':'")?;
            let if_false = self.parse_conditional()?;
            Ok(ArithExpr::Conditional {
                cond: Box::new(cond),
                if_true: Box::new(if_true),
                if_false: Box::new(if_false),
            })
        } else {
            Ok(cond)
        }
    }

    /// Assignment — right-associative, and only valid with a bare
    /// identifier target. Mirrors the standard C-grammar trick for
    /// parsing `assignment-expression` without backtracking: parse a
    /// full `conditional-expression` first, then reinterpret it as an
    /// assignment target only if an assignment operator actually
    /// follows.
    fn parse_assignment(&mut self) -> Result<ArithExpr, ArithError> {
        let pos = self.peek_pos();
        let lhs = self.parse_conditional()?;
        let Some(op) = self.peek_tok().and_then(assign_op_for_tok) else {
            return Ok(lhs);
        };
        let target = require_identifier(lhs, "assignment", pos)?;
        self.idx += 1;
        let value = self.parse_assignment()?;
        Ok(ArithExpr::Assign {
            op,
            target,
            value: Box::new(value),
        })
    }

    /// The top-level production — POSIX/C's comma operator, lowest
    /// precedence.
    fn parse_comma(&mut self) -> Result<ArithExpr, ArithError> {
        let mut expr = self.parse_assignment()?;
        while matches!(self.peek_tok(), Some(Tok::Comma)) {
            self.idx += 1;
            let next = self.parse_assignment()?;
            expr = ArithExpr::Comma {
                first: Box::new(expr),
                second: Box::new(next),
            };
        }
        Ok(expr)
    }
}

fn assign_op_for_tok(t: &Tok) -> Option<ArithAssignOp> {
    Some(match t {
        Tok::Assign => ArithAssignOp::Assign,
        Tok::PlusEq => ArithAssignOp::AddAssign,
        Tok::MinusEq => ArithAssignOp::SubAssign,
        Tok::StarEq => ArithAssignOp::MulAssign,
        Tok::SlashEq => ArithAssignOp::DivAssign,
        Tok::PercentEq => ArithAssignOp::RemAssign,
        Tok::ShlEq => ArithAssignOp::ShiftLeftAssign,
        Tok::ShrEq => ArithAssignOp::ShiftRightAssign,
        Tok::AmpEq => ArithAssignOp::BitAndAssign,
        Tok::CaretEq => ArithAssignOp::BitXorAssign,
        Tok::PipeEq => ArithAssignOp::BitOrAssign,
        _ => return None,
    })
}

/// Extracts the identifier name from `expr`, for the operators (`++`,
/// `--`, assignment) that require a bare-identifier (lvalue) operand.
fn require_identifier(expr: ArithExpr, op: &'static str, pos: usize) -> Result<String, ArithError> {
    match expr {
        ArithExpr::Variable(name) => Ok(name),
        _ => Err(ArithError::NotAnIdentifier { op, pos }),
    }
}

#[cfg(test)]
mod tests;
