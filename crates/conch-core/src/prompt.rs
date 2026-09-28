//! `PS1`/`PS2` prompt expansion: bash's own backslash-escape prompt
//! grammar (GNU Bash Reference Manual, "Controlling the Prompt"),
//! followed by ordinary parameter/command/arithmetic expansion of
//! whatever was *literally written* in the template — see
//! [`expand_prompt`]'s own docs for why those two passes are **not**
//! simply "decode backslashes into a buffer, then run the whole buffer
//! through the expansion engine a second time" the way that might sound
//! at first (a real command-injection footgun, not just an
//! implementation nicety — see below).
//!
//! # Lives here, not in `conch`'s own binary crate
//!
//! This is the pure, fully-unit-testable core half of `PS1`/`PS2`
//! expansion — `conch`'s binary crate (`main.rs`) is only the thin
//! adapter that decides *when* to call [`expand_prompt`] and what to do
//! with the result (pass it to `rustyline::Editor::readline`). It lives
//! here specifically so `tests/conch-difftest` can call it in-process
//! against a real bash oracle (`${PS1@P}`, via `${parameter@P}`) without
//! needing a pty — `conch`'s binary crate has no `[lib]` target, so
//! nothing outside it could ever call a function that stayed there.
//!
//! # A single interleaved pass, not "decode fully, then re-scan"
//!
//! Confirmed against real bash, empirically, not assumed: a directory
//! literally named `$(touch /tmp/PWNED)` used as `$PWD`, with `PS1`
//! containing `\w`, does **not** run that command every time the prompt
//! is shown — `\w`'s own *substituted* text is never re-scanned for
//! `$(...)`/`` `...` ``/`${...}` syntax. But a literal `$(echo dyn)`
//! written directly in the `PS1` *template* itself genuinely does
//! expand (also confirmed empirically). Both are true at once only if
//! backslash-escape decoding and dollar/backquote-expansion are one
//! *interleaved* left-to-right scan over the template's own literal
//! characters — each expansion site's *result* (whether from a
//! `\`-escape or a `$`/backquote site) is appended as final, inert text
//! and never re-examined — rather than a naive two-phase "decode
//! backslashes into a string, then hand that whole string to a generic
//! shell-expansion pass" pipeline, which *would* re-scan (and could
//! re-execute) whatever a `\w`/`\h`/`\u`/etc. substitution happened to
//! contain (a hostname, username, or directory name is exactly the kind
//! of "text that looks like it came from the user, but is actually
//! system/filesystem-controlled data" this project's own security
//! review process exists to catch).
//!
//! Equally important, and a distinct guarantee from the above: this
//! module **never** hands any part of `template` (or of a substituted
//! escape's own value) to the *command* parser
//! (`conch_shell_parser::parse`) or `exec_command_list`. The only parsing
//! that ever runs here is the narrow, single-`Word`-worth-of-expansion
//! path (`crate::expand_word_single`) over one already-recognized
//! `$`/backquote expansion *site* found in the template's own literal
//! text — never the general command grammar. That distinction matters:
//! a `;`, `|`, `>`, `&&`, or backgrounding `&` appearing anywhere in a
//! prompt (whether typed directly in a template, or arriving via a
//! `\w`/`\u`/etc. substitution) is always inert, literal text here,
//! exactly matching real bash's own confirmed behavior (`PS1='foo; touch
//! x'` and `PS1='foo > x'` both print those characters literally, never
//! executing anything) — never live shell syntax, regardless of where it
//! came from.
//!
//! # `promptvars` is unconditionally on
//!
//! Real bash gates the whole second (`$`/backquote) pass on the
//! `promptvars` shell option (`shopt`), on by default. Conch has no
//! `shopt` builtin at all (not in this phase's or any prior phase's
//! scope), so there is no way to ever turn it off — this always performs
//! that pass, matching bash's own default, documented here as a
//! deliberate simplification rather than a silently-ignored option.
//!
//! # `$?` is saved and restored around expansion
//!
//! Also confirmed empirically against real bash: `$?` after a command is
//! unaffected by whatever `$(...)` a *subsequently displayed* `PS1`
//! happens to run internally — bash does not let prompt-generation's own
//! command substitutions clobber the exit status the next command will
//! see reported by `$?`. [`expand_prompt`] reproduces this explicitly
//! (see its own body) since [`crate::expand_word_single`] naturally
//! updates [`Shell::last_status`] as an ordinary side effect of running a
//! command substitution, same as any other expansion site would.
//!
//! # What's reused, what's new
//!
//! `$`/backquote expansion-*site recognition* is
//! [`conch_shell_lexer::lex_dollar_expansion`]/[`conch_shell_lexer::lex_backquote_expansion`]
//! — the same standalone entry points `conch-shell-parser`'s own
//! arithmetic-expansion body scanner already reuses for its own "a third
//! quoting ruleset needs this same recognition" reason (see that crate's
//! module docs); this is a fourth. *Evaluating* a recognized site (real
//! parameter/command/arithmetic expansion, running `$(...)` if present)
//! is [`crate::expand_word_single`] — the same "no splitting, no
//! globbing, just expand-and-quote-remove" entry point assignment values
//! and redirect targets already use, exactly matching what an unsplit,
//! un-globbed *single* prompt string needs. Neither is reimplemented
//! here. Bash's own backslash-escape prompt-grammar table itself (`\u`,
//! `\h`, `\w`, `\$`, `\t`, ...) has no existing equivalent anywhere in
//! this codebase and is new, bounded, purpose-built parsing — the same
//! shape of work as `printf`'s own format mini-language
//! (`conch-shell-builtins::printf_builtin`).
//!
//! # Scope
//!
//! `PROMPT_DIRTRIM` (bash 4.0+, truncates `\w`/`\W` to the last *N* path
//! components) is out of scope for this phase — a real, but narrower and
//! less commonly used feature than the escape table itself, and not
//! called out as required by the phase plan. `\!`/`\#` (history number /
//! command number) are both implemented as the *same* value — the
//! current history list length, `+1` — rather than bash's own two
//! genuinely distinct counters (`\!` counts every persisted history
//! entry across sessions; `\#` counts only commands run in *this*
//! session). Getting `\#` exactly right needs an extra "history length
//! at session start" counter threaded in for very little practical
//! value (most real-world `PS1`s use neither escape, and the two only
//! visibly diverge once a persisted history file already has entries in
//! it) — documented here as a deliberate, narrow scope decision rather
//! than silently claimed correct.

use std::path::Path;

use conch_shell_lexer::Word;

use crate::Shell;

/// Conch's own default `PS1` — deliberately close to the interactive
/// loop's previous hardcoded prompt literal (`"conch {cwd} $ "`) to
/// minimize surprise, but built from real bash escapes (`\w` for the
/// working directory, `\$` for the root-aware `#`/`$` suffix this
/// crate's prior hardcoded prompt never had) rather than a fixed string,
/// so a user who overrides `$PS1` sees the same escape grammar bash
/// itself documents.
pub const DEFAULT_PS1: &str = "conch \\w \\$ ";

/// Conch's own default `PS2` — identical to real bash's own compiled-in
/// default (confirmed empirically): there's no reason for conch's own
/// continuation prompt to look different from the one every bash user
/// already recognizes.
pub const DEFAULT_PS2: &str = "> ";

/// `$PS1`, or [`DEFAULT_PS1`] if unset. Deliberately distinguishes unset
/// from set-but-empty (`PS1=""` is a valid, if unusual, real bash
/// configuration that shows no prompt at all) — only a genuinely unset
/// `$PS1` falls back to the default.
#[must_use]
pub fn ps1_template(shell: &Shell) -> String {
    shell
        .get_var("PS1")
        .map_or_else(|| DEFAULT_PS1.to_string(), str::to_string)
}

/// See [`ps1_template`]'s own docs — the `$PS2`/[`DEFAULT_PS2`] sibling.
#[must_use]
pub fn ps2_template(shell: &Shell) -> String {
    shell
        .get_var("PS2")
        .map_or_else(|| DEFAULT_PS2.to_string(), str::to_string)
}

/// Expands `template` (a `PS1`/`PS2` value) into the literal string to
/// display — see this module's own docs for the full "why" behind the
/// algorithm below.
///
/// `history_len` is the current history list length (e.g.
/// `editor.history().len()`, from `main.rs`) — used for the `\!`/`\#`
/// escapes (see this module's own docs for the one way those two don't
/// quite match real bash).
#[must_use]
pub fn expand_prompt(shell: &mut Shell, template: &str, history_len: usize) -> String {
    let saved_status = shell.last_status;
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    let len = template.len();

    while i < len {
        let rest = &template[i..];
        let ch = rest.chars().next().expect("i < len");
        match ch {
            '\\' => {
                let (text, consumed) = decode_escape(shell, history_len, rest);
                out.push_str(&text);
                i += consumed.max(1);
            }
            '$' => match conch_shell_lexer::lex_dollar_expansion(rest) {
                Ok((segment, consumed)) => {
                    append_expanded(&mut out, shell, segment);
                    i += consumed.max(1);
                }
                Err(_) => {
                    // Malformed `$(...)`/`${...}`/`$((...))` written
                    // directly in the user's own `PS1` -- never worth
                    // failing prompt display over; show the `$` literally
                    // and keep going, same tolerant policy `\X`'s
                    // "unrecognized escape stays literal" fallback below
                    // uses.
                    out.push('$');
                    i += 1;
                }
            },
            '`' => match conch_shell_lexer::lex_backquote_expansion(rest) {
                Ok((segment, consumed)) => {
                    append_expanded(&mut out, shell, segment);
                    i += consumed.max(1);
                }
                Err(_) => {
                    out.push('`');
                    i += 1;
                }
            },
            other => {
                out.push(other);
                i += other.len_utf8();
            }
        }
    }

    // See this module's own docs: a `$(...)`/`` `...` `` evaluated as
    // *part of generating the prompt* must never be visible to the next
    // command's own `$?` — confirmed against real bash empirically.
    shell.last_status = saved_status;
    out
}

/// Evaluates one recognized `$`/backquote expansion `segment` (real
/// parameter/command/arithmetic expansion — see this module's own docs)
/// and appends its result to `out`. A failed expansion (e.g. `${x:?}` on
/// an unset parameter, or a command substitution that fails to spawn) is
/// deliberately swallowed rather than propagated: unlike an ordinary
/// command's own expansion errors, a broken expansion site in a user's
/// own custom `PS1` must never crash prompt display or abort the REPL —
/// see [`Shell::nounset`]/`report_expand_error`'s docs for the ordinary
/// executor-level fatality this deliberately does *not* invoke here.
fn append_expanded(out: &mut String, shell: &mut Shell, segment: conch_shell_lexer::WordSegment) {
    let word = Word::new(vec![segment]);
    if let Ok(text) = crate::expand_word_single(&word, shell) {
        out.push_str(&text);
    }
}

/// Decodes one bash `PS1`/`PS2` backslash escape starting at `rest`
/// (which must start with `\`), returning the literal text it expands to
/// and how many bytes of `rest` it consumed. An unrecognized `\X` is
/// left as the literal two-character text `\X` — confirmed against real
/// bash: not an error, and the backslash is *not* silently dropped.
fn decode_escape(shell: &Shell, history_len: usize, rest: &str) -> (String, usize) {
    debug_assert!(rest.starts_with('\\'));
    let after = &rest[1..];
    let Some(c) = after.chars().next() else {
        // A lone trailing '\' with nothing left to escape.
        return ("\\".to_string(), 1);
    };

    match c {
        'a' => ("\x07".to_string(), 2),
        'd' => (strftime_local("%a %b %e"), 2),
        'D' => decode_strftime_escape(after),
        'e' => ("\x1b".to_string(), 2),
        'h' => (
            crate::hostname()
                .and_then(|h| h.split('.').next().map(str::to_string))
                .unwrap_or_default(),
            2,
        ),
        'H' => (crate::hostname().unwrap_or_default(), 2),
        'j' => (shell.job_table.iter().count().to_string(), 2),
        'l' => (tty_basename(), 2),
        'n' => ("\n".to_string(), 2),
        'r' => ("\r".to_string(), 2),
        's' => (shell_name(shell), 2),
        't' => (strftime_local("%H:%M:%S"), 2),
        'T' => (strftime_local("%I:%M:%S"), 2),
        '@' => (strftime_local("%I:%M %p"), 2),
        'A' => (strftime_local("%H:%M"), 2),
        'u' => (shell.get_var("USER").unwrap_or_default().to_string(), 2),
        'v' => (
            format!(
                "{}.{}",
                env!("CARGO_PKG_VERSION_MAJOR"),
                env!("CARGO_PKG_VERSION_MINOR")
            ),
            2,
        ),
        'V' => (env!("CARGO_PKG_VERSION").to_string(), 2),
        'w' => (cwd_full(shell), 2),
        'W' => (cwd_basename(shell), 2),
        '!' | '#' => (history_len.saturating_add(1).to_string(), 2),
        '$' => (
            (if crate::is_effective_root() { "#" } else { "$" }).to_string(),
            2,
        ),
        '\\' => ("\\".to_string(), 2),
        // Bash's own non-printing-sequence markers (wrap ANSI color
        // codes so *real* GNU readline's cursor-position math can skip
        // them). Dropped entirely here rather than translated to some
        // rustyline-specific marker: confirmed by reading rustyline
        // 18.0.1's own `tty::width` that it already recognizes and
        // zero-widths a literal ANSI CSI sequence on its own (no
        // bracket-marker mechanism needed at all) -- see `conch`'s own
        // `main.rs` for the fuller "why", since it's an easy thing to
        // assume rustyline needs bash's own `\[`/`\]` convention for
        // when it actually doesn't.
        '[' | ']' => (String::new(), 2),
        '0'..='7' => {
            let digits: String = after
                .chars()
                .take(3)
                .take_while(|c| ('0'..='7').contains(c))
                .collect();
            let value = u32::from_str_radix(&digits, 8).unwrap_or(0);
            let decoded = u8::try_from(value).map(char::from).unwrap_or('\0');
            (decoded.to_string(), 1 + digits.len())
        }
        other => (format!("\\{other}"), 1 + other.len_utf8()),
    }
}

/// `\D{format}` — `after` is everything past the `\D` itself (so this
/// starts by checking for the opening `{`). An empty `{}` uses `%X`
/// (locale's own default time representation), confirmed empirically
/// against real bash. A malformed `\D` (no `{...}` at all, or no closing
/// `}`) falls back to the literal text `\D`, same "unrecognized/broken
/// escape stays literal" policy [`decode_escape`]'s own catch-all uses.
fn decode_strftime_escape(after: &str) -> (String, usize) {
    let Some(rest) = after[1..].strip_prefix('{') else {
        return ("\\D".to_string(), 2);
    };
    let Some(close) = rest.find('}') else {
        return ("\\D".to_string(), 2);
    };
    let format = &rest[..close];
    let format = if format.is_empty() { "%X" } else { format };
    // "\D" + "{" + format + "}"
    let consumed = 2 + 1 + close + 1;
    (strftime_local(format), consumed)
}

/// Formats the current local time via the real system `strftime(3)` —
/// deliberately the actual libc call (via `nix::libc`, no safe wrapper
/// exists in `nix` itself for this), not a hand-rolled reimplementation:
/// this is the one narrow escape (`\D{format}`) that accepts an
/// arbitrary, user-supplied format string, and `strftime(3)` is exactly
/// what real bash itself calls for the identical feature — reusing it
/// guarantees identical output (including locale-dependent specifiers
/// like `%x`/`%X`/`%a`/`%b`) without this crate needing its own format
/// mini-language for every `strftime` specifier. `format` is fully
/// user-controlled (from the user's own `$PS1`/`$PS2`) but that crosses
/// no privilege/trust boundary `strftime(3)` doesn't already sit behind
/// for real bash's own identical feature — matching, not exceeding, real
/// bash's own trust model.
fn strftime_local(format: &str) -> String {
    let Ok(c_format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    // SAFETY: `t`/`tm` are plain, fully-owned local values;
    // `localtime_r`/`strftime` are given valid pointers to them and a
    // `buf` slice whose exact length is passed as `strftime`'s own `max`
    // bound, so it can never write past `buf`'s end. `strftime` returns
    // `0` (handled below, not treated as success) rather than
    // overflowing when the formatted result wouldn't fit.
    unsafe {
        let t: nix::libc::time_t = nix::libc::time(std::ptr::null_mut());
        let mut tm: nix::libc::tm = std::mem::zeroed();
        if nix::libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = vec![0_u8; 256];
        let written =
            nix::libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c_format.as_ptr(), &tm);
        if written == 0 {
            return String::new();
        }
        buf.truncate(written);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// The basename of the controlling terminal device (`PS1`'s `\l`) — an
/// empty string if there isn't one (e.g. no real tty, matching real
/// bash's own silent-empty fallback there too).
fn tty_basename() -> String {
    nix::unistd::ttyname(std::io::stdin())
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// `PS1`'s `\s` — the shell's own name: `$0`'s basename, with one
/// leading `-` stripped (real bash's own login-shell `argv[0]`
/// convention — harmless to strip here even though conch has no
/// login-shell concept of its own, since a plain `"conch"` `$0` never
/// starts with `-` anyway).
fn shell_name(shell: &Shell) -> String {
    let base = Path::new(&shell.arg0).file_name().map_or_else(
        || shell.arg0.clone(),
        |name| name.to_string_lossy().into_owned(),
    );
    base.strip_prefix('-').map_or(base.clone(), str::to_string)
}

/// `PS1`'s `\w` — `$PWD`, with a `$HOME` prefix abbreviated to `~`
/// (including `$PWD == $HOME` itself, shown as exactly `~`) — confirmed
/// against real bash, both cases, empirically.
fn cwd_full(shell: &Shell) -> String {
    let cwd = shell.cwd.to_string_lossy();
    tildeize(&cwd, shell).unwrap_or_else(|| cwd.into_owned())
}

/// `PS1`'s `\W` — the basename of `$PWD`, except when `$PWD` is exactly
/// `$HOME` (shown as `~`, not the basename of the home directory) —
/// confirmed against real bash, including the `$PWD == "/"` edge case
/// (`Path::file_name` returns `None` for `/`; falls back to the full
/// `"/"`, matching real bash's own `\W` there too).
fn cwd_basename(shell: &Shell) -> String {
    let cwd = shell.cwd.to_string_lossy();
    if let Some(home) = shell.get_var("HOME")
        && !home.is_empty()
        && cwd == home
    {
        return "~".to_string();
    }
    Path::new(cwd.as_ref()).file_name().map_or_else(
        || cwd.clone().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Shared `\w`/`\W` `$HOME`-abbreviation logic — see both callers' own
/// docs for the exact, empirically-confirmed behavior this reproduces.
fn tildeize(cwd: &str, shell: &Shell) -> Option<String> {
    let home = shell.get_var("HOME")?;
    if home.is_empty() {
        return None;
    }
    if cwd == home {
        Some("~".to_string())
    } else {
        cwd.strip_prefix(home)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| format!("~/{rest}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_with(home: &str, cwd: &str) -> Shell {
        let mut shell = Shell::new();
        shell.env_vars.insert("HOME".to_string(), home.to_string());
        shell.cwd = std::path::PathBuf::from(cwd);
        shell
    }

    #[test]
    fn default_ps1_is_used_when_unset() {
        let shell = Shell::new();
        assert_eq!(ps1_template(&shell), DEFAULT_PS1);
    }

    #[test]
    fn explicit_empty_ps1_is_not_the_default() {
        let mut shell = Shell::new();
        shell.env_vars.insert("PS1".to_string(), String::new());
        assert_eq!(ps1_template(&shell), "");
    }

    #[test]
    fn literal_text_passes_through_unchanged() {
        let mut shell = Shell::new();
        assert_eq!(expand_prompt(&mut shell, "hello $ ", 0), "hello $ ");
    }

    #[test]
    fn w_expands_full_cwd_with_home_abbreviated() {
        let mut shell = shell_with("/home/conch", "/home/conch/proj");
        assert_eq!(expand_prompt(&mut shell, "\\w", 0), "~/proj");
    }

    #[test]
    fn w_shows_tilde_exactly_at_home() {
        let mut shell = shell_with("/home/conch", "/home/conch");
        assert_eq!(expand_prompt(&mut shell, "\\w", 0), "~");
    }

    #[test]
    fn capital_w_shows_basename_only() {
        let mut shell = shell_with("/home/conch", "/home/conch/proj/sub");
        assert_eq!(expand_prompt(&mut shell, "\\W", 0), "sub");
    }

    #[test]
    fn capital_w_shows_tilde_at_home() {
        let mut shell = shell_with("/home/conch", "/home/conch");
        assert_eq!(expand_prompt(&mut shell, "\\W", 0), "~");
    }

    #[test]
    fn dollar_sign_is_dollar_for_non_root() {
        let mut shell = Shell::new();
        // The test process is essentially never euid 0.
        assert_eq!(expand_prompt(&mut shell, "\\$", 0), "$");
    }

    #[test]
    fn unrecognized_escape_stays_literal() {
        let mut shell = Shell::new();
        assert_eq!(expand_prompt(&mut shell, "[\\q]", 0), "[\\q]");
    }

    #[test]
    fn trailing_lone_backslash_stays_literal() {
        let mut shell = Shell::new();
        assert_eq!(expand_prompt(&mut shell, "abc\\", 0), "abc\\");
    }

    #[test]
    fn octal_escape_decodes_to_the_named_byte() {
        let mut shell = Shell::new();
        assert_eq!(expand_prompt(&mut shell, "\\101", 0), "A");
    }

    #[test]
    fn bracket_markers_are_dropped() {
        let mut shell = Shell::new();
        assert_eq!(
            expand_prompt(&mut shell, "\\[\\e[32m\\]ok\\[\\e[0m\\]", 0),
            "\x1b[32mok\x1b[0m"
        );
    }

    #[test]
    fn dollar_variable_in_template_is_expanded() {
        let mut shell = Shell::new();
        shell.env_vars.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_prompt(&mut shell, "[$FOO]", 0), "[bar]");
    }

    #[test]
    fn dynamic_escape_output_is_not_re_expanded() {
        // The classic injection shape: a directory (or hostname/user)
        // that happens to *contain* shell syntax must never be
        // re-interpreted as one just because it was substituted in via
        // `\w`.
        let mut shell = shell_with("/home/conch", "/home/conch/$(x)");
        assert_eq!(expand_prompt(&mut shell, "\\w", 0), "~/$(x)");
    }

    #[test]
    fn semicolons_and_redirection_operators_in_a_dynamic_value_stay_inert() {
        // Directly exercises the "never hands anything to the command
        // parser" guarantee this module's own docs describe -- confirmed
        // this is exactly real bash's own behavior too (`PS1='foo; touch
        // x'`/`PS1='foo > x'` both print the operator characters
        // literally, empirically, before this test was written).
        let mut shell = shell_with("/home/conch", "/home/conch/a;b|c>d&e");
        assert_eq!(expand_prompt(&mut shell, "\\w", 0), "~/a;b|c>d&e");
    }

    #[test]
    fn last_status_is_restored_after_command_substitution() {
        let mut shell = Shell::new();
        shell.last_status = 42;
        let _ = expand_prompt(&mut shell, "$(true)", 0);
        assert_eq!(shell.last_status, 42);
    }

    #[test]
    fn history_escape_uses_history_len_plus_one() {
        let mut shell = Shell::new();
        assert_eq!(expand_prompt(&mut shell, "\\!", 41), "42");
        assert_eq!(expand_prompt(&mut shell, "\\#", 41), "42");
    }

    #[test]
    fn newline_and_backslash_escapes_decode() {
        let mut shell = Shell::new();
        assert_eq!(expand_prompt(&mut shell, "a\\nb\\\\c", 0), "a\nb\\c");
    }
}
