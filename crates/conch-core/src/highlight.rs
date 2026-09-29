//! Syntax highlighting's pure classification core — [`classify`],
//! [`Span`], [`SpanKind`]. `conch`'s binary crate holds the thin
//! [`rustyline::highlight::Highlighter`] adapter that ANSI-wraps this
//! output (see that crate's own `highlight.rs`); this lives here purely
//! so it's reachable the same way every other pure Phase 6 core is (see
//! this crate's `prompt`/`completion` module docs for the fuller "why"),
//! even though — unlike those two — nothing about syntax highlighting has
//! a live-bash-oracle shape to differentially test against (bash has no
//! built-in syntax highlighting at all; this is a plain, ordinary Rust
//! unit-test surface, which the tests below already are).
//!
//! Reuses [`crate::word_scan`]'s tolerant scanner for quote/word-boundary
//! detection (the same "cursor is mid-edit, not a complete lexable
//! string" problem [`crate::completion`] has) plus
//! `conch_shell_lexer::lex_dollar_expansion`/`lex_backquote_expansion`
//! for `$`/backquote expansion-site recognition, rather than a third
//! reimplementation of either.

use conch_shell_lexer::{WordSegment, lex_backquote_expansion, lex_dollar_expansion};

use crate::word_scan::{RESERVED_WORDS, is_break_char, is_command_position};

/// One classified byte range of a (possibly incomplete/invalid mid-edit)
/// command line — see [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: SpanKind,
}

/// What [`classify`] found at a given [`Span`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// A POSIX/bash reserved word (`if`, `for`, `done`, ...).
    Keyword,
    /// A command-position word matching a name in the caller-supplied
    /// `known_commands` list (function, builtin, or alias name).
    Command,
    /// A `'...'` region (POSIX 2.2.2), including one left open (running
    /// to the end of `line`) by an in-progress, not-yet-closed quote.
    SingleQuoted,
    /// A `"..."` region (POSIX 2.2.3), including one left open the same
    /// way.
    DoubleQuoted,
    /// A `$name`/`${...}`/`$(...)`/`$((...))`/`` `...` `` expansion site.
    Variable,
    /// A shell operator (`|`, `||`, `&`, `&&`, `;`, `;;`, `<`, `<<`,
    /// `>`, `>>`, `(`, `)`).
    Operator,
    /// A `#`-to-end-of-line comment.
    Comment,
}

/// Classifies `line` into non-overlapping, left-to-right [`Span`]s —
/// keywords, quoting, `$`/backquote expansion sites, operators, and
/// comments — tolerating an unterminated quote/expansion (highlighted as
/// open, running to the end of `line`, rather than erroring) since `line`
/// may be a still-in-progress, not-yet-valid command a user is mid-way
/// through typing.
///
/// `known_commands` (function/builtin/alias names — see
/// [`crate::completion::CompletionState`]) is consulted only for
/// command-position words (see [`crate::word_scan::is_command_position`]);
/// an argument-position word is never colored as [`SpanKind::Command`]
/// even if it happens to share a name with one (matching real bash:
/// `echo cd` doesn't highlight `cd` as a command, since it's an argument
/// there, not one being invoked).
#[must_use]
pub fn classify(line: &str, known_commands: &[String]) -> Vec<Span> {
    let mut spans = Vec::new();
    let len = line.len();
    let mut i = 0;
    let mut at_word_start = true;

    while i < len {
        let rest = &line[i..];
        let ch = rest.chars().next().expect("i < len");

        if ch.is_whitespace() {
            i += ch.len_utf8();
            at_word_start = true;
            continue;
        }

        match ch {
            '#' if at_word_start => {
                spans.push(Span {
                    start: i,
                    end: len,
                    kind: SpanKind::Comment,
                });
                i = len;
            }
            '\'' => {
                let end = rest[1..].find('\'').map_or(len, |p| i + 1 + p + 1);
                spans.push(Span {
                    start: i,
                    end,
                    kind: SpanKind::SingleQuoted,
                });
                i = end;
                at_word_start = false;
            }
            '"' => {
                let end = scan_double_quoted_end(line, i);
                spans.push(Span {
                    start: i,
                    end,
                    kind: SpanKind::DoubleQuoted,
                });
                i = end;
                at_word_start = false;
            }
            '$' => {
                let consumed = match lex_dollar_expansion(rest) {
                    Ok((WordSegment::Literal(_), consumed)) => consumed, // a lone, non-special '$'
                    Ok((_, consumed)) => {
                        spans.push(Span {
                            start: i,
                            end: i + consumed,
                            kind: SpanKind::Variable,
                        });
                        consumed
                    }
                    Err(_) => {
                        // Unterminated ${...}/$(...)/$((...)) -- still
                        // open, mid-typing; highlight it that way rather
                        // than stopping at an error.
                        spans.push(Span {
                            start: i,
                            end: len,
                            kind: SpanKind::Variable,
                        });
                        len - i
                    }
                };
                i += consumed.max(1);
                at_word_start = false;
            }
            '`' => {
                let consumed = match lex_backquote_expansion(rest) {
                    Ok((_, consumed)) => {
                        spans.push(Span {
                            start: i,
                            end: i + consumed,
                            kind: SpanKind::Variable,
                        });
                        consumed
                    }
                    Err(_) => {
                        spans.push(Span {
                            start: i,
                            end: len,
                            kind: SpanKind::Variable,
                        });
                        len - i
                    }
                };
                i += consumed.max(1);
                at_word_start = false;
            }
            ';' | '|' | '&' | '<' | '>' => {
                let doubled = rest.starts_with("&&")
                    || rest.starts_with("||")
                    || rest.starts_with(">>")
                    || rest.starts_with("<<")
                    || rest.starts_with(";;");
                let op_len = if doubled { 2 } else { 1 };
                spans.push(Span {
                    start: i,
                    end: i + op_len,
                    kind: SpanKind::Operator,
                });
                i += op_len;
                at_word_start = true;
            }
            '(' | ')' => {
                spans.push(Span {
                    start: i,
                    end: i + 1,
                    kind: SpanKind::Operator,
                });
                i += 1;
                at_word_start = true;
            }
            _ if is_break_char(ch) => {
                // An escape/other break character with no dedicated
                // highlighting of its own (e.g. a bare `\`) -- skip it,
                // uncolored, one character at a time.
                i += ch.len_utf8();
                at_word_start = false;
            }
            _ => {
                let word_start = i;
                let mut j = i;
                while j < len {
                    let c = line[j..].chars().next().expect("j < len");
                    if is_break_char(c) {
                        break;
                    }
                    j += c.len_utf8();
                }
                let word = &line[word_start..j];
                if RESERVED_WORDS.contains(&word) {
                    spans.push(Span {
                        start: word_start,
                        end: j,
                        kind: SpanKind::Keyword,
                    });
                } else if is_command_position(line, word_start)
                    && known_commands.iter().any(|name| name == word)
                {
                    spans.push(Span {
                        start: word_start,
                        end: j,
                        kind: SpanKind::Command,
                    });
                }
                i = j;
                at_word_start = false;
            }
        }
    }

    spans
}

/// Finds the end (exclusive byte offset, right after the closing `"`) of
/// a `"..."` region starting at `line[start..]`'s opening quote — mirrors
/// `conch`'s own `find_unclosed_quote`'s double-quote escape rules (`\`
/// escapes the next character, including another `\` or `"`), returning
/// `line.len()` for one left open.
fn scan_double_quoted_end(line: &str, start: usize) -> usize {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut j = start + 1;
    while j < len {
        match bytes[j] {
            b'\\' if j + 1 < len => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds<'a>(line: &'a str, known: &[&str]) -> Vec<(SpanKind, &'a str)> {
        let known: Vec<String> = known.iter().map(|s| (*s).to_string()).collect();
        classify(line, &known)
            .into_iter()
            .map(|s| (s.kind, &line[s.start..s.end]))
            .collect()
    }

    #[test]
    fn keyword_is_recognized_at_word_start() {
        let got = kinds("if true; then echo hi; fi", &[]);
        assert!(got.contains(&(SpanKind::Keyword, "if")));
        assert!(got.contains(&(SpanKind::Keyword, "then")));
        assert!(got.contains(&(SpanKind::Keyword, "fi")));
    }

    #[test]
    fn known_command_at_command_position_is_colored() {
        let got = kinds("greet world", &["greet"]);
        assert_eq!(got[0], (SpanKind::Command, "greet"));
        // "world" is an argument -- never colored as a command even if
        // it happened to share a name with one.
        assert!(!got.iter().any(|(_, text)| *text == "world"));
    }

    #[test]
    fn known_command_at_argument_position_is_not_colored() {
        let got = kinds("echo cd", &["cd"]);
        assert!(
            !got.iter()
                .any(|(kind, text)| *kind == SpanKind::Command && *text == "cd")
        );
    }

    #[test]
    fn single_and_double_quoted_regions_are_classified() {
        let got = kinds("echo 'a b' \"c d\"", &[]);
        assert!(got.contains(&(SpanKind::SingleQuoted, "'a b'")));
        assert!(got.contains(&(SpanKind::DoubleQuoted, "\"c d\"")));
    }

    #[test]
    fn unterminated_double_quote_is_highlighted_open_to_end_of_line() {
        let line = "echo \"still open";
        let got = kinds(line, &[]);
        assert_eq!(got, vec![(SpanKind::DoubleQuoted, "\"still open")]);
    }

    #[test]
    fn dollar_variable_and_command_substitution_are_classified() {
        let got = kinds("echo $HOME $(pwd) ${X:-y}", &[]);
        assert!(got.contains(&(SpanKind::Variable, "$HOME")));
        assert!(got.contains(&(SpanKind::Variable, "$(pwd)")));
        assert!(got.contains(&(SpanKind::Variable, "${X:-y}")));
    }

    #[test]
    fn operators_are_classified() {
        let got = kinds("a && b || c | d; e", &[]);
        assert!(got.contains(&(SpanKind::Operator, "&&")));
        assert!(got.contains(&(SpanKind::Operator, "||")));
        assert!(got.contains(&(SpanKind::Operator, "|")));
        assert!(got.contains(&(SpanKind::Operator, ";")));
    }

    #[test]
    fn comment_runs_to_end_of_line() {
        let got = kinds("echo hi # a comment", &[]);
        assert_eq!(got.last(), Some(&(SpanKind::Comment, "# a comment")));
    }

    #[test]
    fn hash_mid_word_is_not_a_comment() {
        let got = kinds("echo foo#bar", &[]);
        assert!(!got.iter().any(|(kind, _)| *kind == SpanKind::Comment));
    }

    #[test]
    fn empty_line_has_no_spans() {
        assert!(classify("", &[]).is_empty());
    }
}
