//! Phase 2 word expansion: tilde expansion, parameter/command/arithmetic
//! expansion, field splitting (`IFS`), and pathname expansion (globbing),
//! in POSIX 2.6's specified order.
//!
//! ## Two expansion modes
//!
//! POSIX only applies field splitting and pathname expansion to command
//! names and arguments — assignment values and redirect targets undergo
//! tilde/parameter/command/arithmetic expansion and quote removal, but
//! never split or glob (`FOO=$X` with `X="a b"` sets `FOO` to the literal
//! two-word string `a b`, not two separate things). So there are two entry
//! points: [`expand_word_single`] (assignments, redirect targets) and
//! [`expand_word_fields`] (command names, arguments).
//!
//! ## Why splitting/globbing needs per-character quote tracking
//!
//! `"$X"a*b` — the `*` is unquoted so it's a glob metacharacter, but any
//! `*` that happened to be *inside* `$X`'s value must not be, even though
//! both end up adjacent in the same field. Collapsing a word to a flat
//! `String` before splitting/globbing loses exactly the information needed
//! to tell those apart. This module keeps each expanded piece tagged (see
//! [`Segment`]) all the way through splitting, and glob-escapes the quoted
//! pieces before pattern matching rather than losing the distinction.
//!
//! ## Splitting and globbing eligibility are not the same flag
//!
//! POSIX 2.6.5 only subjects the *results of parameter, command, and
//! arithmetic expansion* to `IFS` field splitting — never literal source
//! text. That matters here because the lexer has already resolved word
//! boundaries (and backslash-escapes) for literal text by the time it
//! reaches this module: a `Literal` segment can only contain whitespace
//! that was backslash-escaped in the source (an unescaped space would have
//! ended the word at the lexer level), so re-splitting on it would be
//! wrong. Pathname expansion, on the other hand, *does* apply to literal
//! unquoted text (`echo *.txt` must glob). So each [`Segment`] carries two
//! independent eligibility flags rather than one `unquoted` bit: literal
//! text is glob-eligible but never split-eligible, while an unquoted
//! expansion result is both. `${...}` and `$((...))`'s results are
//! treated the same as a plain `$var`/`` $(...) `` here — both are POSIX
//! 2.6.5 split-eligible expansions.
//!
//! ## Why `${...}`/`$((...))` expansion needs a quoting-context flag
//!
//! Both `conch_shell_parser::parse_parameter_expansion` and this module's
//! own arithmetic-body handling need to know whether they're currently
//! expanding inside a [`WordSegment::DoubleQuoted`] region — this is
//! threaded through as `in_double_quotes` on [`expand_to_segments`]/
//! [`expand_segment`] rather than inferred, because it isn't recoverable
//! from a `${...}`/`$((...))` body string alone. See
//! `conch_shell_parser::parameter_expansion`'s module docs for the
//! (confirmed-against-real-bash) worked example of why getting this wrong
//! silently produces the wrong *text*, not an error.

use std::path::Path;

use conch_shell_parser::{
    ArithAssignOp, ArithBinaryOp, ArithExpr, ArithUnaryOp, IncrDecrOp, NullMode, Parameter,
    ParameterExpansion, ParameterOperator, SpecialParameter, Word, WordSegment,
    parse_arithmetic_body, parse_arithmetic_expr, parse_parameter_expansion,
};

use crate::Shell;

/// A conservative bound on how many times a shell variable's value can
/// recursively re-enter arithmetic evaluation (bash manual §6.5: "shell
/// variables may also be referenced by name... the shell will evaluate
/// its value as an expression and use the result") before giving up.
/// Confirmed against real bash that it has an equivalent guard rather
/// than hanging or crashing: `x=x; echo $((x))` reports "expression
/// recursion level exceeded" (the exact wording [`ExpandError::ArithmeticRecursionLimit`]
/// reuses) rather than looping forever. The exact bound doesn't need to
/// match bash's own — this only exists to turn a self-referential
/// variable into a clean error instead of a stack overflow.
const MAX_ARITH_RECURSION: u32 = 100;

/// An error encountered while expanding a word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// The word contains a construct this phase's expansion engine
    /// doesn't evaluate yet — reserved for a future bash-extension
    /// parameter-expansion operator (`${var/pat/repl}`, `${var^^}`, ...)
    /// that `conch-shell-parser` also doesn't decode yet; see
    /// [`ExpandError::ParameterExpansionSyntax`] for the case that's
    /// already reachable today.
    #[error("{construct} is not yet supported")]
    UnsupportedConstruct { construct: &'static str },
    /// Command substitution failed to run.
    #[error("command substitution failed: {0}")]
    CommandSubstitutionFailed(String),
    /// A `${...}` body wasn't a valid POSIX 2.6.2 parameter expansion —
    /// either genuinely invalid syntax, or (per
    /// `conch_shell_parser::ParamExpansionError::UnrecognizedOperator`'s
    /// own message) a bash-extension operator beyond the POSIX baseline
    /// `conch-shell-parser` decodes.
    #[error("{0}")]
    ParameterExpansionSyntax(String),
    /// `${parameter:?word}` / `${parameter?word}` triggered: the
    /// parameter was unset (colon form: unset or null). Message already
    /// includes the parameter's name, matching bash's own
    /// `name: message` style.
    #[error("{0}")]
    ParameterNullOrUnset(String),
    /// `${parameter:=word}` / `${parameter=word}` tried to assign to a
    /// positional or special parameter — POSIX explicitly disallows
    /// assigning to either this way.
    #[error("{0}: cannot assign in this way")]
    CannotAssign(String),
    /// A `$((...))` body — or a shell variable's value, recursively
    /// re-evaluated as arithmetic — failed to parse as an arithmetic
    /// expression.
    #[error("{0}")]
    ArithmeticSyntax(String),
    /// `$((x / 0))` or `$((x % 0))`. Message matches bash's own wording.
    #[error("division by 0")]
    DivisionByZero,
    /// `$((x ** -1))`. Message matches bash's own wording.
    #[error("exponent less than 0")]
    NegativeExponent,
    /// See [`MAX_ARITH_RECURSION`].
    #[error("expression recursion level exceeded")]
    ArithmeticRecursionLimit,
}

/// One piece of an expanded word, tagged with its splitting/globbing
/// eligibility (see the module docs for why these are two separate
/// flags rather than one `unquoted` bit). A quoted segment always has
/// both `false`: never split, and its metacharacters are always literal,
/// even inside a pattern built from a mix of quoted and unquoted pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    text: String,
    /// Eligible for pathname expansion (globbing) if it contains `*`,
    /// `?`, or `[` — true for unquoted literal text and unquoted
    /// expansion results alike.
    glob_eligible: bool,
    /// Eligible for `IFS` field splitting — true only for the unquoted
    /// result of parameter/command/arithmetic expansion (POSIX 2.6.5),
    /// never for literal source text.
    split_eligible: bool,
}

/// Expands `word` to a single field: tilde and parameter/command/
/// arithmetic expansion plus quote removal, with no field splitting or
/// pathname expansion. The correct mode for assignment values and
/// redirect targets.
pub fn expand_word_single(word: &Word, shell: &mut Shell) -> Result<String, ExpandError> {
    let segments = expand_to_segments(word, shell)?;
    Ok(segments.into_iter().map(|s| s.text).collect())
}

/// Expands `word` to zero or more fields: everything [`expand_word_single`]
/// does, plus `IFS` field splitting and pathname expansion (globbing) on
/// unquoted parts, per POSIX 2.6. The correct mode for a command name and
/// its arguments — note a single input word can legitimately expand to
/// zero fields (e.g. an unquoted parameter that's unset or empty) or
/// several (splitting, or a glob matching multiple files).
pub fn expand_word_fields(word: &Word, shell: &mut Shell) -> Result<Vec<String>, ExpandError> {
    let segments = expand_to_segments(word, shell)?;
    let fields = split_fields(segments, &ifs(shell));
    fields
        .into_iter()
        .map(|field| glob_field(field, &shell.cwd))
        .collect::<Result<Vec<Vec<String>>, ExpandError>>()
        .map(|matches| matches.into_iter().flatten().collect())
}

fn ifs(shell: &Shell) -> String {
    shell
        .get_var("IFS")
        .map_or_else(|| " \t\n".to_string(), str::to_string)
}

/// Tilde expansion (POSIX 2.6.1) plus parameter/command/arithmetic
/// expansion, producing quote-tagged segments but not yet splitting or
/// globbing them. Always the top-level, unquoted-context entry point —
/// see [`expand_segment`]'s `in_double_quotes` parameter for the nested
/// case.
fn expand_to_segments(word: &Word, shell: &mut Shell) -> Result<Vec<Segment>, ExpandError> {
    let mut segments = Vec::new();
    for (i, segment) in word.segments.iter().enumerate() {
        // Tilde expansion only applies to an unquoted `~` at the very
        // start of the word (POSIX 2.6.1's "tilde-prefix"); `~user` (an
        // arbitrary user's home directory, via a real password-database
        // lookup) is out of scope for now — only the current user's `~`
        // and `~/...` are handled, which covers the overwhelming common
        // case.
        if i == 0
            && let WordSegment::Literal(text) = segment
            && let Some(rest) = text.strip_prefix('~')
            && (rest.is_empty() || rest.starts_with('/'))
            && let Some(home) = shell.get_var("HOME")
        {
            // Tilde expansion isn't one of POSIX 2.6.5's three
            // split-eligible expansions, so its result is never split —
            // but it's plain unquoted text, so it stays glob-eligible
            // (matching how literal text is treated below).
            segments.push(Segment {
                text: home.to_string(),
                glob_eligible: true,
                split_eligible: false,
            });
            segments.push(Segment {
                text: rest.to_string(),
                glob_eligible: true,
                split_eligible: false,
            });
            continue;
        }
        expand_segment(segment, shell, false, &mut segments)?;
    }
    Ok(segments)
}

/// Expands one [`WordSegment`] into zero or more tagged [`Segment`]s,
/// appended to `out`.
///
/// `in_double_quotes` must be `true` iff this segment is being expanded
/// while recursing into a [`WordSegment::DoubleQuoted`]'s inner segments
/// (the [`WordSegment::DoubleQuoted`] arm below always passes `true`),
/// and `false` at the top level ([`expand_to_segments`] always passes
/// `false`). This only matters for [`WordSegment::ComplexParameterExpansion`]
/// today — see `conch_shell_parser::parameter_expansion`'s module docs
/// for why the flag changes the *text* a `${...}` operand produces, not
/// just its splitting eligibility.
fn expand_segment(
    segment: &WordSegment,
    shell: &mut Shell,
    in_double_quotes: bool,
    out: &mut Vec<Segment>,
) -> Result<(), ExpandError> {
    match segment {
        WordSegment::Literal(text) => {
            // Literal source text: the lexer has already resolved word
            // boundaries (and backslash-escapes) for it, so any
            // whitespace still present here was necessarily escaped and
            // must never be re-split — but it's still eligible for
            // pathname expansion (`echo *.txt`).
            out.push(Segment {
                text: text.clone(),
                glob_eligible: true,
                split_eligible: false,
            });
            Ok(())
        }
        WordSegment::SingleQuoted(text) => {
            out.push(Segment {
                text: text.clone(),
                glob_eligible: false,
                split_eligible: false,
            });
            Ok(())
        }
        WordSegment::DoubleQuoted(inner) => {
            for segment in inner {
                // Everything nested inside double quotes is quoted,
                // including the *result* of a parameter/command/
                // arithmetic expansion site found there (POSIX 2.2.3) —
                // only the outer Literal/SingleQuoted/DoubleQuoted
                // distinction matters at the top level; expand_segment
                // itself doesn't know it's nested, so we override here.
                let mut nested = Vec::new();
                expand_segment(segment, shell, true, &mut nested)?;
                for mut piece in nested {
                    piece.glob_eligible = false;
                    piece.split_eligible = false;
                    out.push(piece);
                }
            }
            Ok(())
        }
        WordSegment::Parameter(param) => {
            out.push(Segment {
                text: expand_parameter(param, shell),
                glob_eligible: true,
                split_eligible: true,
            });
            Ok(())
        }
        WordSegment::CommandSubstitution(sub) => {
            out.push(Segment {
                text: run_command_substitution(&sub.body, shell)?,
                glob_eligible: true,
                split_eligible: true,
            });
            Ok(())
        }
        WordSegment::ComplexParameterExpansion(body) => {
            out.push(Segment {
                text: expand_complex_parameter_expansion(body, in_double_quotes, shell)?,
                glob_eligible: true,
                split_eligible: true,
            });
            Ok(())
        }
        WordSegment::ArithmeticExpansion(body) => {
            out.push(Segment {
                text: expand_arithmetic_expansion(body, shell)?,
                glob_eligible: true,
                split_eligible: true,
            });
            Ok(())
        }
    }
}

fn expand_parameter(param: &Parameter, shell: &Shell) -> String {
    match param {
        Parameter::Name(name) => shell.get_var(name).unwrap_or("").to_string(),
        Parameter::Positional(_) => String::new(),
        Parameter::Special(special) => match special {
            SpecialParameter::Question => shell.last_status.to_string(),
            _ => String::new(),
        },
    }
}

/// Whether `parameter` currently has a value at all — distinct from
/// whether that value is the empty string, which is what POSIX 2.6.2's
/// non-colon operator forms (`${x-y}` etc., as opposed to `${x:-y}`)
/// test for.
fn is_parameter_set(parameter: &Parameter, shell: &Shell) -> bool {
    match parameter {
        Parameter::Name(name) => shell.get_var(name).is_some(),
        // No positional-parameter shell state exists yet — `expand_parameter`
        // above already always expands `Parameter::Positional` to `""` —
        // so treating every positional parameter as unset is the closest
        // honest match: a real argument-less invocation would have every
        // positional parameter genuinely unset too.
        Parameter::Positional(_) => false,
        // Every special parameter conch recognizes is a shell built-in
        // that always has *some* value in a real shell, even if empty —
        // none of them are ever genuinely "unset". `$?` is the only one
        // conch actually varies (`shell.last_status`); the rest being
        // hardcoded to `""` by `expand_parameter` is a distinct,
        // already-documented simplification from being "unset".
        Parameter::Special(_) => true,
    }
}

/// Renders `parameter` the way bash's own diagnostics do (`x`, `1`, `#`,
/// `@`, ...) — used for [`ExpandError::ParameterNullOrUnset`] and
/// [`ExpandError::CannotAssign`] messages.
fn describe_parameter(parameter: &Parameter) -> String {
    match parameter {
        Parameter::Name(name) => name.clone(),
        Parameter::Positional(n) => n.to_string(),
        Parameter::Special(special) => match special {
            SpecialParameter::At => "@".to_string(),
            SpecialParameter::Star => "*".to_string(),
            SpecialParameter::Hash => "#".to_string(),
            SpecialParameter::Question => "?".to_string(),
            SpecialParameter::Dash => "-".to_string(),
            SpecialParameter::Dollar => "$".to_string(),
            SpecialParameter::Bang => "!".to_string(),
            SpecialParameter::Zero => "0".to_string(),
        },
    }
}

/// Assigns `value` to `parameter`, persistently — same "always
/// `shell_vars`, never `env_vars`" convention `exec.rs` uses for a bare
/// `NAME=value` command. Only a plain [`Parameter::Name`] can be assigned
/// this way (POSIX 2.6.2: assigning to a positional or special parameter
/// via `${parameter:=word}` is invalid).
fn assign_parameter(
    parameter: &Parameter,
    value: &str,
    shell: &mut Shell,
) -> Result<(), ExpandError> {
    match parameter {
        Parameter::Name(name) => {
            shell.shell_vars.insert(name.clone(), value.to_string());
            Ok(())
        }
        _ => Err(ExpandError::CannotAssign(describe_parameter(parameter))),
    }
}

/// Decodes and evaluates a [`WordSegment::ComplexParameterExpansion`]'s
/// raw body, producing the final substituted text.
fn expand_complex_parameter_expansion(
    body: &str,
    in_double_quotes: bool,
    shell: &mut Shell,
) -> Result<String, ExpandError> {
    let expansion = parse_parameter_expansion(body, in_double_quotes)
        .map_err(|err| ExpandError::ParameterExpansionSyntax(err.to_string()))?;
    evaluate_parameter_expansion(&expansion, shell)
}

/// Evaluates an already-decoded [`ParameterExpansion`] against `shell`'s
/// state (POSIX 2.6.2).
fn evaluate_parameter_expansion(
    expansion: &ParameterExpansion,
    shell: &mut Shell,
) -> Result<String, ExpandError> {
    let current = expand_parameter(&expansion.parameter, shell);
    let is_set = is_parameter_set(&expansion.parameter, shell);
    // POSIX 2.6.2: the colon form triggers on unset *or* null; the
    // non-colon form triggers on unset only.
    let effectively_empty = |null_mode: NullMode| match null_mode {
        NullMode::UnsetOnly => !is_set,
        NullMode::UnsetOrNull => !is_set || current.is_empty(),
    };
    match &expansion.operator {
        ParameterOperator::Length => Ok(current.chars().count().to_string()),
        ParameterOperator::UseDefault { null_mode, word } => {
            if effectively_empty(*null_mode) {
                expand_word_single(word, shell)
            } else {
                Ok(current)
            }
        }
        ParameterOperator::AssignDefault { null_mode, word } => {
            if effectively_empty(*null_mode) {
                let value = expand_word_single(word, shell)?;
                assign_parameter(&expansion.parameter, &value, shell)?;
                Ok(value)
            } else {
                Ok(current)
            }
        }
        ParameterOperator::ErrorIfUnsetOrNull { null_mode, word } => {
            if effectively_empty(*null_mode) {
                let message = expand_word_single(word, shell)?;
                let message = if message.is_empty() {
                    // POSIX-default diagnostics, matching bash's own
                    // wording exactly (confirmed against real bash:
                    // `${x:?}` -> "parameter null or not set", `${x?}`
                    // -> "parameter not set").
                    match null_mode {
                        NullMode::UnsetOrNull => "parameter null or not set".to_string(),
                        NullMode::UnsetOnly => "parameter not set".to_string(),
                    }
                } else {
                    message
                };
                Err(ExpandError::ParameterNullOrUnset(format!(
                    "{}: {message}",
                    describe_parameter(&expansion.parameter)
                )))
            } else {
                Ok(current)
            }
        }
        ParameterOperator::UseAlternative { null_mode, word } => {
            if effectively_empty(*null_mode) {
                Ok(String::new())
            } else {
                expand_word_single(word, shell)
            }
        }
        ParameterOperator::RemovePrefix { greedy, pattern } => {
            let pattern_text = expand_word_as_pattern(pattern, shell)?;
            Ok(remove_prefix(&current, &pattern_text, *greedy))
        }
        ParameterOperator::RemoveSuffix { greedy, pattern } => {
            let pattern_text = expand_word_as_pattern(pattern, shell)?;
            Ok(remove_suffix(&current, &pattern_text, *greedy))
        }
    }
}

/// Expands `word` into a POSIX 2.13 pattern string while preserving
/// which characters came from a quoted position, so a glob
/// metacharacter that was quoted (`${v#a\*}`, `${v#a"*"}`, `case $v in
/// a"*") ...`) matches itself rather than acting as a wildcard.
/// Confirmed against real bash this distinction is observable in both
/// contexts: with `v='a*c'`, `${v#a"*"}` is `c` (the quoted `*` is
/// literal) but `${v#a*}` is `*c` (the unquoted `*` wildcards, matching
/// zero extra characters for the *shortest* match); with `x='*'`, `case
/// $x in "*") ... ;; *) ... ;; esac` takes the first (literal) arm, but
/// `x='hello'` takes the second (wildcard) one. Plain [`expand_word_single`]
/// can't preserve this — it flattens straight to a `String` and loses
/// the per-character quoting tag — so this goes through
/// [`expand_to_segments`] directly instead, exactly mirroring
/// [`glob_field`]'s own pattern-building for pathname expansion (via the
/// shared [`build_glob_pattern`]).
///
/// Used both for parameter-expansion pattern operands (`${var#pattern}`
/// and siblings, this module) and for `case` pattern matching
/// (`conch-shell-core::exec`, which also reuses [`glob_match`] for the
/// actual matching — both need the exact same quote-aware pattern
/// notation, POSIX 2.13, not two different implementations of it).
pub(crate) fn expand_word_as_pattern(
    word: &Word,
    shell: &mut Shell,
) -> Result<String, ExpandError> {
    let segments = expand_to_segments(word, shell)?;
    Ok(build_glob_pattern(&segments))
}

/// Expands a [`WordSegment::ArithmeticExpansion`]'s raw body and returns
/// its final integer value's decimal text.
fn expand_arithmetic_expansion(body: &str, shell: &mut Shell) -> Result<String, ExpandError> {
    Ok(eval_arithmetic_body(body, shell)?.to_string())
}

/// Runs both of POSIX 2.6.4's arithmetic-expansion steps: expand the
/// body's tokens (parameter/command/arithmetic expansion, quote
/// removal), then evaluate the resulting text as an arithmetic
/// expression. See `conch_shell_parser::arithmetic`'s module docs for
/// why this is a genuine two-phase parse-then-evaluate, not one pass.
fn eval_arithmetic_body(body: &str, shell: &mut Shell) -> Result<i64, ExpandError> {
    let source_word = parse_arithmetic_body(body)
        .map_err(|err| ExpandError::ArithmeticSyntax(err.to_string()))?;
    let source_text = expand_arithmetic_source(&source_word, shell)?;
    let expr = parse_arithmetic_expr(&source_text)
        .map_err(|err| ExpandError::ArithmeticSyntax(err.to_string()))?;
    eval_arith(&expr, shell, 0)
}

/// Expands an arithmetic-expansion body's segments (from
/// [`parse_arithmetic_body`]) to a single flat string — this is POSIX
/// 2.6.4's first ("expand tokens") step. Deliberately *not*
/// [`expand_word_single`]: that applies tilde expansion to a leading
/// `~`, which has no special meaning inside `$((...))` (`~` there is
/// ordinary text — e.g. the bitwise-NOT operator character in
/// `$((~5))`).
///
/// Always expands with `in_double_quotes = true`, regardless of whether
/// this `$((...))` is itself nested inside real `"..."` — matching
/// `conch_shell_parser::arithmetic`'s "the body is treated as if it were
/// in double-quotes" framing throughout, not just for backslash/quote
/// handling. Confirmed against real bash: a nested `${...}` *inside* an
/// arithmetic body never lets its own `'...'` quote anything, exactly as
/// if the whole expression were textually wrapped in `"..."` —
/// `unset x; echo $(( ${x:-'a'} ))` reports the resulting arithmetic
/// syntax error as `'a' ` (literal quote characters still attached),
/// the same failure a literal `'` produces anywhere else in an
/// arithmetic body.
fn expand_arithmetic_source(word: &Word, shell: &mut Shell) -> Result<String, ExpandError> {
    let mut segments = Vec::new();
    for segment in &word.segments {
        expand_segment(segment, shell, true, &mut segments)?;
    }
    Ok(segments.into_iter().map(|s| s.text).collect())
}

/// Walks an [`ArithExpr`] tree, applying bash's documented wrapping
/// 64-bit-integer semantics throughout (bash manual §6.5: "Evaluation is
/// done in the largest fixed-width integers available, with no check for
/// overflow" — see `conch_shell_parser::arithmetic`'s module docs for the
/// full grounding this parser's own numeric-literal decoding already
/// follows). `depth` counts how many nested "a variable's value is
/// itself re-evaluated as arithmetic" hops have happened so far; see
/// [`MAX_ARITH_RECURSION`].
fn eval_arith(expr: &ArithExpr, shell: &mut Shell, depth: u32) -> Result<i64, ExpandError> {
    match expr {
        ArithExpr::Number(n) => Ok(*n),
        ArithExpr::Variable(name) => eval_arith_variable(name, shell, depth),
        ArithExpr::Unary { op, operand } => {
            let value = eval_arith(operand, shell, depth)?;
            Ok(match op {
                ArithUnaryOp::Plus => value,
                ArithUnaryOp::Minus => value.wrapping_neg(),
                ArithUnaryOp::LogicalNot => i64::from(value == 0),
                ArithUnaryOp::BitNot => !value,
            })
        }
        ArithExpr::IncrDecr { op, target, prefix } => {
            let old = eval_arith_variable(target, shell, depth)?;
            let new = match op {
                IncrDecrOp::Increment => old.wrapping_add(1),
                IncrDecrOp::Decrement => old.wrapping_sub(1),
            };
            shell.shell_vars.insert(target.clone(), new.to_string());
            Ok(if *prefix { new } else { old })
        }
        ArithExpr::Binary { op, lhs, rhs } => eval_arith_binary(*op, lhs, rhs, shell, depth),
        ArithExpr::Conditional {
            cond,
            if_true,
            if_false,
        } => {
            if eval_arith(cond, shell, depth)? != 0 {
                eval_arith(if_true, shell, depth)
            } else {
                eval_arith(if_false, shell, depth)
            }
        }
        ArithExpr::Assign { op, target, value } => {
            let rhs = eval_arith(value, shell, depth)?;
            let new = match op {
                ArithAssignOp::Assign => rhs,
                ArithAssignOp::AddAssign => {
                    eval_arith_variable(target, shell, depth)?.wrapping_add(rhs)
                }
                ArithAssignOp::SubAssign => {
                    eval_arith_variable(target, shell, depth)?.wrapping_sub(rhs)
                }
                ArithAssignOp::MulAssign => {
                    eval_arith_variable(target, shell, depth)?.wrapping_mul(rhs)
                }
                ArithAssignOp::DivAssign => {
                    checked_div(eval_arith_variable(target, shell, depth)?, rhs)?
                }
                ArithAssignOp::RemAssign => {
                    checked_rem(eval_arith_variable(target, shell, depth)?, rhs)?
                }
                ArithAssignOp::ShiftLeftAssign => {
                    eval_arith_variable(target, shell, depth)?.wrapping_shl(shift_amount(rhs))
                }
                ArithAssignOp::ShiftRightAssign => {
                    eval_arith_variable(target, shell, depth)?.wrapping_shr(shift_amount(rhs))
                }
                ArithAssignOp::BitAndAssign => eval_arith_variable(target, shell, depth)? & rhs,
                ArithAssignOp::BitXorAssign => eval_arith_variable(target, shell, depth)? ^ rhs,
                ArithAssignOp::BitOrAssign => eval_arith_variable(target, shell, depth)? | rhs,
            };
            shell.shell_vars.insert(target.clone(), new.to_string());
            Ok(new)
        }
        ArithExpr::Comma { first, second } => {
            eval_arith(first, shell, depth)?;
            eval_arith(second, shell, depth)
        }
    }
}

/// `&&`/`||` short-circuit (confirmed against real bash: `x=1; echo
/// $((0 && (x=5))); echo $x` leaves `x` at `1` — the assignment on the
/// untaken side never runs); every other binary operator always
/// evaluates both operands.
fn eval_arith_binary(
    op: ArithBinaryOp,
    lhs: &ArithExpr,
    rhs: &ArithExpr,
    shell: &mut Shell,
    depth: u32,
) -> Result<i64, ExpandError> {
    if matches!(op, ArithBinaryOp::LogicalAnd) {
        let l = eval_arith(lhs, shell, depth)?;
        if l == 0 {
            return Ok(0);
        }
        let r = eval_arith(rhs, shell, depth)?;
        return Ok(i64::from(r != 0));
    }
    if matches!(op, ArithBinaryOp::LogicalOr) {
        let l = eval_arith(lhs, shell, depth)?;
        if l != 0 {
            return Ok(1);
        }
        let r = eval_arith(rhs, shell, depth)?;
        return Ok(i64::from(r != 0));
    }

    let l = eval_arith(lhs, shell, depth)?;
    let r = eval_arith(rhs, shell, depth)?;
    Ok(match op {
        ArithBinaryOp::Power => return arith_pow(l, r),
        ArithBinaryOp::Mul => l.wrapping_mul(r),
        ArithBinaryOp::Div => return checked_div(l, r),
        ArithBinaryOp::Rem => return checked_rem(l, r),
        ArithBinaryOp::Add => l.wrapping_add(r),
        ArithBinaryOp::Sub => l.wrapping_sub(r),
        ArithBinaryOp::ShiftLeft => l.wrapping_shl(shift_amount(r)),
        ArithBinaryOp::ShiftRight => l.wrapping_shr(shift_amount(r)),
        ArithBinaryOp::Less => i64::from(l < r),
        ArithBinaryOp::LessEq => i64::from(l <= r),
        ArithBinaryOp::Greater => i64::from(l > r),
        ArithBinaryOp::GreaterEq => i64::from(l >= r),
        ArithBinaryOp::Eq => i64::from(l == r),
        ArithBinaryOp::NotEq => i64::from(l != r),
        ArithBinaryOp::BitAnd => l & r,
        ArithBinaryOp::BitXor => l ^ r,
        ArithBinaryOp::BitOr => l | r,
        ArithBinaryOp::LogicalAnd | ArithBinaryOp::LogicalOr => {
            unreachable!("short-circuited above")
        }
    })
}

/// Converts a shift-amount operand to the `u32` `wrapping_shl`/
/// `wrapping_shr` require, via the same bit-truncating cast bash's
/// underlying (hardware) shift uses. Confirmed against real bash both
/// an over-wide and a negative shift amount get masked to the low 6
/// bits of a 64-bit shift rather than erroring: `echo $((1 << 65))` is
/// `2` (shift amount `65 & 63 == 1`), and `echo $((1 << -1))` is
/// `i64::MIN` (shift amount `(-1i64 as u32) & 63 == 63`) — both exactly
/// reproduced by `rhs as u32` here combined with `wrapping_shl`/
/// `wrapping_shr`'s own automatic masking.
fn shift_amount(rhs: i64) -> u32 {
    rhs as u32
}

fn checked_div(l: i64, r: i64) -> Result<i64, ExpandError> {
    if r == 0 {
        return Err(ExpandError::DivisionByZero);
    }
    Ok(l.wrapping_div(r))
}

fn checked_rem(l: i64, r: i64) -> Result<i64, ExpandError> {
    if r == 0 {
        return Err(ExpandError::DivisionByZero);
    }
    Ok(l.wrapping_rem(r))
}

/// `l ** r`, via repeated squaring with wrapping multiplication
/// throughout. Bash rejects a negative exponent outright (confirmed:
/// `echo $((2**-1))` -> "exponent less than 0") rather than defining it
/// as 0 or a fraction, so this does too.
fn arith_pow(base: i64, exp: i64) -> Result<i64, ExpandError> {
    if exp < 0 {
        return Err(ExpandError::NegativeExponent);
    }
    let mut result: i64 = 1;
    let mut base = base;
    let mut exp = exp as u64;
    while exp > 0 {
        if exp & 1 == 1 {
            result = result.wrapping_mul(base);
        }
        base = base.wrapping_mul(base);
        exp >>= 1;
    }
    Ok(result)
}

/// Looks up `name`, then — per the bash manual §6.5 — recursively
/// re-evaluates its value as another arithmetic expression if it isn't
/// itself already fully consumed as one (this is what makes
/// `x="1+2"; echo $((x*3))` equal `9`, distinct from the *textual*
/// splicing a `$x`/`` $(...) `` expansion site gets — see
/// `conch_shell_parser::arithmetic`'s module docs for that contrast in
/// full). An unset variable, or one holding only whitespace, evaluates
/// to `0` without error (bash: same).
fn eval_arith_variable(name: &str, shell: &mut Shell, depth: u32) -> Result<i64, ExpandError> {
    let Some(value) = shell.get_var(name).map(str::to_string) else {
        return Ok(0);
    };
    if depth >= MAX_ARITH_RECURSION {
        return Err(ExpandError::ArithmeticRecursionLimit);
    }
    let expr = parse_arithmetic_expr(&value)
        .map_err(|err| ExpandError::ArithmeticSyntax(err.to_string()))?;
    eval_arith(&expr, shell, depth + 1)
}

/// Runs `body` as a command substitution and returns its captured stdout
/// with trailing newlines trimmed (POSIX 2.6.3).
///
/// Implemented by re-invoking the `conch` binary itself as `conch -c
/// <body>`, rather than recursively calling the in-process executor: a
/// real subshell's state (variable assignments, `cd`) must not leak back
/// into the calling shell, and spawning a fresh process gets that
/// isolation for free — inheriting exactly the exported environment and
/// cwd a real subshell would see — without conch's `Shell` needing to be
/// cloneable or the executor needing a separate "isolated" execution path
/// that could drift from top-level behavior.
fn run_command_substitution(body: &str, shell: &Shell) -> Result<String, ExpandError> {
    let exe = std::env::current_exe()
        .map_err(|err| ExpandError::CommandSubstitutionFailed(err.to_string()))?;
    let output = std::process::Command::new(exe)
        .arg("-c")
        .arg(body)
        .current_dir(&shell.cwd)
        .env_clear()
        .envs(&shell.env_vars)
        .output()
        .map_err(|err| ExpandError::CommandSubstitutionFailed(err.to_string()))?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    while text.ends_with('\n') {
        text.pop();
    }
    Ok(text)
}

/// Splits `segments` into fields on `ifs` characters, per POSIX 2.6.5.
/// Only text from `split_eligible` segments is ever split on — the
/// unquoted result of a parameter/command/arithmetic expansion. Every
/// other segment (literal source text, quoted text) is never split and
/// always attaches to whichever field its neighbors place it in, even if
/// its own text contains `IFS` characters.
///
/// Simplification: POSIX distinguishes `IFS` whitespace (which collapses
/// runs and is trimmed at field edges) from other `IFS` characters (each
/// occurrence delimits a field, so `a,,b` with `IFS=,` is 3 fields, one
/// empty) — implemented here by treating ' '/'\t'/'\n' as the
/// whitespace class and everything else in `ifs` as the delimiter class,
/// matching POSIX's own default-vs-custom-IFS distinction.
fn split_fields(segments: Vec<Segment>, ifs: &str) -> Vec<Vec<Segment>> {
    if ifs.is_empty() {
        // IFS='' disables splitting entirely (POSIX 2.6.5).
        return if segments.is_empty() {
            Vec::new()
        } else {
            vec![segments]
        };
    }

    let is_ifs_ws = |c: char| c.is_whitespace() && ifs.contains(c);
    let is_ifs_delim = |c: char| ifs.contains(c) && !c.is_whitespace();

    let mut fields: Vec<Vec<Segment>> = Vec::new();
    let mut current: Vec<Segment> = Vec::new();
    let mut current_has_content = false;

    for segment in segments {
        if !segment.split_eligible {
            current.push(segment);
            current_has_content = true;
            continue;
        }

        let glob_eligible = segment.glob_eligible;
        let mut piece = String::new();
        for c in segment.text.chars() {
            if is_ifs_ws(c) {
                if !piece.is_empty() {
                    current.push(Segment {
                        text: std::mem::take(&mut piece),
                        glob_eligible,
                        split_eligible: true,
                    });
                    current_has_content = true;
                }
                if current_has_content {
                    fields.push(std::mem::take(&mut current));
                    current_has_content = false;
                }
            } else if is_ifs_delim(c) {
                if !piece.is_empty() {
                    current.push(Segment {
                        text: std::mem::take(&mut piece),
                        glob_eligible,
                        split_eligible: true,
                    });
                }
                fields.push(std::mem::take(&mut current));
                current_has_content = false;
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            current.push(Segment {
                text: piece,
                glob_eligible,
                split_eligible: true,
            });
            current_has_content = true;
        }
    }
    if current_has_content {
        fields.push(current);
    }

    fields
}

/// Applies pathname expansion (POSIX 2.13/2.6.6) to one field. Returns
/// the sorted set of matching paths if the field contains at least one
/// unquoted glob metacharacter and it matches something; otherwise
/// returns the field's literal text unchanged (POSIX default behavior —
/// no match means the pattern stands for itself, not an error and not an
/// empty result).
fn glob_field(field: Vec<Segment>, cwd: &Path) -> Result<Vec<String>, ExpandError> {
    let has_unquoted_meta = field
        .iter()
        .any(|s| s.glob_eligible && s.text.chars().any(|c| matches!(c, '*' | '?' | '[')));

    // No real wildcard anywhere in this field: skip pattern-building
    // entirely and just concatenate the raw text. Building an escaped
    // glob pattern here (and then un-escaping it back) only for this
    // branch to throw the escaping away is both pointless and exactly
    // the kind of round-trip that's easy to get subtly wrong (a literal
    // backslash in the text would need escaping-then-unescaping too, not
    // just the glob metacharacters).
    if !has_unquoted_meta {
        return Ok(vec![field.into_iter().map(|s| s.text).collect()]);
    }

    // A real wildcard is present: build a pattern where quoted
    // metacharacters (and literal backslashes) are escaped so they match
    // themselves rather than acting as wildcards.
    let pattern = build_glob_pattern(&field);

    let mut matches = glob_match_dir(cwd, &pattern);
    if matches.is_empty() {
        // No match: POSIX default is the pattern stands for itself,
        // literally — the raw (unescaped) field text, not the escaped
        // pattern.
        let literal: String = field.into_iter().map(|s| s.text).collect();
        return Ok(vec![literal]);
    }
    matches.sort();
    Ok(matches)
}

/// Builds a POSIX 2.13 pattern string from `segments`, escaping any glob
/// metacharacter (and literal backslash) that came from a
/// non-glob-eligible (i.e. quoted) segment so it matches itself rather
/// than acting as a wildcard. Shared by pathname expansion ([`glob_field`])
/// and parameter-expansion pattern operands ([`expand_word_as_pattern`]),
/// which both need the same quoted-vs-unquoted distinction for pattern
/// metacharacters.
fn build_glob_pattern(segments: &[Segment]) -> String {
    let mut pattern = String::new();
    for segment in segments {
        for c in segment.text.chars() {
            if segment.glob_eligible && matches!(c, '*' | '?' | '[') {
                pattern.push(c);
            } else if matches!(c, '*' | '?' | '[' | '\\') {
                pattern.push('\\');
                pattern.push(c);
            } else {
                pattern.push(c);
            }
        }
    }
    pattern
}

/// A small, self-contained glob matcher: supports `*`, `?`, `[...]`
/// (including `[!...]` negation), and `\x` as a literal escape for `x` —
/// POSIX 2.13's pattern-matching notation, not a general globbing crate,
/// since only single-directory matching against `cwd` is needed for
/// Phase 2 (no `**`/recursive globbing, which isn't POSIX anyway).
/// Hidden files (dotfiles) only match an explicit leading `.` in the
/// pattern, matching POSIX/bash's default (non-`dotglob`) behavior.
fn glob_match_dir(dir: &Path, pattern: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let pattern_starts_with_dot = pattern.starts_with('.');
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| pattern_starts_with_dot || !name.starts_with('.'))
        .filter(|name| glob_match(pattern, name))
        .collect()
}

/// A full, anchored match of `pattern` (POSIX 2.13 notation, already
/// glob-escaped as needed by [`expand_word_as_pattern`]) against `name` —
/// `pattern` must match the *entire* string, not just a prefix/suffix of
/// it. This is what pathname expansion needs (a directory entry either
/// matches a glob or it doesn't), and, reused directly, what `case`
/// pattern matching needs too (`conch-shell-core::exec`) — the same
/// "does this pattern match this whole string" question either way.
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = name.chars().collect();
    glob_match_at(&pat, 0, &text, 0)
}

fn glob_match_at(pat: &[char], pi: usize, text: &[char], ti: usize) -> bool {
    if pi == pat.len() {
        return ti == text.len();
    }
    match pat[pi] {
        '\\' if pi + 1 < pat.len() => {
            ti < text.len() && text[ti] == pat[pi + 1] && glob_match_at(pat, pi + 2, text, ti + 1)
        }
        '*' => (ti..=text.len()).any(|k| glob_match_at(pat, pi + 1, text, k)),
        '?' => ti < text.len() && glob_match_at(pat, pi + 1, text, ti + 1),
        '[' => match match_bracket(pat, pi, text.get(ti).copied()) {
            Some(next_pi) if ti < text.len() => glob_match_at(pat, next_pi, text, ti + 1),
            _ => false,
        },
        c => ti < text.len() && text[ti] == c && glob_match_at(pat, pi + 1, text, ti + 1),
    }
}

/// Matches a `[...]`/`[!...]` bracket expression starting at `pat[pi]`
/// (which must be `[`) against `c`. Returns the pattern index just past
/// the closing `]` if `c` is present (and matched, accounting for `!`
/// negation), `None` otherwise.
fn match_bracket(pat: &[char], pi: usize, c: Option<char>) -> Option<usize> {
    let mut i = pi + 1;
    let negate = pat.get(i) == Some(&'!');
    if negate {
        i += 1;
    }
    let start = i;
    let mut matched = false;
    while i < pat.len() && (pat[i] != ']' || i == start) {
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            if let Some(c) = c
                && pat[i] <= c
                && c <= pat[i + 2]
            {
                matched = true;
            }
            i += 3;
        } else {
            if Some(pat[i]) == c {
                matched = true;
            }
            i += 1;
        }
    }
    if i >= pat.len() {
        return None; // unterminated bracket expression
    }
    let close = i + 1; // past the ']'
    let is_match = c.is_some() && (matched != negate);
    is_match.then_some(close)
}

/// Removes the smallest (`greedy = false`, `${parameter#pattern}`) or
/// largest (`greedy = true`, `${parameter##pattern}`) prefix of `text`
/// matching `pattern` (POSIX 2.13 pattern notation, via the same
/// [`glob_match_at`] engine pathname expansion uses — but anchored to
/// the *start* of `text` and required to consume the candidate prefix
/// exactly, rather than the whole string). No match anywhere leaves
/// `text` unchanged, per POSIX.
fn remove_prefix(text: &str, pattern: &str, greedy: bool) -> String {
    let pat: Vec<char> = pattern.chars().collect();
    let chars: Vec<char> = text.chars().collect();
    if greedy {
        for k in (0..=chars.len()).rev() {
            if glob_match_at(&pat, 0, &chars[..k], 0) {
                return chars[k..].iter().collect();
            }
        }
    } else {
        for k in 0..=chars.len() {
            if glob_match_at(&pat, 0, &chars[..k], 0) {
                return chars[k..].iter().collect();
            }
        }
    }
    text.to_string()
}

/// Removes the smallest (`greedy = false`, `${parameter%pattern}`) or
/// largest (`greedy = true`, `${parameter%%pattern}`) suffix of `text`
/// matching `pattern`. Symmetric with [`remove_prefix`].
fn remove_suffix(text: &str, pattern: &str, greedy: bool) -> String {
    let pat: Vec<char> = pattern.chars().collect();
    let chars: Vec<char> = text.chars().collect();
    if greedy {
        for k in (0..=chars.len()).rev() {
            let start = chars.len() - k;
            if glob_match_at(&pat, 0, &chars[start..], 0) {
                return chars[..start].iter().collect();
            }
        }
    } else {
        for k in 0..=chars.len() {
            let start = chars.len() - k;
            if glob_match_at(&pat, 0, &chars[start..], 0) {
                return chars[..start].iter().collect();
            }
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use conch_shell_parser::{Command, parse};

    fn name_word(input: &str) -> Word {
        let list = parse(input).unwrap();
        let Command::Simple(cmd) = &list.items[0].and_or.first.commands[0] else {
            unreachable!()
        };
        cmd.name.clone().unwrap()
    }

    fn fields(input: &str, shell: &mut Shell) -> Vec<String> {
        expand_word_fields(&name_word(input), shell).unwrap()
    }

    fn single(input: &str, shell: &mut Shell) -> String {
        expand_word_single(&name_word(input), shell).unwrap()
    }

    fn single_err(input: &str, shell: &mut Shell) -> ExpandError {
        expand_word_single(&name_word(input), shell).unwrap_err()
    }

    #[test]
    fn literal_word_is_one_field() {
        let mut shell = Shell::new();
        assert_eq!(fields("hello", &mut shell), vec!["hello"]);
    }

    #[test]
    fn single_quoted_is_never_split() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "irrelevant".into());
        assert_eq!(single("'$HOME'", &mut shell), "$HOME");
    }

    #[test]
    fn unset_variable_expands_to_empty() {
        let mut shell = Shell::new();
        assert_eq!(single("$NO_SUCH_VAR", &mut shell), "");
    }

    #[test]
    fn unquoted_expansion_splits_on_default_ifs() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "a b  c".into());
        assert_eq!(fields("$X", &mut shell), vec!["a", "b", "c"]);
    }

    #[test]
    fn quoted_expansion_does_not_split() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "a b c".into());
        assert_eq!(fields("\"$X\"", &mut shell), vec!["a b c"]);
    }

    #[test]
    fn empty_unquoted_expansion_produces_zero_fields() {
        let mut shell = Shell::new();
        assert_eq!(fields("$NO_SUCH_VAR", &mut shell), Vec::<String>::new());
    }

    #[test]
    fn custom_ifs_delimiter_splits_on_empty_fields_too() {
        let mut shell = Shell::new();
        shell.env_vars.insert("IFS".into(), ",".into());
        shell.env_vars.insert("X".into(), "a,,b".into());
        assert_eq!(fields("$X", &mut shell), vec!["a", "", "b"]);
    }

    #[test]
    fn quoted_glob_metacharacter_is_literal() {
        let mut shell = Shell::new();
        assert_eq!(single("'*'", &mut shell), "*");
    }

    #[test]
    fn glob_matches_files_in_cwd() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();
        let mut shell = Shell::new();
        shell.cwd = dir.path().to_path_buf();
        let mut result = fields("*.txt", &mut shell);
        result.sort();
        assert_eq!(result, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn glob_with_no_matches_stays_literal() {
        let dir = tempfile_dir();
        let mut shell = Shell::new();
        shell.cwd = dir.path().to_path_buf();
        assert_eq!(fields("*.nonexistent", &mut shell), vec!["*.nonexistent"]);
    }

    #[test]
    fn glob_skips_dotfiles_by_default() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("visible"), "").unwrap();
        let mut shell = Shell::new();
        shell.cwd = dir.path().to_path_buf();
        assert_eq!(fields("*", &mut shell), vec!["visible"]);
    }

    #[test]
    fn tilde_expands_to_home() {
        let mut shell = Shell::new();
        shell.env_vars.insert("HOME".into(), "/home/conch".into());
        assert_eq!(single("~", &mut shell), "/home/conch");
        assert_eq!(single("~/foo", &mut shell), "/home/conch/foo");
    }

    // Command substitution itself (trimmed-stdout capture, subshell state
    // isolation) is deliberately not unit-tested here: `run_command_substitution`
    // spawns `env::current_exe()`, which resolves to this test binary
    // during `cargo test`, not the real `conch` executable — and
    // conch-core can't depend on the `conch` binary crate to get one (that
    // would be a circular dependency, since `conch` depends on
    // `conch-core`). This is exactly what `tests/conch-difftest`'s
    // differential suite is for, and it covers this against the real
    // compiled binary.

    #[test]
    fn bracket_expression_matches_char_class() {
        let dir = tempfile_dir();
        std::fs::write(dir.path().join("a1"), "").unwrap();
        std::fs::write(dir.path().join("a2"), "").unwrap();
        std::fs::write(dir.path().join("ax"), "").unwrap();
        let mut shell = Shell::new();
        shell.cwd = dir.path().to_path_buf();
        let mut result = fields("a[0-9]", &mut shell);
        result.sort();
        assert_eq!(result, vec!["a1", "a2"]);
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ---- parameter expansion operators (POSIX 2.6.2) -----------------------

    #[test]
    fn length_of_set_and_unset_parameter() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "hello".into());
        assert_eq!(single("${#X}", &mut shell), "5");
        assert_eq!(single("${#NOPE}", &mut shell), "0");
    }

    #[test]
    fn use_default_colon_form_triggers_on_null_too() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), String::new());
        assert_eq!(single("${X:-fallback}", &mut shell), "fallback");
        // Non-colon form only triggers on unset, not null -- see
        // use_default_non_colon_form_ignores_null for the full contrast.
        assert_eq!(single("${X-fallback}", &mut shell), "");
    }

    #[test]
    fn use_default_non_colon_form_ignores_null() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), String::new());
        assert_eq!(single("${X-fallback}", &mut shell), "");
        shell.env_vars.remove("X");
        assert_eq!(single("${X-fallback}", &mut shell), "fallback");
    }

    #[test]
    fn assign_default_persists_to_shell_vars() {
        let mut shell = Shell::new();
        assert_eq!(single("${X:=assigned}", &mut shell), "assigned");
        assert_eq!(shell.get_var("X"), Some("assigned"));
    }

    #[test]
    fn assign_default_to_special_parameter_errors() {
        // $? doesn't work for this: its current value ("0") is always
        // set and non-null, so `${?:=x}` never even attempts the
        // assignment (matches real bash: confirmed `${?:=x}` alone just
        // returns "0" without erroring, since the default-triggering
        // condition is never met). A positional parameter always counts
        // as unset in this simplification (no positional-parameter
        // shell state exists yet), so it reliably exercises the actual
        // assignment attempt.
        let mut shell = Shell::new();
        assert!(matches!(
            single_err("${1:=x}", &mut shell),
            ExpandError::CannotAssign(_)
        ));
    }

    #[test]
    fn error_if_unset_uses_posix_default_message() {
        let mut shell = Shell::new();
        let err = single_err("${X:?}", &mut shell);
        assert_eq!(
            err,
            ExpandError::ParameterNullOrUnset("X: parameter null or not set".into())
        );
    }

    #[test]
    fn error_if_unset_uses_custom_message() {
        let mut shell = Shell::new();
        let err = single_err("${X:?required}", &mut shell);
        assert_eq!(err, ExpandError::ParameterNullOrUnset("X: required".into()));
    }

    #[test]
    fn error_if_unset_does_not_trigger_when_set() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "hi".into());
        assert_eq!(single("${X:?required}", &mut shell), "hi");
    }

    #[test]
    fn use_alternative_only_when_set_and_non_null() {
        let mut shell = Shell::new();
        assert_eq!(single("${X:+alt}", &mut shell), "");
        shell.env_vars.insert("X".into(), "set".into());
        assert_eq!(single("${X:+alt}", &mut shell), "alt");
    }

    #[test]
    fn remove_shortest_and_longest_prefix() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "a/b/c".into());
        assert_eq!(single("${X#*/}", &mut shell), "b/c");
        assert_eq!(single("${X##*/}", &mut shell), "c");
    }

    #[test]
    fn remove_shortest_and_longest_suffix() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "a/b/c".into());
        assert_eq!(single("${X%/*}", &mut shell), "a/b");
        assert_eq!(single("${X%%/*}", &mut shell), "a");
    }

    #[test]
    fn quoted_glob_metacharacter_in_pattern_operand_is_literal() {
        // Confirmed against real bash: v='a*c'; echo "${v#a"*"}" -> c
        let mut shell = Shell::new();
        shell.env_vars.insert("V".into(), "a*c".into());
        assert_eq!(single(r#"${V#a"*"}"#, &mut shell), "c");
        assert_eq!(single("${V#a*}", &mut shell), "*c");
    }

    #[test]
    fn default_word_single_quote_unquoted_vs_double_quoted_context() {
        // Confirmed against real bash: unquoted ${x:-'a b'} really
        // single-quotes; the identical text nested in "..." does not.
        let mut shell = Shell::new();
        assert_eq!(single("${X:-'a b'}", &mut shell), "a b");
        assert_eq!(single("\"${X:-'a b'}\"", &mut shell), "'a b'");
    }

    // ---- arithmetic expansion (POSIX 2.6.4) ---------------------------------

    #[test]
    fn arithmetic_basic_precedence() {
        let mut shell = Shell::new();
        assert_eq!(single("$((1 + 2 * 3))", &mut shell), "7");
    }

    #[test]
    fn arithmetic_variable_reference_without_dollar() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "5".into());
        assert_eq!(single("$((X + 1))", &mut shell), "6");
        assert_eq!(single("$(($X + 1))", &mut shell), "6");
    }

    #[test]
    fn arithmetic_command_substitution_splices_text_not_a_single_operand() {
        // Confirmed against real bash: $(( $(echo "1+2") * 3 )) == 7,
        // i.e. the substitution injects tokens, not one opaque value.
        let mut shell = Shell::new();
        // No real command execution needed to prove the splicing model:
        // use a parameter instead of a command substitution, which
        // exercises the identical "expand text, then parse" code path
        // without needing to spawn `conch -c` from within a unit test.
        shell.env_vars.insert("X".into(), "1+2".into());
        assert_eq!(single("$(($X * 3))", &mut shell), "7");
    }

    #[test]
    fn arithmetic_bare_identifier_recursively_evaluates_its_value() {
        // Confirmed against real bash: x="1+2"; echo $((x*3)) == 9 --
        // contrast with the textual-splicing case above (== 7).
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "1+2".into());
        assert_eq!(single("$((X*3))", &mut shell), "9");
    }

    #[test]
    fn arithmetic_assignment_persists_to_shell_vars() {
        let mut shell = Shell::new();
        assert_eq!(single("$((X = 5))", &mut shell), "5");
        assert_eq!(shell.get_var("X"), Some("5"));
    }

    #[test]
    fn arithmetic_increment_decrement_side_effects() {
        // Uses shell_vars (not env_vars) for the initial value: the
        // side effect always writes to shell_vars (same convention as
        // a bare `NAME=value` command; see eval_arith's `IncrDecr` arm),
        // and Shell::get_var checks env_vars first -- pre-seeding via
        // env_vars would mask the update behind the stale env_vars
        // entry, which is a distinct, already-known Phase 1
        // simplification, not something this test is about.
        let mut shell = Shell::new();
        shell.shell_vars.insert("X".into(), "1".into());
        assert_eq!(single("$((X++))", &mut shell), "1");
        assert_eq!(shell.get_var("X"), Some("2"));
        assert_eq!(single("$((++X))", &mut shell), "3");
        assert_eq!(shell.get_var("X"), Some("3"));
    }

    #[test]
    fn arithmetic_ternary_and_comparison() {
        let mut shell = Shell::new();
        assert_eq!(single("$((3 > 2 ? 10 : 20))", &mut shell), "10");
    }

    #[test]
    fn arithmetic_logical_and_or_short_circuit() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "1".into());
        assert_eq!(single("$((0 && (X = 5)))", &mut shell), "0");
        assert_eq!(shell.get_var("X"), Some("1")); // untouched
        assert_eq!(single("$((1 || (X = 5)))", &mut shell), "1");
        assert_eq!(shell.get_var("X"), Some("1")); // still untouched
    }

    #[test]
    fn arithmetic_division_by_zero_errors() {
        let mut shell = Shell::new();
        assert_eq!(
            single_err("$((1/0))", &mut shell),
            ExpandError::DivisionByZero
        );
        assert_eq!(
            single_err("$((1%0))", &mut shell),
            ExpandError::DivisionByZero
        );
    }

    #[test]
    fn arithmetic_negative_exponent_errors() {
        let mut shell = Shell::new();
        assert_eq!(
            single_err("$((2**-1))", &mut shell),
            ExpandError::NegativeExponent
        );
    }

    #[test]
    fn arithmetic_overflow_wraps() {
        let mut shell = Shell::new();
        assert_eq!(
            single("$((9223372036854775807+1))", &mut shell),
            i64::MIN.to_string()
        );
    }

    #[test]
    fn arithmetic_self_referential_variable_errors_instead_of_hanging() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".into(), "X".into());
        assert_eq!(
            single_err("$((X))", &mut shell),
            ExpandError::ArithmeticRecursionLimit
        );
    }

    #[test]
    fn arithmetic_within_double_quotes_still_evaluates() {
        let mut shell = Shell::new();
        assert_eq!(single("\"$((1+1))\"", &mut shell), "2");
    }

    #[test]
    fn nested_nonquoting_single_quote_inside_arithmetic_parameter_expansion() {
        // Confirmed against real bash: unset x; echo $(( ${x:-'a'} ))
        // errors with the literal quote characters still attached -- a
        // nested ${...}'s own '...' never quotes inside $((...)).
        let mut shell = Shell::new();
        assert!(matches!(
            single_err("$(( ${X:-'a'} ))", &mut shell),
            ExpandError::ArithmeticSyntax(_)
        ));
    }

    #[test]
    fn arithmetic_tilde_is_not_special() {
        let mut shell = Shell::new();
        shell.env_vars.insert("HOME".into(), "/home/conch".into());
        assert_eq!(single("$((~0))", &mut shell), "-1");
    }
}
