//! `read` (POSIX 2.9.4 / bash manual §4.2) — reads one line (or, with
//! `-n`/`-N`, a fixed character count) from `stdin` and assigns it,
//! `IFS`-field-split, to the named variables (default: `REPLY`).
//!
//! `stdin` here is exactly whatever `conch-shell-core::exec::exec_builtin`
//! resolved for this invocation (an explicit `<` redirect, piped-in bytes
//! from the previous pipeline stage, or the real inherited process
//! stdin) — see [`conch_shell_core::Builtin::run`]'s own docs for why
//! that's a real, load-bearing plumbing fix this phase needed rather than
//! something already in place: `read` is the first builtin in this crate
//! that ever consumes its own stdin at all.

use std::io::{Read, Write};

use conch_shell_core::{Builtin, Shell};

/// `read [-r] [-s] [-p prompt] [-d delim] [-n|-N nchars] [-t timeout]
/// [-u fd] [name...]`.
///
/// Known gaps, flagged rather than silently misbehaving: `-t timeout`
/// and `-u fd` are accepted (so a script using them doesn't hit a hard
/// parse/usage error) but not actually honored — `-t` never times out
/// (reads block exactly as if it were absent) and `-u` is ignored (this
/// builtin's own `stdin` parameter is always used, never an arbitrary
/// other fd). Both would need real `poll`/`select`-shaped machinery this
/// crate has no dependency for (`conch-shell-builtins` deliberately has
/// no direct `nix` dependency of its own — see the crate module docs),
/// and — for `-t` specifically — `stdin` here is frequently *not* a raw
/// OS fd at all (a pipeline's piped-in bytes are an in-memory
/// `Cursor`), so "wait up to N seconds for readability" doesn't even
/// have a consistent meaning across every `stdin` source this builtin
/// can receive. `-a array` (bash extension) isn't implemented at all —
/// conch has no array support yet (a project-wide gap, not specific to
/// `read`).
pub struct ReadBuiltin;
impl Builtin for ReadBuiltin {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let options = match ReadOptions::parse(args) {
            Ok(options) => options,
            Err(message) => {
                let _ = writeln!(stderr, "read: {message}");
                return 2;
            }
        };

        if let Some(prompt) = &options.prompt {
            let _ = write!(std::io::stderr(), "{prompt}");
            let _ = std::io::stderr().flush();
        }

        let (bytes, protected, hit_eof) = if let Some(n) = options.nchars {
            let (bytes, hit_eof) =
                read_n_chars(stdin, n, options.ignore_delim_for_nchars, options.delim);
            let protected = vec![false; bytes.len()];
            (bytes, protected, hit_eof)
        } else {
            read_logical_line(stdin, options.raw, options.delim)
        };

        let names = if options.names.is_empty() {
            vec!["REPLY".to_string()]
        } else {
            options.names
        };

        let ifs = shell
            .get_var("IFS")
            .map_or_else(|| " \t\n".to_string(), str::to_string);
        let fields = split_bytes_for_read(&bytes, &protected, &ifs, names.len());

        for (i, name) in names.iter().enumerate() {
            let value = fields.get(i).cloned().unwrap_or_default();
            shell.shell_vars.insert(name.clone(), value);
        }

        // POSIX: exit status is nonzero if EOF was reached before a
        // complete line (or the requested character count) was read —
        // confirmed against real bash this is true even when *some*
        // partial data was still read and assigned (the assignment above
        // already happened unconditionally; only the exit status
        // reflects the EOF).
        i32::from(hit_eof)
    }
}

struct ReadOptions {
    raw: bool,
    prompt: Option<String>,
    delim: u8,
    nchars: Option<usize>,
    ignore_delim_for_nchars: bool,
    names: Vec<String>,
}

impl ReadOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut raw = false;
        let mut prompt = None;
        let mut delim = b'\n';
        let mut nchars = None;
        let mut ignore_delim_for_nchars = false;
        let mut iter = args.iter().peekable();

        while let Some(arg) = iter.peek() {
            match arg.as_str() {
                "-r" => {
                    raw = true;
                    iter.next();
                }
                "-s" | "-e" => {
                    // `-s` (silent/no-echo) and `-e` (readline-style
                    // editing) both need real terminal-mode control this
                    // builtin has no infrastructure for -- accepted as a
                    // documented no-op rather than a hard usage error,
                    // since neither changes *what* gets read, only how
                    // it's echoed/edited while typing interactively.
                    iter.next();
                }
                "-p" => {
                    iter.next();
                    let value = iter
                        .next()
                        .ok_or_else(|| "-p: option requires an argument".to_string())?;
                    prompt = Some(value.clone());
                }
                "-d" => {
                    iter.next();
                    let value = iter
                        .next()
                        .ok_or_else(|| "-d: option requires an argument".to_string())?;
                    delim = value.bytes().next().unwrap_or(0);
                }
                "-n" | "-N" => {
                    let ignore_delim = arg.as_str() == "-N";
                    iter.next();
                    let value = iter
                        .next()
                        .ok_or_else(|| "option requires an argument".to_string())?;
                    let n = value
                        .parse::<usize>()
                        .map_err(|_| format!("{value}: invalid number"))?;
                    nchars = Some(n);
                    ignore_delim_for_nchars = ignore_delim;
                }
                "-t" | "-u" => {
                    // Accepted-but-not-honored -- see this module's own
                    // docs.
                    iter.next();
                    iter.next()
                        .ok_or_else(|| "option requires an argument".to_string())?;
                }
                "--" => {
                    iter.next();
                    break;
                }
                _ => break,
            }
        }

        let names: Vec<String> = iter.cloned().collect();
        for name in &names {
            if !is_valid_identifier(name) {
                return Err(format!("`{name}': not a valid identifier"));
            }
        }

        Ok(Self {
            raw,
            prompt,
            delim,
            nchars,
            ignore_delim_for_nchars,
            names,
        })
    }
}

/// The same rule `local`/`unset` already enforce — duplicated for the
/// same "five-line, self-contained rule" reason those do (see e.g.
/// `is_valid_identifier` in this crate's root module).
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Reads one logical line from `stdin`, honoring backslash handling per
/// POSIX 2.9.4: in non-raw mode, a backslash immediately followed by
/// `delim` is a line continuation (both characters are dropped, and
/// reading continues as if they'd never appeared — this is what lets a
/// single logical `read` line span several physical input lines); a
/// backslash followed by anything else is dropped and the following
/// character is kept literally, *and marked protected* in the returned
/// mask so [`split_bytes_for_read`] never treats it as an `IFS`
/// delimiter later — confirmed against real bash: `read a b <<< 'one\
/// two'` (an escaped space) leaves `a` as `"one two"` and `b` empty, not
/// split at the escaped space the way an *un*escaped one would be. This
/// is the same "per-character quoting tag, not a flattened string"
/// requirement `conch-shell-core::expand`'s own module docs describe for
/// ordinary word splitting — `read`'s input has no quote characters at
/// all, only backslash-escaping, but the underlying reason (a
/// downstream, flattened string can't tell an escaped delimiter from a
/// real one) is identical. In raw mode (`-r`), a backslash is just an
/// ordinary, unprotected character with no special meaning at all.
///
/// Returns the accumulated bytes, a same-length protection mask, and
/// whether EOF was hit before `delim` was ever seen (POSIX: `read`'s own
/// nonzero-on-EOF exit status).
fn read_logical_line(stdin: &mut dyn Read, raw: bool, delim: u8) -> (Vec<u8>, Vec<bool>, bool) {
    let mut buf = Vec::new();
    let mut protected = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(1) => {}
            _ => return (buf, protected, true),
        }
        let c = byte[0];
        if !raw && c == b'\\' {
            let mut next = [0u8; 1];
            match stdin.read(&mut next) {
                Ok(1) => {
                    if next[0] == delim {
                        // Line continuation: swallow both characters,
                        // keep reading the same logical line.
                        continue;
                    }
                    buf.push(next[0]);
                    protected.push(true);
                    continue;
                }
                // A trailing backslash immediately at EOF: nothing to
                // escape, drop it (matches real bash).
                _ => return (buf, protected, true),
            }
        }
        if c == delim {
            return (buf, protected, false);
        }
        buf.push(c);
        protected.push(false);
    }
}

/// `-n`/`-N nchars` — reads up to `nchars` raw bytes. `-n` (`ignore_delim:
/// false`) still stops early if `delim` is seen first (matching real
/// bash: `-n` is "at most this many, but a newline still ends it early
/// unless `-d ''` was also given"); `-N` (`ignore_delim: true`) reads
/// exactly `nchars` bytes regardless of their content, stopping early
/// only on EOF.
///
/// Known simplification: real bash counts a backslash-escaped character
/// as a single "character" toward `nchars` (consuming 2 input bytes for
/// it), matching its own line-mode escaping; this reads `nchars` raw
/// bytes with no backslash processing at all — a real but narrow gap
/// (`-n`/`-N` combined with backslash-containing input specifically),
/// flagged here rather than silently claiming full fidelity.
fn read_n_chars(
    stdin: &mut dyn Read,
    nchars: usize,
    ignore_delim: bool,
    delim: u8,
) -> (Vec<u8>, bool) {
    let mut buf = Vec::with_capacity(nchars);
    let mut byte = [0u8; 1];
    while buf.len() < nchars {
        match stdin.read(&mut byte) {
            Ok(1) => {
                if !ignore_delim && byte[0] == delim {
                    return (buf, false);
                }
                buf.push(byte[0]);
            }
            _ => return (buf, true),
        }
    }
    (buf, false)
}

/// `read`'s own `IFS` field-splitting — bounded to at most `max_fields`
/// entries, with the *last* one being everything left from wherever the
/// second-to-last field's own trailing delimiter/whitespace run ended,
/// trimmed only of *trailing* `IFS` whitespace (not re-split any
/// further, and critically not re-*joined* from independently-split
/// pieces either, which would lose any embedded, non-whitespace `IFS`
/// delimiter character past that point — confirmed against real bash:
/// `IFS=:; read a b <<< "x:y:z"` leaves `b` as `y:z`, delimiter intact).
///
/// Deliberately a standalone implementation rather than a reuse of
/// `conch-shell-core::expand`'s internal `split_fields` (the engine
/// ordinary command-argument word splitting uses): that function has no
/// "stop splitting early, keep the remainder verbatim" mode at all (every
/// caller there wants *every* field, independently split-eligible), and
/// — the harder requirement — this function also needs `protected[i] ==
/// true` to mean "`bytes[i]` must never be classified as `IFS`
/// whitespace or a delimiter, regardless of `ifs`'s own contents,
/// because it was backslash-escaped in the original input" (see
/// [`read_logical_line`]'s own docs for why: `read`'s backslash-escaping
/// interacts with `IFS` splitting in a way a plain, already-flattened
/// `&str` can't represent, confirmed against real bash with `read a b
/// <<< 'one\ two'` — an escaped space — leaving `a` as `"one two"` and
/// `b` empty, not split at the escaped space the way an unescaped one
/// would be). Retrofitting either behavior onto `split_fields` itself
/// would mean threading a new capability through an already-intricate,
/// separately-tested function for a caller-shape only `read` needs.
///
/// Operates on raw bytes rather than `char`s — `read`'s own input is raw
/// bytes off `stdin` to begin with, and (`IFS` characters being always
/// single-byte ASCII in practice) byte granularity is both sufficient
/// and simpler than decoding UTF-8 just to re-encode it a moment later;
/// [`String::from_utf8_lossy`] is applied once per field, at the end, so
/// non-UTF-8 `stdin` content degrades gracefully instead of panicking.
fn split_bytes_for_read(
    bytes: &[u8],
    protected: &[bool],
    ifs: &str,
    max_fields: usize,
) -> Vec<String> {
    if max_fields == 0 {
        return Vec::new();
    }
    let ifs_bytes: Vec<u8> = ifs.bytes().collect();
    let is_ifs_ws =
        |i: usize| !protected[i] && ifs_bytes.contains(&bytes[i]) && bytes[i].is_ascii_whitespace();
    let is_ifs_delim = |i: usize| {
        !protected[i] && ifs_bytes.contains(&bytes[i]) && !bytes[i].is_ascii_whitespace()
    };

    let len = bytes.len();
    let mut fields: Vec<String> = Vec::new();
    let mut i = 0;
    while i < len && is_ifs_ws(i) {
        i += 1;
    }
    let mut field_start = i;

    while fields.len() + 1 < max_fields && i < len {
        if is_ifs_ws(i) || is_ifs_delim(i) {
            fields.push(String::from_utf8_lossy(&bytes[field_start..i]).into_owned());
            if is_ifs_delim(i) {
                i += 1;
            }
            while i < len && is_ifs_ws(i) {
                i += 1;
            }
            field_start = i;
        } else {
            i += 1;
        }
    }

    let mut end = len;
    while end > field_start && is_ifs_ws(end - 1) {
        end -= 1;
    }
    fields.push(String::from_utf8_lossy(&bytes[field_start..end]).into_owned());
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[String], stdin_bytes: &[u8]) -> (i32, Shell) {
        let mut shell = Shell::new();
        let mut stdin = std::io::Cursor::new(stdin_bytes.to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = ReadBuiltin.run(&mut shell, args, &mut stdin, &mut stdout, &mut stderr);
        (status, shell)
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_a_single_line_into_reply_by_default() {
        let (status, shell) = run(&s(&[]), b"hello world\n");
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("REPLY"), Some("hello world"));
    }

    #[test]
    fn splits_across_named_variables() {
        let (status, shell) = run(&s(&["a", "b", "c"]), b"x y z\n");
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("a"), Some("x"));
        assert_eq!(shell.get_var("b"), Some("y"));
        assert_eq!(shell.get_var("c"), Some("z"));
    }

    #[test]
    fn last_variable_gets_the_remainder_verbatim() {
        let (_, shell) = run(&s(&["a", "b"]), b"x y z\n");
        assert_eq!(shell.get_var("a"), Some("x"));
        assert_eq!(shell.get_var("b"), Some("y z"));
    }

    #[test]
    fn eof_without_a_trailing_newline_is_a_nonzero_status_but_still_assigns() {
        let (status, shell) = run(&s(&["a"]), b"no newline here");
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("a"), Some("no newline here"));
    }

    #[test]
    fn eof_on_an_empty_stream_is_a_nonzero_status_with_empty_variables() {
        let (status, shell) = run(&s(&["a"]), b"");
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("a"), Some(""));
    }

    #[test]
    fn backslash_newline_is_a_line_continuation() {
        let (status, shell) = run(&s(&["a"]), b"one \\\ntwo\n");
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("a"), Some("one two"));
    }

    #[test]
    fn backslash_escapes_the_next_character_including_whitespace() {
        // `one\ two` -- the escaped space must not be treated as an IFS
        // delimiter, so `a` gets the whole thing as a single field.
        let (_, shell) = run(&s(&["a", "b"]), b"one\\ two\n");
        assert_eq!(shell.get_var("a"), Some("one two"));
        assert_eq!(shell.get_var("b"), Some(""));
    }

    #[test]
    fn dash_r_disables_backslash_handling_entirely() {
        let (_, shell) = run(&s(&["-r", "a"]), b"one\\ two\n");
        assert_eq!(shell.get_var("a"), Some("one\\ two"));
    }

    #[test]
    fn dash_d_changes_the_line_delimiter() {
        let (status, shell) = run(&s(&["-d", ";", "a"]), b"one;two\n");
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("a"), Some("one"));
    }

    #[test]
    fn dash_n_reads_a_fixed_character_count() {
        let (status, shell) = run(&s(&["-n", "3", "a"]), b"abcdef");
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("a"), Some("abc"));
    }

    #[test]
    fn dash_n_still_stops_early_on_the_delimiter() {
        let (_, shell) = run(&s(&["-n", "10", "a"]), b"ab\ncd");
        assert_eq!(shell.get_var("a"), Some("ab"));
    }

    #[test]
    fn dash_big_n_ignores_the_delimiter() {
        let (_, shell) = run(&s(&["-N", "5", "a"]), b"ab\ncd");
        assert_eq!(shell.get_var("a"), Some("ab\ncd"));
    }

    #[test]
    fn invalid_identifier_is_a_usage_error() {
        let (status, _) = run(&s(&["1x"]), b"hi\n");
        assert_eq!(status, 2);
    }

    #[test]
    fn honors_a_custom_ifs() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("IFS".to_string(), ":".to_string());
        let mut stdin = std::io::Cursor::new(b"x:y:z\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = ReadBuiltin.run(
            &mut shell,
            &s(&["a", "b"]),
            &mut stdin,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("a"), Some("x"));
        assert_eq!(shell.get_var("b"), Some("y:z"));
    }
}
