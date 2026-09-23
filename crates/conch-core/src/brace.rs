//! Bash brace expansion: `{a,b,c}` and `{start..end[..step]}` numeric or
//! single-character alpha ranges.
//!
//! This is a bash extension, not POSIX, but widely relied on. Real shells
//! apply it to a word's *unparsed text*, before any other expansion runs,
//! and it can turn one word into several — so unlike the rest of Phase 2
//! (which resolves `WordSegment`s to string values), this operates on the
//! AST directly and returns `Vec<Word>`.
//!
//! **Scope**: only recognizes a brace group fully contained within a
//! single [`WordSegment::Literal`] run — `{$a,$b}` (an expansion site
//! *inside* the braces) isn't expanded, since that would need
//! brace-awareness threaded through the lexer/parser's expansion-site
//! boundaries for a rarely-used combination. `{a,b,c}`, `pre{1..5}post`,
//! chained (`{a,b}{1,2}`), and nested (`{a,{b,c}}`) groups are all
//! handled. Zero-padding numeric ranges to a fixed width (bash pads
//! `{01..10}` to two digits) is not implemented — out of scope for now.

use conch_shell_parser::{Word, WordSegment};

/// Expands `word`'s brace groups, returning the (possibly-just-one-element)
/// list of resulting words in left-to-right order.
pub fn brace_expand(word: &Word) -> Vec<Word> {
    for (seg_idx, segment) in word.segments.iter().enumerate() {
        let WordSegment::Literal(text) = segment else {
            continue;
        };
        let Some((prefix, alternatives, suffix)) = find_brace_group(text) else {
            continue;
        };

        let results: Vec<Word> = alternatives
            .into_iter()
            .map(|alt| {
                let mut segments = word.segments[..seg_idx].to_vec();
                if !prefix.is_empty() {
                    segments.push(WordSegment::Literal(prefix.clone()));
                }
                if !alt.is_empty() {
                    segments.push(WordSegment::Literal(alt));
                }
                if !suffix.is_empty() {
                    segments.push(WordSegment::Literal(suffix.clone()));
                }
                segments.extend(word.segments[seg_idx + 1..].iter().cloned());
                Word::new(segments)
            })
            .collect();

        // Recurse: the expansion above may have revealed further brace
        // groups (nesting), or the word may have a second, chained group
        // later in its text (`{a,b}{1,2}`) that this single pass over
        // `seg_idx` didn't reach yet.
        return results.iter().flat_map(brace_expand).collect();
    }
    vec![word.clone()]
}

/// Finds the first top-level `{...}` brace group in `text` that's valid
/// bash brace-expansion syntax (a comma list with 2+ alternatives, or a
/// `start..end` range) and returns `(prefix, alternatives, suffix)`. A
/// `{...}` with no comma and no valid range syntax isn't a brace group at
/// all per bash's own rules (`{foo}` stays literal), so returns `None`
/// and the caller leaves that segment untouched.
fn find_brace_group(text: &str) -> Option<(String, Vec<String>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let open = chars.iter().position(|&c| c == '{')?;

    let mut depth = 0;
    let mut close = None;
    for (i, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;

    let body: String = chars[open + 1..close].iter().collect();
    let prefix: String = chars[..open].iter().collect();
    let suffix: String = chars[close + 1..].iter().collect();

    if let Some(range) = expand_range(&body) {
        return Some((prefix, range, suffix));
    }

    let parts = split_top_level_commas(&body);
    if parts.len() >= 2 {
        return Some((prefix, parts, suffix));
    }

    None
}

/// Splits `body` on top-level commas only — a comma nested inside its own
/// `{...}` doesn't count, so `{a,{b,c}}`'s outer body `a,{b,c}` splits
/// into `["a", "{b,c}"]`, not three pieces.
fn split_top_level_commas(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}

/// Parses `body` as a `start..end` or `start..end..step` range — numeric
/// (either direction, `step` defaults to 1 and its sign is ignored, per
/// bash) or a single-ASCII-letter range — returning the expanded sequence
/// if valid.
fn expand_range(body: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = body.split("..").collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }

    if let (Ok(start), Ok(end)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let step = match parts.get(2) {
            Some(s) => s.parse::<i64>().ok()?.unsigned_abs().max(1) as i64,
            None => 1,
        };
        let mut result = Vec::new();
        if start <= end {
            let mut v = start;
            while v <= end {
                result.push(v.to_string());
                v += step;
            }
        } else {
            let mut v = start;
            while v >= end {
                result.push(v.to_string());
                v -= step;
            }
        }
        return Some(result);
    }

    if parts.len() == 2 {
        let (a, b) = (
            parts[0].chars().collect::<Vec<_>>(),
            parts[1].chars().collect::<Vec<_>>(),
        );
        if let ([a], [b]) = (a.as_slice(), b.as_slice())
            && a.is_ascii_alphabetic()
            && b.is_ascii_alphabetic()
        {
            let (start, end) = (*a as u32, *b as u32);
            let mut result = Vec::new();
            let mut v = start;
            loop {
                result.push(char::from_u32(v).unwrap().to_string());
                if v == end {
                    break;
                }
                if start <= end {
                    v += 1;
                } else {
                    v -= 1;
                }
            }
            return Some(result);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(text: &str) -> Word {
        Word::new(vec![WordSegment::Literal(text.to_string())])
    }

    fn texts(words: &[Word]) -> Vec<String> {
        words
            .iter()
            .map(|w| {
                w.segments
                    .iter()
                    .map(|s| match s {
                        WordSegment::Literal(t) => t.clone(),
                        _ => panic!("unexpected non-literal segment in test"),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn no_braces_is_unchanged() {
        assert_eq!(texts(&brace_expand(&lit("hello"))), vec!["hello"]);
    }

    #[test]
    fn brace_without_comma_or_range_stays_literal() {
        assert_eq!(texts(&brace_expand(&lit("{foo}"))), vec!["{foo}"]);
    }

    #[test]
    fn simple_comma_list() {
        assert_eq!(texts(&brace_expand(&lit("{a,b,c}"))), vec!["a", "b", "c"]);
    }

    #[test]
    fn comma_list_with_prefix_and_suffix() {
        assert_eq!(
            texts(&brace_expand(&lit("pre{a,b}post"))),
            vec!["preapost", "prebpost"]
        );
    }

    #[test]
    fn numeric_range_ascending() {
        assert_eq!(
            texts(&brace_expand(&lit("{1..5}"))),
            vec!["1", "2", "3", "4", "5"]
        );
    }

    #[test]
    fn numeric_range_descending() {
        assert_eq!(
            texts(&brace_expand(&lit("{5..1}"))),
            vec!["5", "4", "3", "2", "1"]
        );
    }

    #[test]
    fn numeric_range_with_step() {
        assert_eq!(
            texts(&brace_expand(&lit("{0..10..2}"))),
            vec!["0", "2", "4", "6", "8", "10"]
        );
    }

    #[test]
    fn alpha_range() {
        assert_eq!(
            texts(&brace_expand(&lit("{a..e}"))),
            vec!["a", "b", "c", "d", "e"]
        );
    }

    #[test]
    fn chained_groups_produce_cross_product() {
        let mut result = texts(&brace_expand(&lit("{a,b}{1,2}")));
        result.sort();
        assert_eq!(result, vec!["a1", "a2", "b1", "b2"]);
    }

    #[test]
    fn nested_groups() {
        let mut result = texts(&brace_expand(&lit("{a,{b,c}}")));
        result.sort();
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn non_literal_segments_pass_through_untouched() {
        let word = Word::new(vec![
            WordSegment::Parameter(conch_shell_parser::Parameter::Name("X".to_string())),
            WordSegment::Literal("{a,b}".to_string()),
        ]);
        let result = brace_expand(&word);
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0].segments[0], WordSegment::Parameter(_)));
    }
}
