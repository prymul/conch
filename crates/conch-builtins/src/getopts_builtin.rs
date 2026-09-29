//! `getopts optstring name [arg...]` (POSIX 2.9.4 / utility 12.2) —
//! parses positional-parameter-shaped options one at a time per call,
//! meant to be driven from a `while getopts ...; do ... done` loop.
//!
//! # Grounding: the exact algorithm
//!
//! Per POSIX's own `getopts` description:
//! - `OPTIND` (a plain shell variable, `1` if unset) is the 1-based index
//!   of the next operand to examine.
//! - `optstring` lists recognized option letters; a letter immediately
//!   followed by `:` requires an option-argument (either the rest of the
//!   same operand, e.g. `-ofile`, or the *next* operand, e.g. `-o file`).
//! - A leading `:` in `optstring` switches to *silent* error reporting:
//!   an invalid option sets `name` to `?` and `OPTARG` to the offending
//!   character with no diagnostic printed; a missing required
//!   option-argument sets `name` to `:` (not `?`) and `OPTARG` to the
//!   option character, again with no diagnostic. Without the leading
//!   `:`, both cases print their own diagnostic to stderr, set `name` to
//!   `?`, and leave `OPTARG` unset.
//! - End of options is recognized by the first `--` operand, the first
//!   operand that doesn't start with `-` (or is exactly `-`), or running
//!   out of operands entirely — `getopts` returns nonzero (`1`) *only*
//!   in this case, setting `name` to `?`; every other outcome (a valid
//!   option, an invalid one, or a missing required argument) returns
//!   `0`, since the calling `while` loop is expected to keep iterating
//!   and switch on `name`'s value itself.
//! - `OPTIND` after end-of-options points at the first non-option
//!   operand (or one past the last operand if there isn't one), letting
//!   the calling script continue processing `"$@"` from `$OPTIND` onward
//!   as the non-option arguments.
//!
//! Persisted state beyond `OPTIND`/`OPTARG` themselves (needed to resume
//! parsing partway through a bundled-short-option operand like `-abc`
//! across separate calls) lives on [`Shell::getopts_sub_index`]/
//! [`Shell::getopts_last_optind`] — see those fields' own docs for why.

use std::io::{Read, Write};

use conch_shell_core::Builtin;
use conch_shell_core::Shell;

pub struct Getopts;
impl Builtin for Getopts {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let Some(optstring) = args.first() else {
            let _ = writeln!(stderr, "getopts: usage: getopts optstring name [arg...]");
            return 2;
        };
        let Some(name) = args.get(1) else {
            let _ = writeln!(stderr, "getopts: usage: getopts optstring name [arg...]");
            return 2;
        };
        if !is_valid_identifier(name) {
            let _ = writeln!(stderr, "getopts: {name}: not a valid identifier");
            return 2;
        }

        let operands: Vec<String> = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            shell.positional_params.clone()
        };

        let silent = optstring.starts_with(':');
        let spec: Vec<char> = optstring.trim_start_matches(':').chars().collect();

        let optind = shell
            .get_var("OPTIND")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(1);
        if shell.getopts_last_optind != Some(optind) {
            shell.getopts_sub_index = 1;
        }
        let optind = optind as usize;

        // ---- end-of-options cases: the only ones that return 1 --------
        if optind == 0 || optind > operands.len() {
            finish(shell, name, "?", None, optind as i64);
            return 1;
        }

        let current = &operands[optind - 1];
        if shell.getopts_sub_index == 1 {
            if current == "--" {
                finish(shell, name, "?", None, (optind + 1) as i64);
                return 1;
            }
            if current == "-" || !current.starts_with('-') {
                finish(shell, name, "?", None, optind as i64);
                return 1;
            }
        }

        let chars: Vec<char> = current.chars().collect();
        let char_pos = shell.getopts_sub_index;
        let Some(&opt_char) = chars.get(char_pos) else {
            // Exhausted this operand without ever matching a bundled
            // option that would have already advanced past it (shouldn't
            // normally happen — every path below advances OPTIND once
            // an operand's characters run out) — treat as end of options
            // defensively rather than panicking or looping forever.
            finish(shell, name, "?", None, (optind + 1) as i64);
            return 1;
        };

        let Some(spec_pos) = spec.iter().position(|&c| c == opt_char) else {
            let next = advance_past_char(&chars, char_pos, optind);
            if silent {
                finish(shell, name, "?", Some(&opt_char.to_string()), next);
            } else {
                let _ = writeln!(stderr, "{name}: illegal option -- {opt_char}");
                finish(shell, name, "?", None, next);
            }
            return 0;
        };

        let needs_arg = spec.get(spec_pos + 1) == Some(&':');
        if !needs_arg {
            let next = advance_past_char(&chars, char_pos, optind);
            finish(shell, name, &opt_char.to_string(), None, next);
            return 0;
        }

        // The option-argument is either the rest of this same operand...
        if char_pos + 1 < chars.len() {
            let value: String = chars[char_pos + 1..].iter().collect();
            finish(
                shell,
                name,
                &opt_char.to_string(),
                Some(&value),
                (optind + 1) as i64,
            );
            return 0;
        }
        // ...or the next operand entirely...
        if optind < operands.len() {
            let value = operands[optind].clone();
            finish(
                shell,
                name,
                &opt_char.to_string(),
                Some(&value),
                (optind + 2) as i64,
            );
            return 0;
        }
        // ...or it's simply missing.
        let next = (optind + 1) as i64;
        if silent {
            finish(shell, name, ":", Some(&opt_char.to_string()), next);
        } else {
            let _ = writeln!(stderr, "{name}: option requires an argument -- {opt_char}");
            finish(shell, name, "?", None, next);
        }
        0
    }
}

/// Where scanning should resume after a *non-argument-taking* option (a
/// plain flag, or an unrecognized character) at `char_pos` within
/// `chars`: the same operand, one character further, if more remain
/// (bundled options like `-abc`); otherwise the next operand entirely.
/// Returns the `OPTIND` value to record either way.
fn advance_past_char(chars: &[char], char_pos: usize, optind: usize) -> i64 {
    if char_pos + 1 < chars.len() {
        optind as i64
    } else {
        (optind + 1) as i64
    }
}

/// Applies one `getopts` call's outcome: sets `name` and `OPTARG` (or
/// unsets the latter), records the new `OPTIND` both as the real shell
/// variable and as [`Shell::getopts_last_optind`] (so the *next* call can
/// tell whether the user changed `OPTIND` by hand in between), and
/// updates [`Shell::getopts_sub_index`] to match: `1` whenever `OPTIND`
/// itself advanced past the current operand (a fresh start on whatever
/// operand comes next), or one past the character just consumed when
/// `OPTIND` stayed put (still scanning the same bundled-option operand).
fn finish(shell: &mut Shell, name: &str, value: &str, optarg: Option<&str>, new_optind: i64) {
    shell.shell_vars.insert(name.to_string(), value.to_string());
    match optarg {
        Some(value) => {
            shell
                .shell_vars
                .insert("OPTARG".to_string(), value.to_string());
        }
        None => {
            shell.shell_vars.remove("OPTARG");
            shell.env_vars.remove("OPTARG");
        }
    }
    let previous_optind = shell
        .get_var("OPTIND")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1);
    shell
        .shell_vars
        .insert("OPTIND".to_string(), new_optind.to_string());
    shell.getopts_last_optind = Some(new_optind);
    shell.getopts_sub_index = if new_optind == previous_optind {
        shell.getopts_sub_index + 1
    } else {
        1
    };
}

/// The same rule every other name-taking builtin in this crate enforces
/// (`local`/`unset`/`read`) — duplicated for the same "five-line,
/// self-contained rule" reason those do.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(shell: &mut Shell, args: &[String]) -> (i32, String) {
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = Getopts.run(shell, args, &mut stdin, &mut stdout, &mut stderr);
        (status, String::from_utf8(stderr).unwrap())
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_simple_flags_one_call_at_a_time() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-a", "-b", "x"]);

        let (status, _) = run(&mut shell, &s(&["ab", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("a"));
        assert_eq!(shell.get_var("OPTIND"), Some("2"));

        let (status, _) = run(&mut shell, &s(&["ab", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("b"));
        assert_eq!(shell.get_var("OPTIND"), Some("3"));

        let (status, _) = run(&mut shell, &s(&["ab", "opt"]));
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("opt"), Some("?"));
        assert_eq!(shell.get_var("OPTIND"), Some("3"));
    }

    #[test]
    fn bundled_short_options_are_consumed_one_per_call_without_advancing_optind() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-ab"]);

        let (_, _) = run(&mut shell, &s(&["ab", "opt"]));
        assert_eq!(shell.get_var("opt"), Some("a"));
        assert_eq!(shell.get_var("OPTIND"), Some("1"));

        let (_, _) = run(&mut shell, &s(&["ab", "opt"]));
        assert_eq!(shell.get_var("opt"), Some("b"));
        assert_eq!(shell.get_var("OPTIND"), Some("2"));
    }

    #[test]
    fn option_with_argument_attached() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-ofile"]);
        let (status, _) = run(&mut shell, &s(&["o:", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("o"));
        assert_eq!(shell.get_var("OPTARG"), Some("file"));
        assert_eq!(shell.get_var("OPTIND"), Some("2"));
    }

    #[test]
    fn option_with_argument_as_separate_operand() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-o", "file"]);
        let (status, _) = run(&mut shell, &s(&["o:", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("o"));
        assert_eq!(shell.get_var("OPTARG"), Some("file"));
        assert_eq!(shell.get_var("OPTIND"), Some("3"));
    }

    #[test]
    fn invalid_option_default_mode_prints_and_sets_question_mark() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-z"]);
        let (status, stderr) = run(&mut shell, &s(&["a", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("?"));
        assert_eq!(shell.get_var("OPTARG"), None);
        assert!(stderr.contains("illegal option"));
    }

    #[test]
    fn invalid_option_silent_mode_sets_optarg_without_printing() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-z"]);
        let (status, stderr) = run(&mut shell, &s(&[":a", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("?"));
        assert_eq!(shell.get_var("OPTARG"), Some("z"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn missing_required_argument_default_mode() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-o"]);
        let (status, stderr) = run(&mut shell, &s(&["o:", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("?"));
        assert!(stderr.contains("requires an argument"));
    }

    #[test]
    fn missing_required_argument_silent_mode() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-o"]);
        let (status, stderr) = run(&mut shell, &s(&[":o:", "opt"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some(":"));
        assert_eq!(shell.get_var("OPTARG"), Some("o"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn double_dash_ends_options_and_advances_past_it() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-a", "--", "rest"]);
        run(&mut shell, &s(&["a", "opt"]));
        let (status, _) = run(&mut shell, &s(&["a", "opt"]));
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("OPTIND"), Some("3"));
    }

    #[test]
    fn a_non_option_operand_ends_options_without_consuming_it() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-a", "file"]);
        run(&mut shell, &s(&["a", "opt"]));
        let (status, _) = run(&mut shell, &s(&["a", "opt"]));
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("OPTIND"), Some("2"));
    }

    #[test]
    fn extra_operands_override_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = s(&["-b"]);
        let (status, _) = run(&mut shell, &s(&["a", "opt", "-a"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("opt"), Some("a"));
    }

    #[test]
    fn resetting_optind_by_hand_restarts_scanning() {
        // The realistic reset idiom: `OPTIND=1` issued *after* a getopts
        // loop has already finished consuming several operands (so
        // `OPTIND` is genuinely different from whatever value it's being
        // reset to) — see [`Shell::getopts_last_optind`]'s own docs for
        // the one narrower case (resetting to the *same* value `getopts`
        // already left it at, specifically while mid-bundled-option) this
        // doesn't attempt to distinguish from an ordinary continued scan.
        let mut shell = Shell::new();
        shell.positional_params = s(&["-a", "-b", "-c"]);
        run(&mut shell, &s(&["abc", "opt"])); // OPTIND -> 2
        run(&mut shell, &s(&["abc", "opt"])); // OPTIND -> 3
        shell
            .shell_vars
            .insert("OPTIND".to_string(), "1".to_string());
        let (_, _) = run(&mut shell, &s(&["abc", "opt"]));
        assert_eq!(shell.get_var("opt"), Some("a"));
    }
}
