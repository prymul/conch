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
    /// Forces a field boundary immediately after this segment,
    /// regardless of its own content, `split_eligible`, or any
    /// neighboring segment — used *only* for `"$@"` (always) and, when
    /// `IFS` is set to the empty string, `$@`/`$*` (see
    /// [`push_positional_params_as_independent_fields`]'s docs for the
    /// full "why" and worked examples this reproduces, all confirmed
    /// against real bash). Every other segment this module ever
    /// constructs leaves this `false` — a plain expansion or piece of
    /// literal text never forces a boundary the way one positional
    /// parameter ending and the next beginning does.
    force_field_break_after: bool,
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
                force_field_break_after: false,
            });
            segments.push(Segment {
                text: rest.to_string(),
                glob_eligible: true,
                split_eligible: false,
                force_field_break_after: false,
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
                force_field_break_after: false,
            });
            Ok(())
        }
        WordSegment::SingleQuoted(text) => {
            out.push(Segment {
                text: text.clone(),
                glob_eligible: false,
                split_eligible: false,
                force_field_break_after: false,
            });
            Ok(())
        }
        WordSegment::DoubleQuoted(inner) => {
            // A *syntactically* empty double-quoted region — `""`,
            // literally nothing between the quotes, i.e. `inner` itself
            // has zero `WordSegment`s — still counts as one quoted,
            // non-splitting empty field, not zero: POSIX confirms `""`
            // is a single empty field (confirmed against real bash:
            // `set -- "" x; echo $#` is `2`), and this crate's own
            // `WordSegment::SingleQuoted("")` arm just above already
            // gets this right by construction (it unconditionally
            // pushes a `Segment` regardless of whether `text` is
            // empty). Without this, a word that's *entirely* `""` would
            // hit the `for` loop below zero times, push nothing, and
            // silently vanish into zero fields instead of surviving as
            // one empty argument — caught (not assumed, empirically)
            // while exercising `trap '' SIG` (POSIX's ignore-a-signal
            // form) for Phase 4's own differential corpus.
            //
            // Deliberately checked on `inner` itself (before expansion),
            // *not* on whether the loop below ended up pushing anything
            // — those are different questions: `"$@"` with zero
            // positional parameters, for instance, has a *non-empty*
            // `inner` (one `Parameter(At)` segment) that correctly
            // expands to *zero* pushed segments (its own POSIX-mandated
            // "vanishes entirely" rule — see
            // `push_positional_params_as_independent_fields`'s docs),
            // and must keep doing exactly that; an `out.len()`-based
            // check would have wrongly given it a phantom empty field
            // too (caught by `quoted_at_with_zero_positional_params_produces_zero_fields`
            // regressing when an earlier version of this fix tried
            // exactly that).
            if inner.is_empty() {
                out.push(Segment {
                    text: String::new(),
                    glob_eligible: false,
                    split_eligible: false,
                    force_field_break_after: false,
                });
                return Ok(());
            }
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
        // `$@`/`${@}` and `$*`/`${*}` (the lexer decodes a *bare*
        // `${@}`/`${*}` — no operator at all before the closing `}` — to
        // this exact same `WordSegment::Parameter` variant, not
        // `ComplexParameterExpansion`; see `conch-shell-lexer`'s
        // `scan_braced_parameter`) each need special, N-field-aware
        // handling no other parameter gets — see
        // [`push_positional_params_as_independent_fields`]'s and
        // [`join_positional_params_with_ifs`]'s docs for the full
        // POSIX 2.5.2 grounding and worked examples (all confirmed
        // against real bash) this reproduces. Every other parameter
        // keeps the single-segment handling this arm always had.
        WordSegment::Parameter(Parameter::Special(SpecialParameter::At)) => {
            // `"$@"` (quoted) and unquoted `$@` are *both* "one field per
            // positional parameter", differing only in whether each
            // field independently undergoes further glob/IFS-splitting
            // afterward (`in_double_quotes` alone already controls
            // exactly that, the same way it does for every other
            // parameter) — except unquoted `$@` additionally needs
            // ordinary IFS-driven splitting *within* each field when
            // `IFS` is non-empty, per POSIX 2.5.2 ("initially producing
            // one field ... non-empty fields split further") — handled
            // by [`push_unquoted_at_or_star_fields`], not here.
            if in_double_quotes {
                push_positional_params_as_independent_fields(shell, false, out);
            } else {
                push_unquoted_at_or_star_fields(shell, out);
            }
            Ok(())
        }
        WordSegment::Parameter(Parameter::Special(SpecialParameter::Star)) => {
            if in_double_quotes {
                // `"$*"` (POSIX 2.5.2): all positional parameters joined
                // into a *single* field — unlike `"$@"`, this never
                // needs `force_field_break_after` at all.
                out.push(Segment {
                    text: join_positional_params_with_ifs(shell),
                    glob_eligible: false,
                    split_eligible: false,
                    force_field_break_after: false,
                });
            } else {
                // Confirmed against real bash: unquoted `$*` and
                // unquoted `$@` are indistinguishable, even under a
                // custom `IFS` and even with positional parameters whose
                // own text contains `IFS` characters.
                push_unquoted_at_or_star_fields(shell, out);
            }
            Ok(())
        }
        WordSegment::Parameter(param) => {
            out.push(Segment {
                text: expand_parameter(param, shell),
                glob_eligible: true,
                split_eligible: true,
                force_field_break_after: false,
            });
            Ok(())
        }
        WordSegment::CommandSubstitution(sub) => {
            out.push(Segment {
                text: run_command_substitution(&sub.body, shell)?,
                glob_eligible: true,
                split_eligible: true,
                force_field_break_after: false,
            });
            Ok(())
        }
        WordSegment::ComplexParameterExpansion(body) => {
            out.push(Segment {
                text: expand_complex_parameter_expansion(body, in_double_quotes, shell)?,
                glob_eligible: true,
                split_eligible: true,
                force_field_break_after: false,
            });
            Ok(())
        }
        WordSegment::ArithmeticExpansion(body) => {
            out.push(Segment {
                text: expand_arithmetic_expansion(body, shell)?,
                glob_eligible: true,
                split_eligible: true,
                force_field_break_after: false,
            });
            Ok(())
        }
    }
}

/// Pushes one [`Segment`] per current positional parameter
/// ([`Shell::positional_params`]), each with [`Segment::split_eligible`]
/// and [`Segment::glob_eligible`] set to `!in_double_quoted_wrapper` and
/// [`Segment::force_field_break_after`] set on every one but the last —
/// what both `"$@"` (`in_double_quoted_wrapper = true`, always) and
/// unquoted `$@`/`$*` when `IFS` is set to the empty string
/// (`in_double_quoted_wrapper = false` — see
/// [`push_unquoted_at_or_star_fields`]) need.
///
/// The `force_field_break_after` mechanism this relies on ([`split_fields`])
/// only needs to close the *current* field immediately after a segment
/// like this, unconditionally — never per-character, unlike ordinary IFS
/// splitting — which is exactly what makes an empty positional parameter
/// correctly become its own empty field here (confirmed against real
/// bash: `set -- a "" b; for x in "$@"; do ...; done` visits three
/// fields, the second empty) while an *unquoted* empty one instead
/// vanishes entirely when `IFS` is non-empty (a completely different
/// mechanism — [`push_unquoted_at_or_star_fields`]'s docs).
fn push_positional_params_as_independent_fields(
    shell: &Shell,
    glob_eligible: bool,
    out: &mut Vec<Segment>,
) {
    let count = shell.positional_params.len();
    for (i, param) in shell.positional_params.iter().enumerate() {
        out.push(Segment {
            text: param.clone(),
            glob_eligible,
            split_eligible: false,
            force_field_break_after: i + 1 < count,
        });
    }
}

/// Unquoted `$@`/`$*` (POSIX 2.5.2 — confirmed against real bash these
/// two are indistinguishable when unquoted, including under a custom
/// `IFS`; see [`expand_segment`]'s `Star` arm). Two genuinely different
/// mechanisms, chosen by whether `IFS` is set to the empty string:
///
/// - `IFS` unset, default, or any non-empty value: every positional
///   parameter is joined with a single literal separator character (the
///   first character of `IFS`, or a space if `IFS` is unset — POSIX
///   2.5.2's own wording for `*`, which this reuses since confirmed
///   behavior makes `@` identical here) into *one* new segment, fed
///   through the ordinary, unmodified [`split_fields`] IFS-splitting
///   algorithm exactly like any other unquoted expansion result. This
///   alone reproduces real bash's actual, sometimes-surprising boundary
///   behavior exactly — confirmed against real bash with `IFS=","; set
///   -- "a," "b"` (a positional parameter whose *own* text happens to
///   end in an `IFS` delimiter character): unquoted `$@` there is 3
///   fields, `a`, ``, `b` — an empty field appears at the parameter
///   boundary that *neither* parameter's own text would produce if
///   split in isolation (`"a,"` alone unquoted is just 1 field, `a` — no
///   trailing empty one; see POSIX 2.6.5's "no *trailing* empty field"
///   rule, which is exactly what would otherwise suppress it) —
///   inserting a real separator character reproduces this for free,
///   without any special-casing: splitting the combined `"a,,b"` the
///   *existing*, unmodified algorithm's own already-tested
///   embedded-empty-field behavior naturally produces exactly that
///   extra field. It also reproduces the opposite-looking case
///   correctly: `set -- a "" b` (empty positional parameter, *default*
///   `IFS`) is only 2 fields, `a`, `b` — no field at all for the empty
///   one — because the inserted separator there is a plain space
///   (`IFS`'s whitespace class), and whitespace-class IFS characters
///   already collapse/never produce an empty field on their own
///   ([`split_fields`]'s existing, unmodified `is_ifs_ws` handling).
/// - `IFS` set to the empty string: splitting is disabled entirely (POSIX
///   2.6.5), so no separator is inserted and no further per-character
///   splitting happens — but `$@`/`$*` must still preserve one field per
///   positional parameter rather than collapsing to a single field the
///   way ordinary `IFS=''` input does (confirmed against real bash:
///   `IFS=""; set -- "a," "b"; for x in $@; do ...; done` is still 2
///   fields, `a,` and `b`, unmodified and un-joined) — reuses
///   [`push_positional_params_as_independent_fields`], glob-eligible
///   since this is the unquoted case.
fn push_unquoted_at_or_star_fields(shell: &Shell, out: &mut Vec<Segment>) {
    match shell.get_var("IFS") {
        Some("") => {
            push_positional_params_as_independent_fields(shell, true, out);
        }
        ifs => {
            let sep = ifs.and_then(|s| s.chars().next()).unwrap_or(' ');
            out.push(Segment {
                text: shell.positional_params.join(&sep.to_string()),
                glob_eligible: true,
                split_eligible: true,
                force_field_break_after: false,
            });
        }
    }
}

/// `"$*"` (POSIX 2.5.2): every positional parameter joined into one
/// string by the first character of `IFS` — a space if `IFS` is unset, or
/// no separator at all (not even a null one) if `IFS` is set but empty.
/// Confirmed against real bash for all three cases: `IFS` unset →
/// space-joined; `IFS=","` → comma-joined; `IFS=""` → concatenated with
/// no separator whatsoever.
fn join_positional_params_with_ifs(shell: &Shell) -> String {
    let ifs = shell.get_var("IFS");
    let sep = match ifs {
        None => " ".to_string(),
        Some(s) => s.chars().next().map(String::from).unwrap_or_default(),
    };
    shell.positional_params.join(&sep)
}

/// The single-string "current value" of `param` — used directly for
/// every ordinary parameter, and as the fallback/`ParameterOperator::Length`
/// operand for `@`/`*` too (their *field-splicing* behavior, which this
/// can't express in one `String`, is handled entirely separately — see
/// [`expand_segment`]'s dedicated `At`/`Star` arms, used for a plain
/// `$@`/`"$@"`/`$*`/`"$*"`/`${@}`/`${*}` word segment; this function is
/// only ever reached for `@`/`*` from a *different* parameter-expansion
/// operator applied to one of them, e.g. `${@:-word}` or `${#@}`'s own
/// `current` — a narrower, less-tested corner this follow-up doesn't
/// chase exhaustively). Joining with [`join_positional_params_with_ifs`]
/// here (the same as `"$*"`'s own value) is a reasonable stand-in for
/// that corner, not a claim that every operator's interaction with
/// `@`/`*` is fully bash-accurate this way.
fn expand_parameter(param: &Parameter, shell: &Shell) -> String {
    match param {
        Parameter::Name(name) => shell.get_var(name).unwrap_or("").to_string(),
        Parameter::Positional(n) => positional_param(shell, *n).unwrap_or("").to_string(),
        Parameter::Special(SpecialParameter::Question) => shell.last_status.to_string(),
        Parameter::Special(SpecialParameter::Zero) => shell.arg0.clone(),
        Parameter::Special(SpecialParameter::Hash) => shell.positional_params.len().to_string(),
        Parameter::Special(SpecialParameter::At | SpecialParameter::Star) => {
            join_positional_params_with_ifs(shell)
        }
        Parameter::Special(SpecialParameter::Dash) => String::new(),
        // `$$` -- see `Shell::pid`'s own docs for why this is the
        // top-level shell's PID even inside a subshell, not
        // `nix::unistd::getpid()` called fresh here (which would give
        // whichever real process happens to be running this expansion).
        Parameter::Special(SpecialParameter::Dollar) => shell.pid.to_string(),
        // `$!` -- the process-group-leader PID of the most recently
        // `&`-backgrounded job (`Shell::last_background_pid`, set by
        // `conch-shell-core::exec::spawn_background_job`), unset (empty,
        // same as any other unset parameter) until the first `&` this
        // session — Phase 4 job control; previously always empty here
        // since backgrounding didn't exist yet.
        Parameter::Special(SpecialParameter::Bang) => shell
            .last_background_pid
            .map(|pid| pid.to_string())
            .unwrap_or_default(),
    }
}

/// `$1`, `$2`, ... (`n` is 1-based, matching POSIX/the lexer's own
/// [`Parameter::Positional`] numbering) — `None` if `n` is out of range
/// (unset), same as an ordinary unset named variable.
fn positional_param(shell: &Shell, n: u32) -> Option<&str> {
    let index = usize::try_from(n).ok()?.checked_sub(1)?;
    shell.positional_params.get(index).map(String::as_str)
}

/// Whether `parameter` currently has a value at all — distinct from
/// whether that value is the empty string, which is what POSIX 2.6.2's
/// non-colon operator forms (`${x-y}` etc., as opposed to `${x:-y}`)
/// test for.
fn is_parameter_set(parameter: &Parameter, shell: &Shell) -> bool {
    match parameter {
        Parameter::Name(name) => shell.get_var(name).is_some(),
        Parameter::Positional(n) => positional_param(shell, *n).is_some(),
        // `@`/`*` specifically count as *unset* when there are zero
        // positional parameters, for both the colon and non-colon
        // operator forms alike — confirmed against real bash: `set --;
        // echo "${@-fallback}"` (non-colon) *and* `echo "${@:-fallback}"`
        // (colon) both print "fallback", and `set --; : "${@?msg}"`
        // errors with "@: msg" — none of which would happen if `@`/`*`
        // were simply always "set" the way every other special parameter
        // is.
        Parameter::Special(SpecialParameter::At | SpecialParameter::Star) => {
            !shell.positional_params.is_empty()
        }
        // Every *other* special parameter conch recognizes is a shell
        // built-in that always has *some* value in a real shell, even if
        // empty — none of the rest are ever genuinely "unset".
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
        // `${#@}`/`${#*}` are a POSIX-mandated special case: confirmed
        // against real bash `set -- a bb ccc; echo "${#@}" "${#*}" "$#"`
        // prints `3 3 3` — the *count* of positional parameters, not the
        // length of their joined-together text (`current` here) the way
        // `${#name}` ordinarily means.
        ParameterOperator::Length
            if matches!(
                expansion.parameter,
                Parameter::Special(SpecialParameter::At | SpecialParameter::Star)
            ) =>
        {
            Ok(shell.positional_params.len().to_string())
        }
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
        // Command substitution is a POSIX 2.12 "subshell environment"
        // too -- `$$` inside `` $(...) ``/`` `...` `` must still read as
        // the top-level shell's PID (`Shell::pid`), matching
        // `exec_subshell`/`spawn_background_job`'s own identical
        // propagation and for the same reason.
        .env("__CONCH_PID", shell.pid.to_string())
        // Same `trap ''`-ignored-signal inheritance as
        // `exec_subshell`/`spawn_background_job` -- see
        // `Shell::ignored_trap_names`'s own docs.
        .env("__CONCH_IGNORED_SIGNALS", shell.ignored_trap_names())
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
    // No early `ifs.is_empty()` shortcut here (an earlier version of this
    // function had one, unconditionally returning every segment combined
    // into at most one field) — `is_ifs_ws`/`is_ifs_delim` below are
    // already always `false` for an empty `ifs` (`"".contains(c)` is
    // always `false`), so the loop below already disables ordinary
    // per-character splitting on its own, with no special-casing needed.
    // The shortcut had to go specifically so `force_field_break_after`
    // (`"$@"`, and unquoted `$@`/`$*` when `IFS` is exactly `""` — see
    // [`push_positional_params_as_independent_fields`]'s docs) still
    // takes effect even when `IFS` is empty: confirmed against real bash
    // `IFS=""; set -- "a," "b"; for x in $@; do ...; done` is still 2
    // *separate* fields, not 1 combined one, so an ordinary
    // ifs-is-empty-therefore-everything-is-one-field rule can't be
    // correct once `$@` is in the picture.
    let is_ifs_ws = |c: char| c.is_whitespace() && ifs.contains(c);
    let is_ifs_delim = |c: char| ifs.contains(c) && !c.is_whitespace();

    let mut fields: Vec<Vec<Segment>> = Vec::new();
    let mut current: Vec<Segment> = Vec::new();
    let mut current_has_content = false;

    for segment in segments {
        if !segment.split_eligible {
            let force_break = segment.force_field_break_after;
            current.push(segment);
            current_has_content = true;
            if force_break {
                fields.push(std::mem::take(&mut current));
                current_has_content = false;
            }
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
                        force_field_break_after: false,
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
                        force_field_break_after: false,
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
                force_field_break_after: false,
            });
            current_has_content = true;
        }
        // `force_field_break_after` never fires here for a *split-
        // eligible* segment in practice — the only segments this module
        // ever constructs with it set are the non-split-eligible ones
        // above (`push_positional_params_as_independent_fields`'s N
        // independent fields) — but handled uniformly regardless, for
        // the same reason the non-split branch above does: nothing about
        // `force_field_break_after`'s *meaning* is specific to either
        // branch.
        if segment.force_field_break_after && current_has_content {
            fields.push(std::mem::take(&mut current));
            current_has_content = false;
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

    // ---- positional parameters / `$@` / `$*` (POSIX 2.5.2) -----------------

    fn set_positional(shell: &mut Shell, params: &[&str]) {
        shell.positional_params = params.iter().map(|s| s.to_string()).collect();
    }

    #[test]
    fn positional_parameter_expands_to_its_value_or_empty_if_unset() {
        let mut shell = Shell::new();
        set_positional(&mut shell, &["first", "second"]);
        assert_eq!(single("$1", &mut shell), "first");
        assert_eq!(single("$2", &mut shell), "second");
        // Out of range -- same as an ordinary unset variable.
        assert_eq!(single("$3", &mut shell), "");
    }

    #[test]
    fn hash_expands_to_positional_param_count() {
        let mut shell = Shell::new();
        assert_eq!(single("$#", &mut shell), "0");
        set_positional(&mut shell, &["a", "b", "c"]);
        assert_eq!(single("$#", &mut shell), "3");
    }

    #[test]
    fn dollar_zero_expands_to_arg0() {
        let mut shell = Shell::new();
        shell.arg0 = "myscript".to_string();
        assert_eq!(single("$0", &mut shell), "myscript");
    }

    #[test]
    fn quoted_at_produces_one_independent_field_per_positional_parameter() {
        // Confirmed against real bash: set -- one two three; for x in
        // pre"$@"post; do ...; done visits "preone", "two", "threepost"
        // -- literal text glues only to the *first*/*last* field, never
        // in between.
        let mut shell = Shell::new();
        set_positional(&mut shell, &["one", "two", "three"]);
        assert_eq!(
            fields(r#"pre"$@"post"#, &mut shell),
            vec!["preone", "two", "threepost"]
        );
    }

    #[test]
    fn quoted_at_with_zero_positional_params_produces_zero_fields() {
        let mut shell = Shell::new();
        assert_eq!(fields(r#""$@""#, &mut shell), Vec::<String>::new());
    }

    #[test]
    fn empty_double_quoted_word_is_one_empty_field_not_zero() {
        // Confirmed against real bash: `set -- "" x; echo $#` is `2`,
        // not `1` -- a syntactically empty `""` still counts as one
        // (empty) field, distinct from `"$@"` with zero positional
        // parameters (the *previous* test), which correctly vanishes
        // into zero fields precisely because it isn't syntactically
        // empty (there's a `$@` between the quotes; it just expands to
        // nothing) -- these two must not be conflated. Caught while
        // exercising `trap '' SIG` for Phase 4's own differential
        // corpus: single-quoted `''` already got this right (see
        // `single_quoted_is_never_split`'s neighbor tests), only the
        // double-quoted form had this gap.
        let mut shell = Shell::new();
        assert_eq!(fields(r#""""#, &mut shell), vec![""]);
    }

    #[test]
    fn empty_double_quoted_word_glued_to_literal_text_stays_one_field() {
        let mut shell = Shell::new();
        assert_eq!(fields(r#"a""b"#, &mut shell), vec!["ab"]);
    }

    #[test]
    fn quoted_at_with_zero_positional_params_glued_to_literal_text_keeps_the_literal_text() {
        // Confirmed against real bash: set --; for x in pre"$@"post; do
        // ...; done still visits exactly one field, "prepost" -- the
        // vanishing behavior only applies to `"$@"` standing alone.
        let mut shell = Shell::new();
        assert_eq!(fields(r#"pre"$@"post"#, &mut shell), vec!["prepost"]);
    }

    #[test]
    fn quoted_at_preserves_an_empty_positional_parameter_as_its_own_field() {
        // Confirmed against real bash: set -- a "" b; for x in "$@"; do
        // ...; done visits three fields, the second one empty.
        let mut shell = Shell::new();
        set_positional(&mut shell, &["a", "", "b"]);
        assert_eq!(fields(r#""$@""#, &mut shell), vec!["a", "", "b"]);
    }

    #[test]
    fn quoted_star_joins_positional_params_by_first_ifs_char() {
        let mut shell = Shell::new();
        set_positional(&mut shell, &["a", "b", "c"]);
        // Unset IFS -> space-joined.
        assert_eq!(fields(r#""$*""#, &mut shell), vec!["a b c"]);
        // Custom IFS -> joined by its first character.
        shell.env_vars.insert("IFS".into(), ",".into());
        assert_eq!(fields(r#""$*""#, &mut shell), vec!["a,b,c"]);
        // IFS set but empty -> no separator at all.
        shell.env_vars.insert("IFS".into(), String::new());
        assert_eq!(fields(r#""$*""#, &mut shell), vec!["abc"]);
    }

    #[test]
    fn unquoted_at_and_star_are_indistinguishable_under_custom_ifs() {
        // Confirmed against real bash: IFS=","; set -- "a b" "c d"; for x
        // in $@; do ...; done and the identical loop over $* both visit
        // exactly "a b" then "c d" -- the internal spaces aren't in IFS
        // so they don't cause further splitting, and the comma-joined
        // boundary between the two parameters doesn't merge them either.
        let mut shell = Shell::new();
        shell.env_vars.insert("IFS".into(), ",".into());
        set_positional(&mut shell, &["a b", "c d"]);
        assert_eq!(fields("$@", &mut shell), vec!["a b", "c d"]);
        assert_eq!(fields("$*", &mut shell), vec!["a b", "c d"]);
    }

    #[test]
    fn unquoted_at_under_default_ifs_splits_each_parameter_and_discards_empty_ones() {
        // Confirmed against real bash: set -- a "" b; for x in $@; do
        // ...; done visits only "a" then "b" -- the empty positional
        // parameter contributes no field at all when unquoted (contrast
        // quoted_at_preserves_an_empty_positional_parameter_as_its_own_field).
        let mut shell = Shell::new();
        set_positional(&mut shell, &["a", "", "b"]);
        assert_eq!(fields("$@", &mut shell), vec!["a", "b"]);
    }

    #[test]
    fn unquoted_at_under_default_ifs_further_splits_each_parameters_own_text() {
        // Confirmed against real bash: unset IFS; set -- "a b" "c d";
        // for x in $@; do ...; done visits four fields, "a" "b" "c" "d".
        let mut shell = Shell::new();
        set_positional(&mut shell, &["a b", "c d"]);
        assert_eq!(fields("$@", &mut shell), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn unquoted_at_custom_delimiter_ifs_can_produce_an_extra_boundary_field() {
        // Confirmed against real bash: IFS=","; set -- "a," "b"; for x
        // in $@; do ...; done visits THREE fields ("a", "", "b") even
        // though "a," split in isolation is just one field ("a", no
        // trailing empty -- POSIX 2.6.5's "no trailing empty field"
        // rule) -- the extra empty field only appears because it's now
        // *internal* to the whole $@ expansion, not the true end. A
        // second real-bash-confirmed case makes this even clearer:
        // IFS=","; set -- "a," ",b" visits FOUR fields ("a","","","b").
        let mut shell = Shell::new();
        shell.env_vars.insert("IFS".into(), ",".into());
        set_positional(&mut shell, &["a,", "b"]);
        assert_eq!(fields("$@", &mut shell), vec!["a", "", "b"]);
        set_positional(&mut shell, &["a,", ",b"]);
        assert_eq!(fields("$@", &mut shell), vec!["a", "", "", "b"]);
    }

    #[test]
    fn unquoted_at_with_ifs_empty_preserves_fields_without_further_splitting() {
        // Confirmed against real bash: IFS=""; set -- "a," "b"; for x in
        // $@; do ...; done visits exactly two fields, "a," and "b",
        // unmodified -- splitting is off, but the two parameters still
        // don't merge into one field the way ordinary IFS='' input
        // would.
        let mut shell = Shell::new();
        shell.env_vars.insert("IFS".into(), String::new());
        set_positional(&mut shell, &["a,", "b"]);
        assert_eq!(fields("$@", &mut shell), vec!["a,", "b"]);
    }

    #[test]
    fn hash_of_at_and_star_is_positional_param_count_not_joined_text_length() {
        // Confirmed against real bash: set -- a bb ccc; echo "${#@}"
        // "${#*}" "$#" prints "3 3 3", not the length of "a bb ccc".
        let mut shell = Shell::new();
        set_positional(&mut shell, &["a", "bb", "ccc"]);
        assert_eq!(single("${#@}", &mut shell), "3");
        assert_eq!(single("${#*}", &mut shell), "3");
    }

    #[test]
    fn at_and_star_count_as_unset_when_there_are_zero_positional_params() {
        // Confirmed against real bash: set --; echo "${@-fallback}"
        // "${@:-fallback}" both print "fallback" -- for *both* the
        // non-colon and colon operator forms, unlike every other special
        // parameter (which never counts as unset at all).
        let mut shell = Shell::new();
        assert_eq!(single("${@-fallback}", &mut shell), "fallback");
        assert_eq!(single("${@:-fallback}", &mut shell), "fallback");
        assert_eq!(single("${*-fallback}", &mut shell), "fallback");

        set_positional(&mut shell, &["a", "b"]);
        assert_eq!(single("${@-fallback}", &mut shell), "a b");
    }
}
