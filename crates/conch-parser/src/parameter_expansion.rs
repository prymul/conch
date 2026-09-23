//! Phase 2: decodes the POSIX 2.6.2 parameter-expansion *operator* out of
//! a `${...}` body.
//!
//! `conch-shell-lexer` already finds the correct extent of every `${...}`
//! construct; when the body is anything beyond a bare parameter name (its
//! own fast path — see [`WordSegment::Parameter`]), it captures that body
//! **verbatim** as [`WordSegment::ComplexParameterExpansion`]'s payload
//! rather than decoding it — see that variant's docs for why. This module
//! is what does the decoding: [`parse_parameter_expansion`] turns the raw
//! body into a [`ParameterExpansion`] — which parameter, which POSIX
//! 2.6.2 operator, and (recursively, since an operand can itself contain
//! quoting and further expansion sites, e.g. `${x:-$OTHER}`) the operand
//! as [`Word`] segments the evaluator can hand straight to its own
//! expansion pipeline.
//!
//! Only the POSIX baseline operator set is decoded here: `${parameter}`
//! (never actually reaches this module — the lexer's own fast path
//! already turns it into [`WordSegment::Parameter`]), `${parameter:-word}`
//! / `${parameter-word}`, `${parameter:=word}` / `${parameter=word}`,
//! `${parameter:?word}` / `${parameter?word}`, `${parameter:+word}` /
//! `${parameter+word}`, `${#parameter}`, `${parameter#word}` /
//! `${parameter##word}`, `${parameter%word}` / `${parameter%%word}`.
//! Bash extensions beyond that (`${var/pat/repl}`, `${var^^}`,
//! `${var,,}`, `${!prefix*}`, `${var@Q}`, ...) are recognized as *not*
//! POSIX baseline and rejected with [`ParamExpansionError::UnrecognizedOperator`]
//! rather than silently mis-parsed — a future bash-mode extension to this
//! module, not something this pass guesses at.
//!
//! # Why this needs to know its surrounding quoting context
//!
//! A `${...}` operand can contain a `'...'` — and, confirmed against real
//! bash, **whether that single-quote actually quotes anything depends on
//! whether the enclosing `${...}` is itself nested inside an outer
//! `"..."`**, even though `conch-shell-lexer`'s own boundary-finding
//! (locating the operand's correct end) doesn't care either way:
//!
//! ```text
//! x is unset in both cases
//! echo ${x:-'a b'}      ->  a b       (real single-quoting; quote removed)
//! echo "${x:-'a b'}"    ->  'a b'     (literal quote characters survive!)
//! ```
//!
//! This is POSIX 2.2.3's "a single-quote loses its special meaning within
//! double-quotes" reaching straight through `${...}` nesting — a nested
//! `"..."` is *not* affected the same way (`echo "${x:-"y"}"` still
//! prints `y`, not `"y"`; see [`conch_shell_lexer::lex_double_quoted_body`]'s
//! docs for the fuller worked example and the POSIX 2.6.2/2.6.3
//! "tokenizing rules applied recursively" citation for why double quotes
//! get to re-open there anyway). Since [`WordSegment::ComplexParameterExpansion`]
//! carries only the raw body string — no positional/context information —
//! every entry point in this module takes an explicit `in_double_quotes`
//! flag instead of trying to infer it. Callers walking a [`Word`] tree
//! already know the answer for free: `false` while walking
//! [`Word::segments`] directly, `true` inside the recursive case for
//! [`WordSegment::DoubleQuoted`]'s inner segments. Getting this backwards
//! doesn't error — it silently produces the wrong operand text, exactly
//! the "almost-right" failure mode this project treats as a correctness
//! bug, not a cosmetic one.
//!
//! [`WordSegment::Parameter`]: conch_shell_lexer::WordSegment::Parameter
//! [`WordSegment::ComplexParameterExpansion`]: conch_shell_lexer::WordSegment::ComplexParameterExpansion
//! [`WordSegment::DoubleQuoted`]: conch_shell_lexer::WordSegment::DoubleQuoted

use conch_shell_lexer::{LexError, Parameter, Word, match_bare_parameter};

/// A decoded POSIX 2.6.2 `${...}` parameter expansion: the parameter
/// being referenced, and which operator (if any) modifies it.
///
/// This is what a [`WordSegment::ComplexParameterExpansion`]'s raw body
/// decodes into; see [`parse_parameter_expansion`].
///
/// [`WordSegment::ComplexParameterExpansion`]: conch_shell_lexer::WordSegment::ComplexParameterExpansion
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterExpansion {
    pub parameter: Parameter,
    pub operator: ParameterOperator,
}

/// Whether an operator's `:`-prefixed ("colon") form was used.
///
/// POSIX 2.6.2: "If the colon ':' is omitted... a test [is applied]
/// only for a parameter that is unset. If the colon is included, the
/// shell shall test for a parameter that is unset or null." Only
/// [`ParameterOperator::UseDefault`], [`ParameterOperator::AssignDefault`],
/// [`ParameterOperator::ErrorIfUnsetOrNull`], and
/// [`ParameterOperator::UseAlternative`] have this colon distinction —
/// `#`/`##`/`%`/`%%` never take a leading `:` (confirmed against real
/// bash; there is no null-vs-unset distinction to make for pattern
/// removal, since removing a pattern from an empty/unset value is
/// already a no-op either way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullMode {
    /// No leading `:` — the operator triggers only when the parameter is
    /// unset.
    UnsetOnly,
    /// Leading `:` — the operator triggers when the parameter is unset
    /// *or* set to the null (empty) string.
    UnsetOrNull,
}

/// The POSIX 2.6.2 baseline parameter-expansion operators. See this
/// module's docs for which bash extensions beyond this set are
/// deliberately *not* decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterOperator {
    /// `${#parameter}` — the string length, in characters, of
    /// `parameter`'s value (evaluator's job to compute; bash treats
    /// `parameter` being `*`/`@` as returning the positional-parameter
    /// count rather than a joined-string length — POSIX leaves that case
    /// unspecified, so this parser just structurally represents "length
    /// of whatever this parameter is" and leaves the semantics to the
    /// evaluator).
    Length,
    /// `${parameter:-word}` / `${parameter-word}` — substitute
    /// `parameter`'s value, or `word`'s expansion if `parameter` is
    /// unset (colon form: unset or null).
    UseDefault { null_mode: NullMode, word: Word },
    /// `${parameter:=word}` / `${parameter=word}` — like
    /// [`Self::UseDefault`], but also assigns `word`'s expansion to
    /// `parameter` when the substitution triggers. POSIX: invalid if
    /// `parameter` is a positional or special parameter — that's an
    /// evaluation-time restriction (this parser accepts any
    /// [`Parameter`] here structurally; it has no shell state to check
    /// "is this actually assignable" against).
    AssignDefault { null_mode: NullMode, word: Word },
    /// `${parameter:?word}` / `${parameter?word}` — substitute
    /// `parameter`'s value; if unset (colon form: unset or null), write
    /// `word`'s expansion (or a default diagnostic, if `word` is empty)
    /// to standard error and treat this as an expansion error.
    ErrorIfUnsetOrNull { null_mode: NullMode, word: Word },
    /// `${parameter:+word}` / `${parameter+word}` — substitute `word`'s
    /// expansion if `parameter` is set (colon form: set and non-null);
    /// otherwise substitute nothing (**not** `parameter`'s own value —
    /// this operator never uses it).
    UseAlternative { null_mode: NullMode, word: Word },
    /// `${parameter#pattern}` / `${parameter##pattern}` — remove the
    /// smallest (`#`) or largest (`##`, "greedy") prefix of
    /// `parameter`'s value that matches `pattern` (POSIX 2.13
    /// pattern-matching notation, evaluated after `pattern` itself is
    /// expanded — matching, and what "smallest"/"largest" mean, is the
    /// evaluator's job).
    RemovePrefix { greedy: bool, pattern: Word },
    /// `${parameter%pattern}` / `${parameter%%pattern}` — remove the
    /// smallest (`%`) or largest (`%%`, "greedy") suffix of
    /// `parameter`'s value that matches `pattern`.
    RemoveSuffix { greedy: bool, pattern: Word },
}

/// An error encountered decoding a `${...}` body into a
/// [`ParameterExpansion`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParamExpansionError {
    /// The body doesn't start with a valid parameter name/positional
    /// digit/special-parameter character at all (e.g. `${}`, or `${!x}`
    /// — `!`-prefixed indirect expansion is a bash extension not decoded
    /// here).
    #[error("`${{{body}}}` is not a valid parameter name or expansion")]
    InvalidParameter { body: String },
    /// The text after the parameter name doesn't start a recognized
    /// POSIX 2.6.2 operator — either genuinely invalid syntax, or a bash
    /// extension operator this module doesn't decode (see the module
    /// docs).
    #[error(
        "`${{{body}}}`: not a recognized POSIX parameter-expansion operator (bash extensions like `${{var/pat/repl}}` or `${{var^^}}` aren't supported yet)"
    )]
    UnrecognizedOperator { body: String },
    /// Re-lexing an operand word (the `word`/`pattern` after the
    /// operator) failed. In practice this should never happen for a body
    /// `conch-shell-lexer` already bounded correctly — see
    /// [`conch_shell_lexer::lex_word_body`]'s docs — but the error is
    /// still surfaced rather than panicking.
    #[error(transparent)]
    Lex(#[from] LexError),
}

/// Decodes the raw body of a [`WordSegment::ComplexParameterExpansion`]
/// into a structured [`ParameterExpansion`].
///
/// `in_double_quotes` must be `true` iff the `${...}` this body came from
/// is itself one of a [`WordSegment::DoubleQuoted`]'s inner segments, and
/// `false` if it appears directly in a [`Word`]'s top-level `segments` —
/// see this module's docs for why that distinction is load-bearing for
/// correctness, not cosmetic.
///
/// # Errors
///
/// Returns [`ParamExpansionError`] if `body` isn't a recognized POSIX
/// 2.6.2 parameter name plus operator, including when it uses a bash
/// extension operator beyond the POSIX baseline this function decodes.
///
/// [`WordSegment::ComplexParameterExpansion`]: conch_shell_lexer::WordSegment::ComplexParameterExpansion
/// [`WordSegment::DoubleQuoted`]: conch_shell_lexer::WordSegment::DoubleQuoted
pub fn parse_parameter_expansion(
    body: &str,
    in_double_quotes: bool,
) -> Result<ParameterExpansion, ParamExpansionError> {
    // `${#parameter}` — the string-length form. POSIX requires the
    // *entire* body (after the leading '#') to be exactly a bare
    // parameter, with nothing else following — confirmed against real
    // bash: `${#x#l}` and `${#x:-def}` are both "bad substitution", not
    // length-of-x with a stray operator tacked on. `match_bare_parameter`
    // reporting how many bytes it consumed is exactly what makes "well,
    // is there anything left over" checkable here.
    if let Some(rest) = body.strip_prefix('#')
        && let Some((parameter, len)) = match_bare_parameter(rest, true)
        && len == rest.len()
    {
        return Ok(ParameterExpansion {
            parameter,
            operator: ParameterOperator::Length,
        });
    }

    // Not the length form (either the body doesn't start with '#', or it
    // does but what follows isn't *exactly* a bare parameter) — fall
    // through to ordinary parameter-plus-operator parsing over the whole
    // (untouched) body. Note this correctly re-matches a leading '#' as
    // the `$#` special parameter itself when the length form didn't
    // apply (e.g. `${#x#l}`: parameter is Special(Hash), and the
    // operator parser below then rejects "x#l" as not starting with a
    // valid operator character) — exactly matching real bash's fallback.
    let (parameter, consumed) =
        match_bare_parameter(body, true).ok_or_else(|| ParamExpansionError::InvalidParameter {
            body: body.to_string(),
        })?;
    let operator = parse_operator(&body[consumed..], in_double_quotes, body)?;
    Ok(ParameterExpansion {
        parameter,
        operator,
    })
}

/// Parses everything after the parameter name: the operator and its
/// operand. `whole_body` is only for error messages (the original,
/// un-sliced `${...}` body).
fn parse_operator(
    rest: &str,
    in_double_quotes: bool,
    whole_body: &str,
) -> Result<ParameterOperator, ParamExpansionError> {
    let unrecognized = || ParamExpansionError::UnrecognizedOperator {
        body: whole_body.to_string(),
    };

    // '#'/'##' and '%'/'%%' never take a leading ':' — check these first
    // and unconditionally, since POSIX only defines the colon
    // (null-vs-unset) distinction for '-' '=' '?' '+' below.
    if let Some(pattern_body) = rest.strip_prefix("##") {
        return Ok(ParameterOperator::RemovePrefix {
            greedy: true,
            pattern: parse_operand(pattern_body, in_double_quotes)?,
        });
    }
    if let Some(pattern_body) = rest.strip_prefix('#') {
        return Ok(ParameterOperator::RemovePrefix {
            greedy: false,
            pattern: parse_operand(pattern_body, in_double_quotes)?,
        });
    }
    if let Some(pattern_body) = rest.strip_prefix("%%") {
        return Ok(ParameterOperator::RemoveSuffix {
            greedy: true,
            pattern: parse_operand(pattern_body, in_double_quotes)?,
        });
    }
    if let Some(pattern_body) = rest.strip_prefix('%') {
        return Ok(ParameterOperator::RemoveSuffix {
            greedy: false,
            pattern: parse_operand(pattern_body, in_double_quotes)?,
        });
    }

    let (null_mode, op_body) = match rest.strip_prefix(':') {
        Some(after_colon) => (NullMode::UnsetOrNull, after_colon),
        None => (NullMode::UnsetOnly, rest),
    };

    let op_char = op_body.chars().next().ok_or_else(unrecognized)?;
    let word = parse_operand(&op_body[op_char.len_utf8()..], in_double_quotes)?;
    match op_char {
        '-' => Ok(ParameterOperator::UseDefault { null_mode, word }),
        '=' => Ok(ParameterOperator::AssignDefault { null_mode, word }),
        '?' => Ok(ParameterOperator::ErrorIfUnsetOrNull { null_mode, word }),
        '+' => Ok(ParameterOperator::UseAlternative { null_mode, word }),
        _ => Err(unrecognized()),
    }
}

/// Re-lexes a `${parameter:-word}`-shaped operand (the `word`/`pattern`
/// after the operator), picking whichever of `conch-shell-lexer`'s two
/// operand-relexing rulesets matches the enclosing `${...}`'s own
/// quoting context. See [`parse_parameter_expansion`]'s docs for why
/// `in_double_quotes` matters.
fn parse_operand(text: &str, in_double_quotes: bool) -> Result<Word, ParamExpansionError> {
    if in_double_quotes {
        Ok(Word::new(conch_shell_lexer::lex_double_quoted_body(text)?))
    } else {
        Ok(conch_shell_lexer::lex_word_body(text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conch_shell_lexer::{SpecialParameter, WordSegment};

    fn word(segments: Vec<WordSegment>) -> Word {
        Word::new(segments)
    }

    fn lit(s: &str) -> Word {
        word(vec![WordSegment::Literal(s.into())])
    }

    fn name(s: &str) -> Parameter {
        Parameter::Name(s.into())
    }

    // ---- string length ----------------------------------------------------

    #[test]
    fn length_of_bare_name() {
        assert_eq!(
            parse_parameter_expansion("#x", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::Length,
            }
        );
    }

    #[test]
    fn length_of_special_hash_via_doubled_hash() {
        // ${##} -- confirmed against real bash: length of $#.
        assert_eq!(
            parse_parameter_expansion("##", false).unwrap(),
            ParameterExpansion {
                parameter: Parameter::Special(SpecialParameter::Hash),
                operator: ParameterOperator::Length,
            }
        );
    }

    #[test]
    fn length_of_positional() {
        assert_eq!(
            parse_parameter_expansion("#10", false).unwrap(),
            ParameterExpansion {
                parameter: Parameter::Positional(10),
                operator: ParameterOperator::Length,
            }
        );
    }

    #[test]
    fn length_form_rejects_trailing_operator() {
        // ${#x#l} and ${#x:-def} are both "bad substitution" in real
        // bash, not length-of-x with a stray operator.
        assert!(matches!(
            parse_parameter_expansion("#x#l", false),
            Err(ParamExpansionError::UnrecognizedOperator { .. })
        ));
        assert!(matches!(
            parse_parameter_expansion("#x:-def", false),
            Err(ParamExpansionError::UnrecognizedOperator { .. })
        ));
    }

    #[test]
    fn length_of_special_dash() {
        // ${#-} -- IS the length form: '-' alone is a bare (special)
        // parameter that consumes the *entire* remainder after '#', so
        // this is length-of-$-, not Special(Hash) with some operator.
        // Confirmed against real bash: $- is "hBc" (3 chars) in a
        // typical `bash -c` invocation, and `echo ${#-}` prints `3`,
        // matching ${##} being a *different* value (length of $#, a
        // single digit) rather than colliding with it.
        assert_eq!(
            parse_parameter_expansion("#-", false).unwrap(),
            ParameterExpansion {
                parameter: Parameter::Special(SpecialParameter::Dash),
                operator: ParameterOperator::Length,
            }
        );
    }

    #[test]
    fn hash_special_parameter_with_default_operator() {
        // ${#-x} -- NOT length form: the text after '#' is "-x", and
        // '-' alone only matches *part* of that (leftover "x" remains),
        // so the length form doesn't apply and this falls back to
        // Special(Hash) with the UseDefault operator. Confirmed against
        // real bash: `echo "${#-x}"` prints `0` (the value of $#, since
        // $# is always "set" so the "x" default is never actually
        // triggered) rather than erroring or printing `x` -- i.e. bash
        // really did parse the parameter as $#, not attempt (and fail)
        // the length form.
        assert_eq!(
            parse_parameter_expansion("#-x", false).unwrap(),
            ParameterExpansion {
                parameter: Parameter::Special(SpecialParameter::Hash),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOnly,
                    word: lit("x"),
                },
            }
        );
    }

    // ---- default / assign-default / error / alternative -------------------

    #[test]
    fn use_default_colon_and_non_colon_forms() {
        assert_eq!(
            parse_parameter_expansion("x:-fallback", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: lit("fallback"),
                },
            }
        );
        assert_eq!(
            parse_parameter_expansion("x-fallback", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOnly,
                    word: lit("fallback"),
                },
            }
        );
    }

    #[test]
    fn empty_word_is_valid() {
        assert_eq!(
            parse_parameter_expansion("x:-", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: word(vec![]),
                },
            }
        );
    }

    #[test]
    fn assign_default() {
        assert_eq!(
            parse_parameter_expansion("x:=fallback", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::AssignDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: lit("fallback"),
                },
            }
        );
    }

    #[test]
    fn error_if_unset_or_null() {
        assert_eq!(
            parse_parameter_expansion("x:?required", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::ErrorIfUnsetOrNull {
                    null_mode: NullMode::UnsetOrNull,
                    word: lit("required"),
                },
            }
        );
    }

    #[test]
    fn use_alternative() {
        assert_eq!(
            parse_parameter_expansion("x:+alt", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseAlternative {
                    null_mode: NullMode::UnsetOrNull,
                    word: lit("alt"),
                },
            }
        );
    }

    // ---- prefix / suffix removal -------------------------------------------

    #[test]
    fn remove_prefix_smallest_and_greedy() {
        assert_eq!(
            parse_parameter_expansion("x#*/", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::RemovePrefix {
                    greedy: false,
                    pattern: lit("*/"),
                },
            }
        );
        assert_eq!(
            parse_parameter_expansion("x##*/", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::RemovePrefix {
                    greedy: true,
                    pattern: lit("*/"),
                },
            }
        );
    }

    #[test]
    fn remove_suffix_smallest_and_greedy() {
        assert_eq!(
            parse_parameter_expansion("x%.*", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::RemoveSuffix {
                    greedy: false,
                    pattern: lit(".*"),
                },
            }
        );
        assert_eq!(
            parse_parameter_expansion("x%%.*", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::RemoveSuffix {
                    greedy: true,
                    pattern: lit(".*"),
                },
            }
        );
    }

    // ---- operand re-lexing: nested expansion / quoting ---------------------

    #[test]
    fn operand_word_can_contain_nested_expansion() {
        // ${x:-$OTHER}
        assert_eq!(
            parse_parameter_expansion("x:-$OTHER", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: word(vec![WordSegment::Parameter(name("OTHER"))]),
                },
            }
        );
    }

    #[test]
    fn operand_word_does_not_split_on_unescaped_space_at_parse_time() {
        // Whether "a b" is later split into two fields is the
        // *evaluator's* job (POSIX 2.6.5, after this expansion runs);
        // this parser must capture it as one literal operand.
        assert_eq!(
            parse_parameter_expansion("x:-a b", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: lit("a b"),
                },
            }
        );
    }

    #[test]
    fn unquoted_context_single_quote_in_operand_really_quotes() {
        // Confirmed against real bash: unquoted `${x:-'a b' c}` keeps
        // 'a b' as one (unsplit) unit via real single-quoting.
        assert_eq!(
            parse_parameter_expansion("x:-'a b' c", false).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: word(vec![
                        WordSegment::SingleQuoted("a b".into()),
                        WordSegment::Literal(" c".into()),
                    ]),
                },
            }
        );
    }

    #[test]
    fn double_quoted_context_single_quote_in_operand_is_literal() {
        // Confirmed against real bash: "${x:-'a b'}" (nested in "...")
        // prints the literal quote characters — see this module's docs.
        assert_eq!(
            parse_parameter_expansion("x:-'a b'", true).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: lit("'a b'"),
                },
            }
        );
    }

    #[test]
    fn double_quoted_context_nested_double_quote_still_quotes() {
        // Confirmed against real bash: "${x:-"y"}" prints `y`, not `"y"`.
        assert_eq!(
            parse_parameter_expansion("x:-\"y\"", true).unwrap(),
            ParameterExpansion {
                parameter: name("x"),
                operator: ParameterOperator::UseDefault {
                    null_mode: NullMode::UnsetOrNull,
                    word: word(vec![WordSegment::DoubleQuoted(vec![WordSegment::Literal(
                        "y".into()
                    )])]),
                },
            }
        );
    }

    #[test]
    fn pattern_operand_preserves_glob_metacharacters_for_the_evaluator() {
        assert_eq!(
            parse_parameter_expansion("path##*/", false).unwrap(),
            ParameterExpansion {
                parameter: name("path"),
                operator: ParameterOperator::RemovePrefix {
                    greedy: true,
                    pattern: lit("*/"),
                },
            }
        );
    }

    // ---- error cases --------------------------------------------------------

    #[test]
    fn empty_body_is_invalid_parameter() {
        assert_eq!(
            parse_parameter_expansion("", false),
            Err(ParamExpansionError::InvalidParameter {
                body: String::new()
            })
        );
    }

    #[test]
    fn unrecognized_operator_is_flagged_not_guessed() {
        // Bash extension (substitution operator), not POSIX baseline.
        assert!(matches!(
            parse_parameter_expansion("x/foo/bar", false),
            Err(ParamExpansionError::UnrecognizedOperator { .. })
        ));
    }

    #[test]
    fn bare_trailing_colon_is_unrecognized() {
        assert!(matches!(
            parse_parameter_expansion("x:", false),
            Err(ParamExpansionError::UnrecognizedOperator { .. })
        ));
    }
}
