//! `declare`/`typeset` (bash builtin, not POSIX) — deliberately narrow
//! scope, matching this phase's own scoping decision: conch's variable
//! model is a flat `HashMap<String, String>` with no integer/array-
//! attribute concept anywhere (`Shell::env_vars`/`shell_vars`), so `-i`
//! (integer — every subsequent assignment auto-evaluated as arithmetic)
//! and `-a`/`-A` (arrays — conch has no array support at all yet, a
//! project-wide gap, not specific to this builtin) are recognized but
//! explicitly reported as not-yet-supported rather than silently
//! accepted and ignored. `-x` (export-equivalent) and `-r` (readonly,
//! via the exact same [`Shell::readonly_vars`] the standalone `readonly`
//! builtin uses — see that type's own docs) are both real. `-f name`
//! prints a defined function's signature line (`name () `, matching
//! real bash's own first line of output) but not a faithful
//! reconstruction of its body — this crate's parser doesn't retain a
//! function definition's original source text (only a subshell's body
//! does, for an unrelated re-exec reason — see
//! `conch_shell_parser::SubshellBody`'s own docs), so a full,
//! byte-for-byte `declare -f` would need a genuine AST pretty-printer
//! this codebase doesn't have; flagged rather than fabricated.
//!
//! `declare` used *inside* a function call is function-local by default
//! (a well-known real bash behavior, distinct from plain `export`/a bare
//! assignment) — implemented here by routing through the exact same
//! capture/record/set-local mechanics the `local` builtin
//! (this crate's root module) already uses, so a `declare -x FOO=bar`
//! inside a function restores the caller's own `FOO` binding once that
//! call returns, exactly like `local FOO=bar` does.

use std::io::{Read, Write};

use conch_shell_core::{Builtin, Shell};

pub struct Declare;
impl Builtin for Declare {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let mut export = false;
        let mut readonly = false;
        let mut print_mode = false;
        let mut function_mode = false;
        let mut status = 0;
        let mut iter = args.iter().peekable();

        while let Some(arg) = iter.peek() {
            let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
                break;
            };
            for flag in flags.chars() {
                match flag {
                    'x' => export = true,
                    'r' => readonly = true,
                    'p' => print_mode = true,
                    'f' => function_mode = true,
                    'g' => {} // "global, not function-local" -- already this builtin's top-level-call behavior; only meaningful as a no-op distinction inside a function, not implemented (rare enough to skip).
                    'i' | 'a' | 'A' => {
                        let _ = writeln!(
                            stderr,
                            "declare: -{flag}: not yet supported (conch has no integer/array variable attributes)"
                        );
                        status = 1;
                    }
                    other => {
                        let _ = writeln!(stderr, "declare: -{other}: invalid option");
                        status = 2;
                    }
                }
            }
            iter.next();
        }

        let names: Vec<&String> = iter.collect();

        if function_mode {
            return print_functions(shell, &names, stdout, stderr).max(status);
        }

        if print_mode {
            return print_declared(shell, &names, stdout, stderr).max(status);
        }

        for name_arg in names {
            let (name, value) = match name_arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (name_arg.as_str(), None),
            };
            if !is_valid_identifier(name) {
                let _ = writeln!(stderr, "declare: `{name_arg}': not a valid identifier");
                status = 1;
                continue;
            }

            let became_locally_unset = shell.in_function_call() && value.is_none();
            if shell.in_function_call() {
                // Confirmed against real bash: `declare [-x] NAME` (no
                // `=value`) inside a function behaves exactly like a
                // bare `local NAME` — it becomes genuinely *unset* in
                // the new local scope (`${NAME+set}` is empty), not
                // merely left at whatever the caller's own binding was —
                // so this always shadows, unconditionally, the same way
                // `Local::run` (this crate's root module) always does,
                // never conditioned on whether a shadow already exists.
                let previous = shell.capture_var(name);
                shell.record_local(name.to_string(), previous);
                shell.set_local(name, value);
            } else if let Some(value) = value {
                shell.shell_vars.insert(name.to_string(), value);
            } else {
                shell.shell_vars.entry(name.to_string()).or_default();
            }

            // Skipped when this `declare -x NAME` just became locally
            // unset above: there's nothing to export, and promoting it
            // here would wrongly leave behind a spurious empty exported
            // entry instead of the genuinely-unset local shadow real
            // bash actually has (confirmed empirically, not assumed —
            // `${NAME+set}` is empty, and it never shows up in `export
            // -p`, for exactly this case).
            if export && !became_locally_unset {
                promote_to_exported(shell, name);
            }
            if readonly {
                shell.readonly_vars.insert(name.to_string());
            }
        }

        status
    }
}

/// `declare -f [name...]` — see this module's own docs for why only the
/// signature line is reproduced, not a faithful body.
fn print_functions(
    shell: &Shell,
    names: &[&String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let targets: Vec<&String> = if names.is_empty() {
        let mut all: Vec<&String> = shell.functions.keys().collect();
        all.sort();
        all
    } else {
        names.to_vec()
    };
    let mut status = 0;
    for name in targets {
        if shell.functions.contains_key(name.as_str()) {
            let _ = writeln!(stdout, "{name} () ");
        } else {
            let _ = writeln!(stderr, "declare: {name}: not found");
            status = 1;
        }
    }
    status
}

/// Bare `declare -x NAME` (no `=value`) promotes an existing binding to
/// exported, matching `export NAME`'s own bare form — shared logic, not
/// duplicated, since both do exactly the same thing to
/// `env_vars`/`shell_vars`.
fn promote_to_exported(shell: &mut Shell, name: &str) {
    if let Some(value) = shell.shell_vars.remove(name) {
        shell.env_vars.insert(name.to_string(), value);
    } else {
        shell.env_vars.entry(name.to_string()).or_default();
    }
}

/// `declare -p [name...]` — lists currently-set variables in a
/// `declare [-x] name="value"` shape (bash's own `-p` output style;
/// POSIX doesn't apply here at all since `declare` itself is a bash
/// extension). With no `name`s, lists every variable this shell knows
/// about; with names given, reports each one specifically (an unset one
/// is a per-name error, matching real bash, while the rest still print).
fn print_declared(
    shell: &Shell,
    names: &[&String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if names.is_empty() {
        let mut all: Vec<(&String, &String, bool)> = shell
            .env_vars
            .iter()
            .map(|(k, v)| (k, v, true))
            .chain(shell.shell_vars.iter().map(|(k, v)| (k, v, false)))
            .collect();
        all.sort_by_key(|(name, _, _)| name.as_str());
        for (name, value, exported) in all {
            print_one(name, value, exported, stdout);
        }
        return 0;
    }

    let mut status = 0;
    for name in names {
        match (
            shell.env_vars.get(name.as_str()),
            shell.shell_vars.get(name.as_str()),
        ) {
            (Some(value), _) => print_one(name, value, true, stdout),
            (None, Some(value)) => print_one(name, value, false, stdout),
            (None, None) => {
                let _ = writeln!(stderr, "declare: {name}: not found");
                status = 1;
            }
        }
    }
    status
}

/// `pub(crate)`, not `fn` — shared with `crate::lib`'s own `export -p`
/// (`Export::run`), which needs the identical `declare -x NAME="value"`
/// line shape for exported variables, filtered to *only* the exported
/// ones (unlike this function's own `declare -p` caller, which shows
/// shell-only variables too, with `--` instead of `-x`) — reusing this
/// one formatter rather than duplicating its exact quoting convention
/// keeps the two builtins' output from silently drifting apart.
pub(crate) fn print_one(name: &str, value: &str, exported: bool, stdout: &mut dyn Write) {
    let flag = if exported { "-x" } else { "--" };
    let _ = writeln!(stdout, "declare {flag} {name}=\"{value}\"");
}

/// The same rule every other name-taking builtin in this crate enforces
/// — duplicated for the same "five-line, self-contained rule" reason
/// those do.
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

    fn run(shell: &mut Shell, args: &[String]) -> (i32, String, String) {
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = Declare.run(shell, args, &mut stdin, &mut stdout, &mut stderr);
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
    fn plain_assignment_at_top_level() {
        let mut shell = Shell::new();
        let (status, _, _) = run(&mut shell, &s(&["X=1"]));
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("X"), Some("1"));
        assert!(!shell.env_vars.contains_key("X"));
    }

    #[test]
    fn dash_x_exports_the_variable() {
        let mut shell = Shell::new();
        run(&mut shell, &s(&["-x", "X=1"]));
        assert!(shell.env_vars.contains_key("X"));
        assert_eq!(shell.get_var("X"), Some("1"));
    }

    #[test]
    fn bare_dash_x_promotes_an_existing_variable() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("X".to_string(), "1".to_string());
        run(&mut shell, &s(&["-x", "X"]));
        assert!(shell.env_vars.contains_key("X"));
    }

    #[test]
    fn inside_a_function_call_is_local_by_default() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        run(&mut shell, &s(&["X=inner"]));
        assert_eq!(shell.get_var("X"), Some("inner"));
        shell.pop_local_frame();
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn bare_dash_x_inside_a_function_makes_the_name_genuinely_unset() {
        // Confirmed against real bash: `declare -x X` (no `=value`)
        // inside a function leaves `${X+set}` empty and never shows up
        // in `export -p` -- it does *not* promote the caller's existing
        // value to exported, unlike the same builtin at the top level.
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        run(&mut shell, &s(&["-x", "X"]));
        assert_eq!(shell.get_var("X"), None);
        assert!(!shell.env_vars.contains_key("X"));
        shell.pop_local_frame();
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn unsupported_flags_report_an_error_but_still_apply_the_supported_ones() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["-i", "-x", "X=1"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("not yet supported"));
        assert!(shell.env_vars.contains_key("X"));
    }

    #[test]
    fn dash_r_marks_a_variable_readonly() {
        let mut shell = Shell::new();
        run(&mut shell, &s(&["-r", "x=5"]));
        assert!(shell.readonly_vars.contains("x"));
        assert_eq!(shell.get_var("x"), Some("5"));
    }

    #[test]
    fn dash_f_prints_a_functions_signature_line() {
        let mut shell = Shell::new();
        shell.functions.insert("f".to_string(), dummy_function());
        let (status, stdout, _) = run(&mut shell, &s(&["-f", "f"]));
        assert_eq!(status, 0);
        assert_eq!(stdout, "f () \n");
    }

    #[test]
    fn dash_f_on_an_unknown_function_is_an_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["-f", "nope"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("not found"));
    }

    fn dummy_function() -> conch_shell_parser::FunctionDefinition {
        conch_shell_parser::FunctionDefinition {
            name: "f".to_string(),
            body: conch_shell_parser::CompoundCommand {
                kind: conch_shell_parser::CompoundCommandKind::BraceGroup(
                    conch_shell_parser::CommandList::default(),
                ),
                redirects: Vec::new(),
            },
        }
    }

    #[test]
    fn invalid_identifier_is_an_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["1x=2"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("not a valid identifier"));
    }

    #[test]
    fn dash_p_with_no_names_lists_every_variable() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("X".to_string(), "1".to_string());
        let (status, stdout, _) = run(&mut shell, &s(&["-p"]));
        assert_eq!(status, 0);
        assert!(stdout.contains("declare -- X=\"1\""));
    }

    #[test]
    fn dash_p_with_a_name_reports_it_specifically() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".to_string(), "1".to_string());
        let (status, stdout, _) = run(&mut shell, &s(&["-p", "X"]));
        assert_eq!(status, 0);
        assert_eq!(stdout.trim(), "declare -x X=\"1\"");
    }

    #[test]
    fn dash_p_with_an_unset_name_is_an_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&mut shell, &s(&["-p", "NEVER_SET"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("not found"));
    }
}
