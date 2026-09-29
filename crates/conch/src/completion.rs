//! Tab completion's thin [`rustyline::completion::Completer`] adapter.
//! The pure candidate-generation core (`CompletionState`,
//! `command_candidates`) lives in `conch_shell_core::completion` instead
//! — see that module's own docs for why (in short: `tests/conch-difftest`
//! needs to call it in-process against a real bash oracle, which is only
//! possible for something reachable from a real library crate; this
//! binary crate has no `[lib]` target).
//!
//! Two genuinely different completion behaviors, matching bash's own
//! `COMP_CWORD == 0` (command position) vs. `> 0` (argument position)
//! distinction (GNU Bash Reference Manual, "Programmable Completion") —
//! see [`conch_shell_core::is_command_position`]:
//! - **Command position**: `conch_shell_core::command_candidates`
//!   (functions/builtins/aliases/reserved words/`$PATH` executables).
//! - **Argument position**: ordinary quote-aware filename completion —
//!   delegated wholesale to `rustyline`'s own
//!   [`rustyline::completion::FilenameCompleter`] rather than
//!   reimplemented here (see [`crate::helper::ConchHelper`]'s own docs
//!   for why that's a deliberate reuse decision, not a shortcut) — but
//!   see "Re-quoting a chosen candidate" below for why its own output
//!   still gets a post-processing pass rather than being trusted as
//!   final.
//!
//! # Re-quoting a chosen candidate for insertion
//!
//! ## Round 1 (backslash-blocklist → single-quote-wrap)
//!
//! Security review finding: backslash-escaping a fixed blocklist of
//! "break characters" (rustyline's own `escape()`, used for the
//! *unquoted*-insertion case) can miss one, and — more concretely — can't
//! even correctly *represent* some characters at all in unquoted context
//! (a literal embedded newline specifically: `\` immediately followed by
//! a real newline is POSIX line-continuation, i.e. "join with the next
//! line," not "insert a literal newline character," so backslash-escaping
//! a newline the way a blocklist-based escaper would silently corrupts a
//! name containing one). [`escaped_candidate`] instead wraps the
//! unquoted-insertion case in `'...'`, escaping an embedded `'` the
//! POSIX-standard way (close the quote, an escaped literal quote, reopen:
//! `'\''`) — this project's own established technique for "safely embed
//! an arbitrary string as literal shell text" (mirrors
//! `tests/conch-difftest`'s own `shell_single_quote` helper, used for the
//! identical problem on the oracle side).
//!
//! ## Round 2 (blocklist-shaped *wrap decision* → allow-list)
//!
//! A second security-review pass, proven end-to-end through a real pty
//! (not just code review), found that round 1's fix only moved the
//! blocklist problem up one level: the *decision* of whether a name needs
//! wrapping at all still used [`conch_shell_core::is_break_char`] — which
//! answers "where does a word *boundary* fall while typing" and has never
//! included `*`/`?`/`[`/`]`, since none of those end a word (`a*b` is one
//! word, live glob syntax and all). But this shell's own pathname-
//! expansion engine (`conch_shell_core::expand`) treats exactly those
//! four characters as live, unquoted pattern syntax. Confirmed concretely
//! (security's own pty-driven repro, with real bash run identically as a
//! control): completing `onlybracket[1].txt` unquoted, then submitting
//! the line, made conch's own glob engine reinterpret `[1]` as a
//! character class and silently operate on a *different* file
//! (`onlybracket1.txt`) than the one actually completed — a real,
//! proven, data-loss-capable bug on the single most common completion
//! path (`rm`/`mv`/`cp`/`>` argument completion), not a hypothetical one.
//!
//! [`is_always_safe_unquoted`] is the fix: an **allow-list** (only
//! characters this shell's grammar — word-splitting, quoting, *and*
//! globbing — never treats specially at all), not a blocklist with one
//! more entry added. This is the same principle round 1's fix already
//! established one level down (prefer "provably safe for everything" over
//! "safe for everything I remembered to list"), just applied to the
//! wrap-or-not decision itself rather than only the escaping mechanism.
//!
//! ## Filename candidates specifically: re-quoting rustyline's own output
//!
//! `rustyline::completion::FilenameCompleter` is delegated to wholesale
//! (see this module's own top-level docs for why), which means its own
//! internal `escape()` call — using its own `default_break_chars`, with
//! the identical "no glob-character awareness" gap `is_break_char` had —
//! produces the *same* bug for ordinary filename completion, inherited
//! from the dependency rather than introduced by this crate's own code.
//! Since forking filename completion just to fix quoting would give up
//! the whole point of reusing a battle-tested implementation,
//! [`requote_filename_candidates`] instead treats its output as a
//! first-pass draft: recovers the *unescaped* filename text via
//! [`rustyline::completion::unescape`] (safe regardless of which
//! characters rustyline chose to escape — the escape *character* is
//! always `\`, uniformly, so undoing it doesn't need to know the
//! predicate that decided which chars got one), then re-escapes it with
//! this module's own, glob-aware logic.
//!
//! One added wrinkle specific to filenames (never comes up for
//! command-position names): rustyline's own `~`-directory support (the
//! `with-dirs` feature) produces replacement text that still contains a
//! *literal*, not-yet-expanded `~` — it resolves `~` to the real home
//! directory only for its own internal `read_dir` lookup, not in the text
//! it hands back, relying on the shell itself to tilde-expand the
//! completed word later, same as if the user had typed it by hand.
//! [`wrap_filename_unquoted`] preserves that (leaves a `~/tail` needing no
//! escaping completely alone) whenever it can, but — confirmed
//! empirically against real bash, not assumed — a tilde-prefix only ever
//! expands when the *entire* prefix-to-first-slash run is unquoted and
//! uninterrupted: `~'/foo'` and even `~$x` (an adjacent *expansion*, not
//! a quote) both print `~` literally in real bash, they do not expand it.
//! So when the remainder genuinely needs protecting, wrapping only the
//! *tail* in `'...'` right after a bare `~` would silently disable
//! tilde-expansion — instead, [`wrap_filename_unquoted`] resolves the
//! leading `~` to the real, current `$HOME` itself in that one case and
//! quotes the whole resulting absolute path as a single, uniform blob
//! (correct for every character, at the cost of the inserted text no
//! longer visually showing `~` in that one narrower case). A `~user/...`
//! prefix (someone *else's* home directory) is left alone entirely —
//! this crate's own tilde expansion (`conch_shell_core::expand`) is
//! explicitly scoped to the current user's `~` only, matching that scope
//! here rather than special-casing a prefix this shell wouldn't expand
//! either way.
//!
//! ## Quote-context-awareness (unchanged from round 1)
//!
//! A chosen candidate is only ever single-quote-wrapped when insertion is
//! happening *unquoted*. Already inside an open `'...'`/`"..."`
//! (`quote: Quote::Single`/`Quote::Double`, from
//! [`crate::quoting::word_at_cursor`]), wrapping in a *fresh* `'...'`
//! would be wrong — it would either break out of the already-open quote
//! or nest incorrectly rather than extending it — so those two cases keep
//! their own, quote-appropriate escaping instead (none at all inside
//! `'...'`, matching POSIX single-quote semantics — under which `*`/`?`/
//! `[`/`]` are already inert, so nothing here needs to change for those
//! two cases at all — and `rustyline::completion::FilenameCompleter`'s
//! own identical behavior there; the narrower double-quote-special-
//! character set — `"`, `$`, `` ` ``, `\` — inside `"..."`, per POSIX
//! 2.2.3, under which glob characters are likewise already inert).

use rustyline::Context;
use rustyline::completion::{Completer, Pair, Quote, escape, unescape};

use crate::helper::ConchHelper;
use crate::quoting::word_at_cursor;

/// POSIX 2.2.3's double-quote escape set: only these four characters are
/// ever special after a backslash inside `"..."`. Reuses the *rule*
/// (mirroring `rustyline::completion::FilenameCompleter`'s own,
/// identically-scoped private `double_quotes_special_chars`, not
/// exported) rather than this crate's general
/// [`conch_shell_core::is_break_char`] set, which includes several
/// characters (`@`, `>`, `<`, `;`, `|`, `&`, `{`, `(`, ...) that have no
/// special meaning inside double quotes at all — escaping one of those
/// there would insert a spurious literal backslash into the completed
/// text instead of protecting anything. Glob characters (`*`/`?`/`[`/
/// `]`) are correctly *excluded* here too: POSIX 2.6.6 pathname expansion
/// never applies to quoted text, single or double alike, so none of them
/// need escaping inside `"..."` either — the bug this module's own docs
/// describe is specific to the *unquoted* insertion case.
fn is_double_quote_special(c: char) -> bool {
    matches!(c, '"' | '$' | '\\' | '`')
}

/// Whether `c` is safe to leave completely unquoted when spliced into an
/// *unquoted* position on the line — an allow-list (see this module's own
/// docs for why an allow-list, not "the old blocklist plus `*?[]`").
/// Deliberately narrow: alphanumerics plus a handful of characters no
/// POSIX shell ever gives special meaning to in *any* context
/// (word-splitting, quoting, or pathname expansion) — `_`, `-`, `.`, `/`,
/// `,`, `:`. Missing a character here only ever means an unnecessary
/// (but harmless) `'...'` wrap; the failure mode of the blocklist this
/// replaced was the opposite and far worse (silently *not* wrapping
/// something that needed it).
fn is_always_safe_unquoted(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ',' | ':')
}

/// Wraps `s` in `'...'` for safe insertion into an *unquoted* position on
/// the line, escaping an embedded `'` the POSIX-standard way — see this
/// module's own docs for the full reasoning. Leaves a plain identifier
/// (the overwhelmingly common shape for a function/builtin/alias/keyword/
/// `$PATH`-executable name, and a very common one for a filename too)
/// untouched rather than wrapping it in quotes that add visual noise for
/// no safety benefit.
fn single_quote_wrap(s: &str) -> String {
    if s.is_empty() || s.chars().all(is_always_safe_unquoted) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Like [`single_quote_wrap`], but handles a leading tilde-prefix
/// (`~` followed by `/` or end-of-string — a *different* user's `~user`
/// prefix is left completely alone, see this module's own docs)
/// specially — see this module's own docs for the full "why" and the
/// empirical finding (`~'/foo'` doesn't tilde-expand in real bash) that
/// rules out the simpler "just wrap the tail" approach.
fn wrap_filename_unquoted(raw: &str, home: Option<&str>) -> String {
    if raw == "~" {
        // A bare `~` alone never needs protecting -- no break/glob
        // characters in it at all.
        return raw.to_string();
    }
    if let Some(tail) = raw.strip_prefix('~') {
        // `tail` starting with `/` is a genuine tilde-prefix (`~/...`);
        // anything else (`~user/...`, or `~` followed by some other
        // character) is left alone below -- see this module's own docs
        // for why.
        if tail.starts_with('/') {
            if tail.chars().all(is_always_safe_unquoted) {
                // Nothing needs protecting -- keep the nice `~/...` form
                // so the shell's own tilde-expansion still applies.
                return raw.to_string();
            }
            if let Some(home) = home {
                return single_quote_wrap(&format!("{home}{tail}"));
            }
        }
    }
    single_quote_wrap(raw)
}

/// Re-escapes `name` for insertion back into the line at `quote`'s
/// context — see this module's own docs for the full reasoning per
/// quote kind. Used for command-position candidates (function/builtin/
/// alias/reserved-word/`$PATH`-executable names) — see
/// [`requote_filename_candidate`] for the filename-completion sibling,
/// which additionally has to recover rustyline's own already-escaped
/// text first and carve out a leading tilde-prefix.
fn escaped_candidate(name: String, quote: Quote) -> Pair {
    let replacement = match quote {
        Quote::Single => name.clone(),
        Quote::Double => escape(name.clone(), Some('\\'), is_double_quote_special, quote),
        Quote::None => single_quote_wrap(&name),
    };
    Pair {
        display: name,
        replacement,
    }
}

/// Re-derives `pair.replacement` (as returned by
/// `rustyline::completion::FilenameCompleter`) using this module's own
/// glob-aware quoting instead of trusting rustyline's own output as
/// final — see this module's own docs for the full "why".
fn requote_filename_candidate(pair: Pair, quote: Quote) -> Pair {
    // Rustyline's own escaping always uses `\` as the escape character
    // (for both the unquoted and double-quoted cases; single-quoted
    // insertion isn't escaped by it at all, matching POSIX) -- undoing it
    // doesn't need to know *which* characters it chose to escape, since
    // `unescape` just removes each `\` and keeps the following character,
    // unconditionally.
    let raw = if quote == Quote::Single {
        pair.replacement
    } else {
        unescape(&pair.replacement, Some('\\')).into_owned()
    };
    let replacement = match quote {
        Quote::Single => raw,
        Quote::Double => escape(raw, Some('\\'), is_double_quote_special, quote),
        Quote::None => wrap_filename_unquoted(&raw, std::env::var("HOME").ok().as_deref()),
    };
    Pair {
        display: pair.display,
        replacement,
    }
}

impl Completer for ConchHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let word = word_at_cursor(line, pos);
        if conch_shell_core::is_command_position(line, word.start) {
            let state = self.completion_state.borrow();
            let candidates = conch_shell_core::command_candidates(&state, &word.text)
                .into_iter()
                .map(|name| escaped_candidate(name, word.quote))
                .collect();
            Ok((word.start, candidates))
        } else {
            let (start, candidates) = self.filename_completer.complete_path(line, pos)?;
            let candidates = candidates
                .into_iter()
                .map(|pair| requote_filename_candidate(pair, word.quote))
                .collect();
            Ok((start, candidates))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_identifier_is_inserted_unquoted() {
        let pair = escaped_candidate("echo".to_string(), Quote::None);
        assert_eq!(pair.replacement, "echo");
    }

    #[test]
    fn unquoted_insertion_of_a_name_with_a_space_is_single_quote_wrapped() {
        let pair = escaped_candidate("my file".to_string(), Quote::None);
        assert_eq!(pair.replacement, "'my file'");
    }

    #[test]
    fn unquoted_insertion_of_an_embedded_newline_is_preserved_correctly() {
        // A backslash-escaped newline would be POSIX line-continuation
        // (silently dropped/joined), not a literal newline -- single-quote
        // wrapping is the only unquoted-context-safe way to represent
        // this at all.
        let pair = escaped_candidate("a\nb".to_string(), Quote::None);
        assert_eq!(pair.replacement, "'a\nb'");
    }

    #[test]
    fn unquoted_insertion_escapes_an_embedded_single_quote() {
        let pair = escaped_candidate("it's".to_string(), Quote::None);
        assert_eq!(pair.replacement, r"'it'\''s'");
    }

    #[test]
    fn unquoted_insertion_of_a_glob_bracket_is_wrapped() {
        // The proven, high-severity bug: `[1]` left bare is live pattern
        // syntax to this shell's own glob engine and can silently match a
        // *different* file once the line is submitted.
        let pair = escaped_candidate("onlybracket[1].txt".to_string(), Quote::None);
        assert_eq!(pair.replacement, "'onlybracket[1].txt'");
    }

    #[test]
    fn unquoted_insertion_of_a_glob_star_or_question_mark_is_wrapped() {
        let pair = escaped_candidate("a*b".to_string(), Quote::None);
        assert_eq!(pair.replacement, "'a*b'");
        let pair = escaped_candidate("a?b".to_string(), Quote::None);
        assert_eq!(pair.replacement, "'a?b'");
    }

    #[test]
    fn already_single_quoted_context_inserts_raw_text() {
        let pair = escaped_candidate("a b".to_string(), Quote::Single);
        assert_eq!(pair.replacement, "a b");
    }

    #[test]
    fn already_double_quoted_context_only_escapes_double_quote_specials() {
        // '@' is a break character in this crate's general unquoted set
        // but has no special meaning inside double quotes -- must not
        // get a spurious backslash.
        let pair = escaped_candidate("user@host".to_string(), Quote::Double);
        assert_eq!(pair.replacement, "user@host");
    }

    #[test]
    fn already_double_quoted_context_does_not_escape_glob_characters() {
        // Pathname expansion never applies to quoted text at all (POSIX
        // 2.6.6) -- `[1]` is just as inert inside "..." as it is inside
        // '...', so no escaping is needed (or correct) here either.
        let pair = escaped_candidate("onlybracket[1].txt".to_string(), Quote::Double);
        assert_eq!(pair.replacement, "onlybracket[1].txt");
    }

    #[test]
    fn already_double_quoted_context_escapes_a_literal_double_quote() {
        let pair = escaped_candidate("a\"b".to_string(), Quote::Double);
        assert_eq!(pair.replacement, "a\\\"b");
    }

    #[test]
    fn requote_filename_recovers_and_rewraps_a_glob_bracket() {
        // Simulates what `FilenameCompleter` itself would have returned
        // for this name unquoted (its own break-char escaping has no
        // reason to touch `[`/`]` either) -- confirms the post-processing
        // pass actually fixes it rather than passing it through.
        let pair = Pair {
            display: "onlybracket[1].txt".to_string(),
            replacement: "onlybracket[1].txt".to_string(),
        };
        let fixed = requote_filename_candidate(pair, Quote::None);
        assert_eq!(fixed.replacement, "'onlybracket[1].txt'");
    }

    #[test]
    fn requote_filename_recovers_rustyline_backslash_escaping_first() {
        // If rustyline *did* backslash-escape something (e.g. a space),
        // the post-processing pass must undo that before re-deciding,
        // not double-escape on top of it.
        let pair = Pair {
            display: "my file".to_string(),
            replacement: r"my\ file".to_string(),
        };
        let fixed = requote_filename_candidate(pair, Quote::None);
        assert_eq!(fixed.replacement, "'my file'");
    }

    #[test]
    fn requote_filename_preserves_a_leading_tilde_prefix_unquoted() {
        // `cd ~/<TAB>`-style completion must keep the leading `~`
        // unquoted so the shell's own tilde expansion still fires -- this
        // needs no escaping at all, so it's unaffected by whatever `$HOME`
        // actually is in the test environment.
        let pair = Pair {
            display: "docs".to_string(),
            replacement: "~/docs".to_string(),
        };
        let fixed = requote_filename_candidate(pair, Quote::None);
        assert_eq!(fixed.replacement, "~/docs");
    }

    #[test]
    fn wrap_filename_unquoted_keeps_tilde_prefix_when_nothing_needs_escaping() {
        assert_eq!(
            wrap_filename_unquoted("~/docs", Some("/home/conch")),
            "~/docs"
        );
    }

    #[test]
    fn wrap_filename_unquoted_resolves_tilde_to_real_home_when_remainder_needs_wrapping() {
        // Confirmed empirically against real bash: `~'/foo'` (a bare `~`
        // immediately followed by a *quoted* continuation) does not
        // tilde-expand at all -- it prints literally. So when the
        // remainder needs protecting, the only safe option is to resolve
        // `~` to the real `$HOME` first and quote the whole resulting
        // absolute path as one blob, rather than trying to quote just the
        // tail next to a bare `~`.
        assert_eq!(
            wrap_filename_unquoted("~/onlybracket[1].txt", Some("/home/conch")),
            "'/home/conch/onlybracket[1].txt'"
        );
    }

    #[test]
    fn wrap_filename_unquoted_falls_back_to_quoting_the_tilde_itself_without_home() {
        // No `$HOME` to resolve against at all (unlikely, but not
        // impossible) -- degrade to quoting everything, including the
        // `~`, rather than leaving the unsafe remainder bare. This does
        // mean tilde-expansion is lost in this one corner case, which is
        // an acceptable tradeoff against the alternative (a live glob
        // metacharacter left unquoted).
        assert_eq!(
            wrap_filename_unquoted("~/onlybracket[1].txt", None),
            "'~/onlybracket[1].txt'"
        );
    }

    #[test]
    fn wrap_filename_unquoted_treats_a_different_users_tilde_prefix_as_an_ordinary_string() {
        // `~user/...` is out of scope for this crate's own tilde
        // expansion (`conch_shell_core::expand`) -- treated as an
        // ordinary string by [`single_quote_wrap`]'s own general
        // allow-list, which doesn't special-case a bare `~` (it has no
        // meaning outside a genuine tilde-prefix position) -- so this
        // always ends up wrapped. Harmless either way: conch's own
        // expansion engine never expands `~user` regardless of quoting,
        // so there's no behavioral difference, just a minor cosmetic one.
        assert_eq!(wrap_filename_unquoted("~alice/docs", None), "'~alice/docs'");
        assert_eq!(
            wrap_filename_unquoted("~alice/only[1].txt", None),
            "'~alice/only[1].txt'"
        );
    }

    #[test]
    fn requote_filename_leaves_already_single_quoted_insertion_untouched() {
        let pair = Pair {
            display: "onlybracket[1].txt".to_string(),
            replacement: "onlybracket[1].txt".to_string(),
        };
        let fixed = requote_filename_candidate(pair, Quote::Single);
        assert_eq!(fixed.replacement, "onlybracket[1].txt");
    }
}
