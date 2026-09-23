//! Token and word types produced by [`crate::lex`].
//!
//! The most important design decision in this module is [`WordSegment`]:
//! rather than collapsing a word to a flat `String`, each word is a
//! sequence of segments that each carry their own quoting context. This is
//! what lets the expansion engine (`conch-core`) implement POSIX word
//! expansion correctly instead of by accident — see the crate-level docs
//! for the full rationale.

/// A byte-offset span into the original input string, `[start, end)`.
///
/// Offsets are byte offsets (not char offsets), matching `str` indexing,
/// so they remain valid for slicing the original input directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "span start must not exceed end");
        Self { start, end }
    }
}

/// A single lexical token, per POSIX 2.10.1 Shell Grammar Lexical
/// Conventions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    #[must_use]
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// The kind of a lexical token.
///
/// This intentionally mirrors POSIX's token categories rather than
/// bash's: `WORD`, `ASSIGNMENT_WORD`-shaped words (still just `WORD` here
/// — see the crate-level docs on why assignment recognition is left to
/// the parser), `IO_NUMBER`, `NEWLINE`, and the operator tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// A `WORD` token: any maximal run of non-blank, non-newline,
    /// non-operator characters, with quoting resolved into [`Word`]'s
    /// structured segments.
    Word(Word),
    /// An `IO_NUMBER` token: a run of digits immediately (no intervening
    /// blank) followed by `<` or `>`, recognized per POSIX 2.10.1 rule 3.
    /// Carries just the numeric value; the following operator is its own
    /// token.
    IoNumber(u32),
    /// One of the fixed shell operator tokens (POSIX 2.10.1).
    Operator(Operator),
    /// A `NEWLINE` token — significant in the grammar (it can terminate a
    /// `list`, same as `;`), unlike a blank.
    Newline,
}

/// The fixed set of POSIX shell operator tokens (2.10.1), recognized by
/// longest match ("maximal munch").
///
/// This lexer recognizes the *complete* POSIX operator set even though
/// Phase 1's parser only implements a subset (`|`, `;`, `&&`, `||`, `&`,
/// `<`, `>`, `>>`) — see the crate-level docs for why: getting tokenization
/// right for constructs the parser doesn't support yet is what keeps the
/// parser from either choking on or silently mis-parsing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    /// `|`
    Pipe,
    /// `||`
    OrIf,
    /// `&`
    Amp,
    /// `&&`
    AndIf,
    /// `;`
    Semi,
    /// `;;` — bash extension point: case-statement terminator (Phase 3).
    DSemi,
    /// `<`
    Less,
    /// `>`
    Great,
    /// `<<` — here-document (Phase 2).
    DLess,
    /// `<<-` — here-document with leading-tab stripping (Phase 2).
    DLessDash,
    /// `<&` — fd-duplicating input redirection (Phase 2).
    LessAnd,
    /// `>&` — fd-duplicating output redirection (Phase 2).
    GreatAnd,
    /// `>>`
    DGreat,
    /// `<>` — open for read+write (Phase 2).
    LessGreat,
    /// `>|` — clobber-override output redirection (Phase 2).
    Clobber,
    /// `(` — lexed as a metacharacter per POSIX's operator-character set
    /// even though subshells aren't parsed until Phase 3; see crate docs.
    LParen,
    /// `)`
    RParen,
}

/// A shell word: a sequence of segments, each carrying its own quoting
/// context.
///
/// See the crate-level documentation for the full contract this type
/// exists to provide.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    pub segments: Vec<WordSegment>,
}

impl Word {
    #[must_use]
    pub fn new(segments: Vec<WordSegment>) -> Self {
        Self { segments }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Returns this word's text if and only if it consists entirely of a
    /// single unquoted, expansion-free literal segment.
    ///
    /// This is useful for the parser's own bookkeeping (e.g. recognizing
    /// `ASSIGNMENT_WORD` shapes, or comparing a word against a reserved
    /// word in a later phase) — it is deliberately *not* a general
    /// "flatten this word to a string" helper, because for any word
    /// containing quoting or expansion sites there is no single string
    /// that would be correct to hand back before expansion runs.
    #[must_use]
    pub fn as_plain_literal(&self) -> Option<&str> {
        match self.segments.as_slice() {
            [WordSegment::Literal(text)] => Some(text),
            [] => Some(""),
            _ => None,
        }
    }
}

/// One segment of a [`Word`]. Each variant is tagged with the quoting
/// context POSIX 2.6.5 (word splitting) and 2.6.6 (pathname expansion)
/// need in order to decide splitting/globbing eligibility later:
///
/// - [`WordSegment::Literal`], [`WordSegment::Parameter`],
///   [`WordSegment::ComplexParameterExpansion`],
///   [`WordSegment::CommandSubstitution`], and
///   [`WordSegment::ArithmeticExpansion`], when they appear **directly** in
///   a [`Word::segments`] list (i.e. not nested inside a
///   [`WordSegment::DoubleQuoted`]), are unquoted: after expansion, their
///   *result* is eligible for field splitting and pathname expansion.
/// - [`WordSegment::SingleQuoted`] and [`WordSegment::DoubleQuoted`] are
///   never themselves split or glob-expanded, regardless of what they
///   contain.
/// - The segments nested inside a [`WordSegment::DoubleQuoted`] reuse this
///   same enum (so parameter/command/arithmetic expansion sites still get
///   recognized inside `"..."`, per POSIX 2.2.3), but by construction only
///   [`WordSegment::Literal`], [`WordSegment::Parameter`],
///   [`WordSegment::ComplexParameterExpansion`],
///   [`WordSegment::CommandSubstitution`], and
///   [`WordSegment::ArithmeticExpansion`] ever appear there — a nested
///   [`WordSegment::SingleQuoted`] or [`WordSegment::DoubleQuoted`] is
///   impossible because single quotes carry no special meaning inside
///   double quotes and a second unescaped `"` always closes the outer
///   double-quoted segment instead of opening a new one.
///
/// Backslash-escape resolution (the character-substitution half of POSIX
/// 2.6.7 quote removal) has already happened by the time you have a
/// `WordSegment`: an unquoted `\X` has already become literal `X`, and a
/// double-quoted `\"`/`\$`/`` \` ``/`\\` has already become the literal
/// escaped character (with backslash-newline removed entirely, in both
/// contexts, as line continuation). This is safe to do this early because
/// it is purely syntactic and never depends on any expansion result. What
/// remains for the *expansion engine's* quote-removal step is simpler:
/// once a word's expansion sites have been resolved, drop the
/// [`WordSegment::SingleQuoted`]/[`WordSegment::DoubleQuoted`] wrapper
/// structure (it has already done its job of marking non-split/non-glob
/// segments) and concatenate every segment's text into the final field(s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordSegment {
    /// Unquoted literal text, with backslash-escapes already resolved.
    /// May still contain pattern-matching metacharacters (`*`, `?`,
    /// `[...]`) that pathname expansion (Phase 2) interprets.
    Literal(String),

    /// `'...'` — POSIX 2.2.2: every character between the quotes is
    /// literal, with **no** escape processing (not even `\'`); the
    /// segment ends at the first `'`. Never split or glob-expanded.
    SingleQuoted(String),

    /// `"..."` — POSIX 2.2.3: nested segments may include literal text
    /// (with the double-quote escape set resolved) and parameter/command/
    /// arithmetic expansion sites. The result of the *whole* segment is
    /// never split or pathname-expanded, and a single quote inside it has
    /// no special meaning. See this enum's top-level docs for the
    /// invariant on what can appear inside.
    DoubleQuoted(Vec<WordSegment>),

    /// `$name`, `$1`, `$?`, `${name}`, `${1}`, `${?}`, etc — a parameter
    /// reference with **no** operator suffix and no `${#name}` length
    /// prefix. This is the only parameter-expansion shape Phase 1's
    /// expansion engine evaluates; see [`ComplexParameterExpansion`] for
    /// everything else.
    ///
    /// [`ComplexParameterExpansion`]: WordSegment::ComplexParameterExpansion
    Parameter(Parameter),

    /// `${...}` where the body is anything beyond a bare parameter name:
    /// a POSIX 2.6.2 operator (`${var:-word}`, `${var#pattern}`, ...), the
    /// `${#name}` string-length form, or a bash-extension operator
    /// (`${var/pat/repl}`, `${var^^}`, ...). This crate only establishes
    /// the correct closing-`}` boundary — POSIX 2.6.3's "tokenizing rules
    /// shall be applied recursively to find the matching" delimiter,
    /// which also governs `${...}` nesting — and captures the raw text
    /// between the braces **verbatim**, so Phase 2 can parse it without
    /// needing any lexer changes. Decoding the operator itself is
    /// out of scope for Phase 1.
    ComplexParameterExpansion(String),

    /// `$(...)` or `` `...` `` — POSIX 2.6.3. The body is captured
    /// **verbatim** (not lexed or parsed); Phase 2 is expected to
    /// recursively invoke `conch-shell-parser`'s own `parse` on it to
    /// evaluate the substitution. See [`CommandSubstitution`] for the
    /// boundary-finding rules this crate already applies.
    CommandSubstitution(CommandSubstitution),

    /// `$((...))` — POSIX 2.6.4. The body is captured **verbatim**;
    /// evaluating it as an arithmetic expression is Phase 2 scope. Per
    /// POSIX 2.6.4, "arithmetic expansion has precedence" over a command
    /// substitution containing a nested subshell — this crate always
    /// prefers the arithmetic reading of `$((`, matching that rule; see
    /// the crate-level docs for the one case this simplifies away.
    ArithmeticExpansion(String),
}

/// `$(...)` vs `` `...` `` — both are POSIX 2.6.3 command substitution,
/// but they have different internal quoting/escaping rules, so the style
/// is preserved for Phase 2 to re-lex the body correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstitutionStyle {
    /// `$(...)`
    DollarParen,
    /// `` `...` ``
    Backtick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSubstitution {
    pub style: SubstitutionStyle,
    /// The verbatim source text between the delimiters (not including
    /// them).
    pub body: String,
}

/// A shell parameter reference, POSIX 2.5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parameter {
    /// An ordinary shell variable name (POSIX 2.5.3), from `$name` or
    /// `${name}`.
    Name(String),
    /// A positional parameter (POSIX 2.5.1): `$1`..`$9` unbraced (exactly
    /// one digit is ever consumed unbraced — `$12` is positional
    /// parameter 1 followed by the literal digit `2`), or any number of
    /// digits when braced (`${10}`, `${11}`, ...).
    Positional(u32),
    /// One of the fixed single-character special parameters (POSIX
    /// 2.5.2).
    Special(SpecialParameter),
}

/// The fixed set of POSIX 2.5.2 special parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialParameter {
    /// `$@`
    At,
    /// `$*`
    Star,
    /// `$#`
    Hash,
    /// `$?`
    Question,
    /// `$-`
    Dash,
    /// `$$`
    Dollar,
    /// `$!`
    Bang,
    /// `$0`
    Zero,
}
