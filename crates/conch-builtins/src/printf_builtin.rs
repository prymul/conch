//! `printf format [argument...]` (POSIX 2.9.4/utility 12.2, plus bash's
//! `-v varname` extension) — a real, separate format-string mini-language
//! from `echo`'s: conversions (`%s %d %i %o %u %x %X %c %b %%`), flags,
//! width/precision (including bash's `*`-from-argument form), and a
//! shared ANSI-C-ish backslash-escape decoder used both for the format
//! string's own literal text and for `%b`'s argument text specifically.
//!
//! # Argument recycling
//!
//! POSIX: "if there are more `argument` operands than format
//! specifications, the format string shall be reused" — confirmed
//! against real bash this reuse is gated on the format containing at
//! least one *argument-consuming* conversion at all (`printf "hi\n" a b
//! c` prints `hi` exactly once, not three times — `a`/`b`/`c` are simply
//! never consumed and silently dropped), and stops as soon as one full
//! pass leaves no operands left ([`format_has_arg_consuming_conversion`]/
//! [`run_printf`]). A pass that runs out of operands *partway through*
//! (more conversions than remaining operands in that one pass) uses an
//! empty string for `%s`/`%b` and `0` for every numeric conversion,
//! matching POSIX exactly.
//!
//! # Known gaps
//!
//! The `#` (alternate form) flag and `%a`/`%A`/`%e`/`%E`/`%f`/`%F`/`%g`/
//! `%G` (bash's floating-point conversions — conch's arithmetic is
//! integer-only throughout, matching `$((...))`'s own documented scope)
//! and `%q` (bash extension: shell-quoted output) are not implemented —
//! encountering one is a clear error rather than a silent wrong answer.

use std::io::{Read, Write};

use conch_shell_core::{Builtin, Shell};

/// See the module docs. `-v varname` (bash extension) assigns the
/// formatted result to a shell variable instead of writing it to stdout.
pub struct Printf;
impl Builtin for Printf {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let mut rest = args;
        let mut var_name = None;
        if rest.first().map(String::as_str) == Some("-v") {
            let Some(name) = rest.get(1) else {
                let _ = writeln!(stderr, "printf: -v: option requires an argument");
                return 2;
            };
            var_name = Some(name.clone());
            rest = &rest[2..];
        }

        let Some(format) = rest.first() else {
            let _ = writeln!(stderr, "printf: usage: printf format [argument...]");
            return 2;
        };
        let operands = &rest[1..];

        match run_printf(format, operands) {
            Ok((output, had_error)) => {
                match var_name {
                    Some(name) => {
                        shell.shell_vars.insert(name, output);
                    }
                    None => {
                        let _ = write!(stdout, "{output}");
                    }
                }
                i32::from(had_error)
            }
            Err(message) => {
                let _ = writeln!(stderr, "printf: {message}");
                1
            }
        }
    }
}

/// Runs `format` against `operands`, looping per the module docs' own
/// "argument recycling" section. Returns the accumulated output and
/// whether any recoverable error occurred along the way (an invalid
/// numeric argument — POSIX: `printf` still produces output, using `0`
/// for that conversion, but exits nonzero).
fn run_printf(format: &str, operands: &[String]) -> Result<(String, bool), String> {
    let has_conversion = format_has_arg_consuming_conversion(format)?;
    let mut iter = operands.iter().peekable();
    let mut output = String::new();
    let mut had_error = false;
    loop {
        let stopped = run_one_pass(format, &mut iter, &mut output, &mut had_error)?;
        if stopped || !has_conversion || iter.peek().is_none() {
            break;
        }
    }
    Ok((output, had_error))
}

/// A quick pre-scan for whether `format` contains any conversion that
/// consumes an operand (anything but a bare `%%`) — see [`run_printf`]'s
/// docs for why this, decided once structurally, is what actually gates
/// recycling rather than how many operands a given pass happened to find
/// available.
fn format_has_arg_consuming_conversion(format: &str) -> Result<bool, String> {
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            let mut scratch = String::new();
            decode_one_escape(&chars, &mut i, &mut scratch);
        } else if chars[i] == '%' {
            if chars.get(i + 1) == Some(&'%') {
                i += 2;
            } else {
                return Ok(true);
            }
        } else {
            i += 1;
        }
    }
    Ok(false)
}

/// Runs one left-to-right pass over `format`, pulling operands from
/// `iter` as conversions need them and appending formatted output to
/// `out`. Returns `true` if a `\c` escape was encountered anywhere (in
/// literal text or inside `%b`'s decoded argument) — POSIX/bash: this
/// means stop producing output immediately, mid-format, not just end
/// this one pass.
fn run_one_pass<'a>(
    format: &str,
    iter: &mut std::iter::Peekable<std::slice::Iter<'a, String>>,
    out: &mut String,
    had_error: &mut bool,
) -> Result<bool, String> {
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            if decode_one_escape(&chars, &mut i, out) {
                return Ok(true);
            }
        } else if chars[i] == '%' {
            if apply_conversion(&chars, &mut i, iter, out, had_error)? {
                return Ok(true);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    Ok(false)
}

/// Decodes one backslash escape at `chars[*i]` (`== '\\'`), advancing
/// `*i` past it and appending the decoded character to `out`. The shared
/// ANSI-C-ish set this crate's `$'...'` quoting (`conch-shell-lexer`)
/// also implements: `\\ \a \b \e \f \n \r \t \v`, `\NNN` (1-3 octal
/// digits), `\xHH` (1-2 hex digits) — plus `\c`, unique to this context,
/// which this function reports via its `bool` return (`true`) rather
/// than pushing anything for, so the caller can stop all further output
/// immediately, per POSIX/bash. An unrecognized escape keeps the
/// backslash literally and leaves the following character for the next
/// iteration to process normally (matching real bash rather than
/// silently dropping the backslash).
fn decode_one_escape(chars: &[char], i: &mut usize, out: &mut String) -> bool {
    *i += 1; // past the backslash
    let Some(&c) = chars.get(*i) else {
        out.push('\\');
        return false;
    };
    match c {
        '\\' => {
            out.push('\\');
            *i += 1;
        }
        'a' => {
            out.push('\u{7}');
            *i += 1;
        }
        'b' => {
            out.push('\u{8}');
            *i += 1;
        }
        'e' | 'E' => {
            out.push('\u{1b}');
            *i += 1;
        }
        'f' => {
            out.push('\u{c}');
            *i += 1;
        }
        'n' => {
            out.push('\n');
            *i += 1;
        }
        'r' => {
            out.push('\r');
            *i += 1;
        }
        't' => {
            out.push('\t');
            *i += 1;
        }
        'v' => {
            out.push('\u{b}');
            *i += 1;
        }
        'c' => {
            *i += 1;
            return true;
        }
        'x' => {
            *i += 1;
            let mut val: u32 = 0;
            let mut n = 0;
            while n < 2 {
                match chars.get(*i).and_then(|c| c.to_digit(16)) {
                    Some(d) => {
                        val = val * 16 + d;
                        *i += 1;
                        n += 1;
                    }
                    None => break,
                }
            }
            if n > 0 {
                out.push(char::from_u32(val).unwrap_or('\u{fffd}'));
            } else {
                out.push('x');
            }
        }
        '0'..='7' => {
            let mut val = c.to_digit(8).unwrap_or(0);
            *i += 1;
            let mut n = 1;
            while n < 3 {
                match chars.get(*i).and_then(|c| c.to_digit(8)) {
                    Some(d) => {
                        val = val * 8 + d;
                        *i += 1;
                        n += 1;
                    }
                    None => break,
                }
            }
            out.push(char::from_u32(val).unwrap_or('\u{fffd}'));
        }
        _ => {
            out.push('\\');
        }
    }
    false
}

#[derive(Default)]
struct Flags {
    left_justify: bool,
    zero_pad: bool,
    force_sign: bool,
    space_sign: bool,
}

/// Parses and applies one `%...conv` conversion at `chars[*i]` (`==
/// '%'`), advancing `*i` past the whole spec, pulling operand(s) from
/// `iter` as needed (including for a `*` width/precision, a bash
/// extension), and appending formatted output to `out`. Returns whether
/// a `\c` was hit while decoding `%b`'s argument text (see
/// [`run_one_pass`]'s own docs).
fn apply_conversion<'a>(
    chars: &[char],
    i: &mut usize,
    iter: &mut std::iter::Peekable<std::slice::Iter<'a, String>>,
    out: &mut String,
    had_error: &mut bool,
) -> Result<bool, String> {
    *i += 1; // past '%'
    if chars.get(*i) == Some(&'%') {
        *i += 1;
        out.push('%');
        return Ok(false);
    }

    let mut flags = Flags::default();
    loop {
        match chars.get(*i) {
            Some('-') => {
                flags.left_justify = true;
                *i += 1;
            }
            Some('0') => {
                flags.zero_pad = true;
                *i += 1;
            }
            Some('+') => {
                flags.force_sign = true;
                *i += 1;
            }
            Some(' ') => {
                flags.space_sign = true;
                *i += 1;
            }
            Some('#') => {
                return Err("the `#' flag is not yet supported".to_string());
            }
            _ => break,
        }
    }

    let width = parse_width_or_precision(chars, i, iter)?;
    let precision = if chars.get(*i) == Some(&'.') {
        *i += 1;
        Some(parse_width_or_precision(chars, i, iter)?.unwrap_or(0))
    } else {
        None
    };

    let Some(&conv) = chars.get(*i) else {
        return Err("missing conversion character".to_string());
    };
    *i += 1;

    let (text, stop) = match conv {
        's' => {
            let value = iter.next().map_or_else(String::new, |s| s.clone());
            let value = match precision {
                Some(p) => value.chars().take(p).collect(),
                None => value,
            };
            (value, false)
        }
        'b' => {
            let raw = iter.next().map_or_else(String::new, |s| s.clone());
            let raw_chars: Vec<char> = raw.chars().collect();
            let mut decoded = String::new();
            let mut j = 0;
            let mut stop = false;
            while j < raw_chars.len() {
                if raw_chars[j] == '\\' {
                    if decode_one_escape(&raw_chars, &mut j, &mut decoded) {
                        stop = true;
                        break;
                    }
                } else {
                    decoded.push(raw_chars[j]);
                    j += 1;
                }
            }
            (decoded, stop)
        }
        'c' => {
            let value = iter.next().map_or_else(String::new, |s| s.clone());
            (
                value.chars().next().map(String::from).unwrap_or_default(),
                false,
            )
        }
        'd' | 'i' => {
            let n = parse_printf_integer(iter.next(), had_error)?;
            let mut text = n.unsigned_abs().to_string();
            if n < 0 {
                text = format!("-{text}");
            } else if flags.force_sign {
                text = format!("+{text}");
            } else if flags.space_sign {
                text = format!(" {text}");
            }
            (text, false)
        }
        'o' => {
            let n = parse_printf_integer(iter.next(), had_error)?;
            (format!("{:o}", n as u64), false)
        }
        'u' => {
            let n = parse_printf_integer(iter.next(), had_error)?;
            (format!("{}", n as u64), false)
        }
        'x' => {
            let n = parse_printf_integer(iter.next(), had_error)?;
            (format!("{:x}", n as u64), false)
        }
        'X' => {
            let n = parse_printf_integer(iter.next(), had_error)?;
            (format!("{:X}", n as u64), false)
        }
        other => {
            return Err(format!(
                "`%{other}': unsupported (or not-yet-implemented) conversion"
            ));
        }
    };

    out.push_str(&pad(&text, width, flags.left_justify, flags.zero_pad));
    Ok(stop)
}

/// A bare numeric width/precision (`42`) or bash's `*` form (pulls the
/// next operand and parses it as the width/precision itself).
fn parse_width_or_precision<'a>(
    chars: &[char],
    i: &mut usize,
    iter: &mut std::iter::Peekable<std::slice::Iter<'a, String>>,
) -> Result<Option<usize>, String> {
    if chars.get(*i) == Some(&'*') {
        *i += 1;
        let value = iter.next().map(String::as_str).unwrap_or("0");
        return value
            .parse::<usize>()
            .map(Some)
            .map_err(|_| format!("{value}: invalid number"));
    }
    let start = *i;
    while chars.get(*i).is_some_and(|c| c.is_ascii_digit()) {
        *i += 1;
    }
    if *i == start {
        return Ok(None);
    }
    let digits: String = chars[start..*i].iter().collect();
    digits
        .parse::<usize>()
        .map(Some)
        .map_err(|_| format!("{digits}: invalid number"))
}

/// POSIX/bash `printf` numeric-argument parsing: an ordinary (optionally
/// signed) decimal integer, or bash's `'c`/`"c` extension (the numeric
/// value of the character `c`'s first byte) — e.g. `printf "%d" "'A"` is
/// `65`. A missing operand is `0` (POSIX: fewer arguments than
/// conversions). An operand that's neither is a recoverable error: still
/// contributes `0` to keep the rest of the format producing output, but
/// records that this call should exit nonzero — matching real bash's own
/// "prints a diagnostic to stderr, keeps going, exits 1" behavior (the
/// diagnostic itself isn't reproduced here, only the exit-status effect,
/// since nothing downstream depends on matching bash's exact wording).
fn parse_printf_integer(arg: Option<&String>, had_error: &mut bool) -> Result<i64, String> {
    let Some(arg) = arg else {
        return Ok(0);
    };
    if let Some(rest) = arg.strip_prefix('\'').or_else(|| arg.strip_prefix('"')) {
        return Ok(i64::from(rest.bytes().next().unwrap_or(0)));
    }
    match arg.trim().parse::<i64>() {
        Ok(n) => Ok(n),
        Err(_) => {
            *had_error = true;
            Ok(0)
        }
    }
}

/// Applies width/justification/zero-padding — shared by every conversion
/// (POSIX: width/flags apply uniformly regardless of conversion type,
/// `%s` included).
fn pad(text: &str, width: Option<usize>, left_justify: bool, zero_pad: bool) -> String {
    let Some(width) = width else {
        return text.to_string();
    };
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let fill = width - len;
    if left_justify {
        format!("{text}{}", " ".repeat(fill))
    } else if zero_pad {
        // Zero-padding goes after a leading sign character, not before it
        // (`%05d` of `-3` is `-0003`, not `000-3`).
        if let Some(rest) = text.strip_prefix('-').or_else(|| text.strip_prefix('+')) {
            format!("{}{}{rest}", &text[..1], "0".repeat(fill))
        } else {
            format!("{}{text}", "0".repeat(fill))
        }
    } else {
        format!("{}{text}", " ".repeat(fill))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(shell: &mut Shell, args: &[String]) -> (i32, String, String) {
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = Printf.run(shell, args, &mut stdin, &mut stdout, &mut stderr);
        (
            status,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn literal_text_and_percent_percent() {
        let mut shell = Shell::new();
        let (status, out, _) = run(&mut shell, &s(&["100%%\n"]));
        assert_eq!(status, 0);
        assert_eq!(out, "100%\n");
    }

    #[test]
    fn simple_conversions() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["%s-%d-%x\n", "hi", "10", "255"]));
        assert_eq!(out, "hi-10-ff\n");
    }

    #[test]
    fn recycles_the_format_over_extra_arguments() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["%s-%s\n", "a", "b", "c"]));
        assert_eq!(out, "a-b\nc-\n");
    }

    #[test]
    fn does_not_recycle_when_the_format_has_no_conversions() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["hi\n", "a", "b", "c"]));
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn missing_operands_default_to_empty_string_or_zero() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["[%s][%d]\n"]));
        assert_eq!(out, "[][0]\n");
    }

    #[test]
    fn width_and_left_justify_and_zero_pad() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["[%5d][%-5d][%05d]\n", "3", "3", "3"]));
        assert_eq!(out, "[    3][3    ][00003]\n");
    }

    #[test]
    fn zero_pad_keeps_the_sign_first() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["%05d\n", "-3"]));
        assert_eq!(out, "-0003\n");
    }

    #[test]
    fn precision_truncates_a_string() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["%.3s\n", "hello"]));
        assert_eq!(out, "hel\n");
    }

    #[test]
    fn star_width_pulls_an_argument() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["[%*d]\n", "5", "3"]));
        assert_eq!(out, "[    3]\n");
    }

    #[test]
    fn percent_b_decodes_backslash_escapes_in_its_argument() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["%b", "a\\tb\\n"]));
        assert_eq!(out, "a\tb\n");
    }

    #[test]
    fn percent_c_takes_the_arguments_first_character() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["%c\n", "hello"]));
        assert_eq!(out, "h\n");
    }

    #[test]
    fn quoted_character_numeric_argument() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["%d\n", "'A"]));
        assert_eq!(out, "65\n");
    }

    #[test]
    fn invalid_numeric_argument_uses_zero_and_reports_failure() {
        let mut shell = Shell::new();
        let (status, out, _) = run(&mut shell, &s(&["%d\n", "not-a-number"]));
        assert_eq!(status, 1);
        assert_eq!(out, "0\n");
    }

    #[test]
    fn dash_v_assigns_to_a_variable_instead_of_stdout() {
        let mut shell = Shell::new();
        let (status, out, _) = run(&mut shell, &s(&["-v", "result", "%s-%d", "x", "1"]));
        assert_eq!(status, 0);
        assert!(out.is_empty());
        assert_eq!(shell.get_var("result"), Some("x-1"));
    }

    #[test]
    fn format_string_backslash_escapes_are_decoded() {
        let mut shell = Shell::new();
        let (_, out, _) = run(&mut shell, &s(&["a\\tb\\n"]));
        assert_eq!(out, "a\tb\n");
    }
}
