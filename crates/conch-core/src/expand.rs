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
//! to tell those apart. This module keeps each expanded piece tagged with
//! whether it came from an unquoted position (see [`Segment`]) all the way
//! through splitting, and glob-escapes the quoted pieces before pattern
//! matching rather than losing the distinction.

use std::path::Path;

use conch_shell_parser::{Parameter, SpecialParameter, Word, WordSegment};

use crate::Shell;

/// An error encountered while expanding a word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// The word contains a construct this phase's expansion engine
    /// doesn't evaluate yet.
    #[error("{construct} is not yet supported")]
    UnsupportedConstruct { construct: &'static str },
    /// Command substitution failed to run.
    #[error("command substitution failed: {0}")]
    CommandSubstitutionFailed(String),
}

/// One piece of an expanded word, tagged with whether it came from an
/// unquoted position (eligible for field splitting and pathname
/// expansion) or a quoted one (never split, and its metacharacters are
/// always literal, even inside a pattern built from a mix of both).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    text: String,
    unquoted: bool,
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
/// globbing them.
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
            segments.push(Segment {
                text: home.to_string(),
                unquoted: true,
            });
            segments.push(Segment {
                text: rest.to_string(),
                unquoted: true,
            });
            continue;
        }
        expand_segment(segment, shell, &mut segments)?;
    }
    Ok(segments)
}

fn expand_segment(
    segment: &WordSegment,
    shell: &mut Shell,
    out: &mut Vec<Segment>,
) -> Result<(), ExpandError> {
    match segment {
        WordSegment::Literal(text) => {
            out.push(Segment {
                text: text.clone(),
                unquoted: true,
            });
            Ok(())
        }
        WordSegment::SingleQuoted(text) => {
            out.push(Segment {
                text: text.clone(),
                unquoted: false,
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
                expand_segment(segment, shell, &mut nested)?;
                for mut piece in nested {
                    piece.unquoted = false;
                    out.push(piece);
                }
            }
            Ok(())
        }
        WordSegment::Parameter(param) => {
            out.push(Segment {
                text: expand_parameter(param, shell),
                unquoted: true,
            });
            Ok(())
        }
        WordSegment::CommandSubstitution(sub) => {
            out.push(Segment {
                text: run_command_substitution(&sub.body, shell)?,
                unquoted: true,
            });
            Ok(())
        }
        WordSegment::ComplexParameterExpansion(_) => Err(ExpandError::UnsupportedConstruct {
            construct: "this parameter expansion operator",
        }),
        WordSegment::ArithmeticExpansion(_) => Err(ExpandError::UnsupportedConstruct {
            construct: "arithmetic expansion ($((...)))",
        }),
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
/// Only text from `unquoted` segments is ever split on; a quoted segment
/// is never split and always attaches to whichever field its neighbors
/// place it in, even if its own text contains `IFS` characters.
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
        if !segment.unquoted {
            current.push(segment);
            current_has_content = true;
            continue;
        }

        let mut piece = String::new();
        for c in segment.text.chars() {
            if is_ifs_ws(c) {
                if !piece.is_empty() {
                    current.push(Segment {
                        text: std::mem::take(&mut piece),
                        unquoted: true,
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
                        unquoted: true,
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
                unquoted: true,
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
        .any(|s| s.unquoted && s.text.chars().any(|c| matches!(c, '*' | '?' | '[')));

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
    let mut pattern = String::new();
    for segment in &field {
        for c in segment.text.chars() {
            if segment.unquoted && matches!(c, '*' | '?' | '[') {
                pattern.push(c);
            } else if matches!(c, '*' | '?' | '[' | '\\') {
                pattern.push('\\');
                pattern.push(c);
            } else {
                pattern.push(c);
            }
        }
    }

    let mut matches = glob_match_dir(cwd, &pattern);
    if matches.is_empty() {
        // No match: POSIX default is the pattern stands for itself,
        // literally — reverse the escaping above exactly (including a
        // literal backslash, not just the three glob metacharacters).
        let literal: String = field.into_iter().map(|s| s.text).collect();
        return Ok(vec![literal]);
    }
    matches.sort();
    Ok(matches)
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

fn glob_match(pattern: &str, name: &str) -> bool {
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
}
