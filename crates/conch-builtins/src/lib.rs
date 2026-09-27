//! Builtin commands: `cd`, `exit`, `export`, `echo`, `pwd` (Phase 1); the
//! `break`/`continue` POSIX special builtins (Phase 3); `local`,
//! `return`, a narrow `set --`, and `shift` (the functions/positional-
//! parameters follow-up); `jobs`/`fg`/`bg`/`wait`/`trap` (Phase 4 job
//! control); and `unset`/`umask`/`type`/`command` (Phase 5 builtins
//! completeness, mechanical batch — see this crate's own module for the
//! rest of Phase 5 as it lands).
//!
//! Every job-control builtin here is a thin wrapper over
//! `conch-shell-core`'s own job-table/signal-handling API
//! (`conch_shell_core::job`/`conch_shell_core::signals`) — this crate
//! deliberately has no direct `nix` dependency of its own (see e.g.
//! [`Shell::trap_set_by_name`]'s own docs for why), so every one of these
//! builtins works entirely in terms of job ids/PIDs-as-strings and
//! [`conch_shell_core::TrapAction`] (which has no OS-signal type in it at
//! all), never a raw `nix::sys::signal::Signal`.

use std::io::{Read, Write};

use conch_shell_core::{Builtin, ControlFlow, Shell, TrapAction};

mod alias_builtin;
mod declare_builtin;
mod exec_builtin;
mod getopts_builtin;
mod kill_builtin;
mod printf_builtin;
mod read_builtin;
mod test_builtin;
use alias_builtin::{Alias, Unalias};
use declare_builtin::Declare;
use exec_builtin::Exec;
use getopts_builtin::Getopts;
use kill_builtin::Kill;
use printf_builtin::Printf;
use read_builtin::ReadBuiltin;
use test_builtin::{Bracket, Test};

/// Registers every builtin this crate implements into `shell` — as either
/// a *regular* builtin ([`Shell::register_builtin`]) or a *special* one
/// ([`Shell::register_special_builtin`]), per POSIX 2.9.1/2.9.5's own
/// classification (`cd`/`echo`/`pwd`/`local` are ordinary utilities-ish
/// builtins; `break`/`continue`/`exit`/`export`/`return`/`set`/`shift`
/// are all on POSIX's special-builtin list). This distinction is
/// structural only — see [`Shell::special_builtins`]'s docs for why it's
/// *not* used to stop a same-named shell function from shadowing either
/// kind (confirmed against real bash's own default, non-`--posix`,
/// behavior).
pub fn register_all(shell: &mut Shell) {
    shell.register_builtin("cd", Box::new(Cd));
    shell.register_special_builtin("exit", Box::new(Exit));
    shell.register_special_builtin("export", Box::new(Export));
    shell.register_special_builtin("readonly", Box::new(Readonly));
    shell.register_builtin("echo", Box::new(Echo));
    shell.register_builtin("pwd", Box::new(Pwd));
    shell.register_special_builtin("break", Box::new(Break));
    shell.register_special_builtin("continue", Box::new(Continue));
    shell.register_builtin("local", Box::new(Local));
    shell.register_special_builtin("return", Box::new(Return));
    shell.register_special_builtin("set", Box::new(Set));
    shell.register_special_builtin("shift", Box::new(Shift));
    shell.register_builtin("jobs", Box::new(Jobs));
    shell.register_builtin("fg", Box::new(Fg));
    shell.register_builtin("bg", Box::new(Bg));
    shell.register_special_builtin("wait", Box::new(Wait));
    shell.register_special_builtin("trap", Box::new(Trap));
    shell.register_special_builtin("unset", Box::new(Unset));
    shell.register_builtin("umask", Box::new(Umask));
    shell.register_builtin("type", Box::new(Type));
    shell.register_builtin("command", Box::new(Command));
    shell.register_special_builtin("eval", Box::new(Eval));
    shell.register_special_builtin(".", Box::new(Source));
    shell.register_special_builtin("source", Box::new(Source));
    shell.register_builtin("test", Box::new(Test));
    shell.register_builtin("[", Box::new(Bracket));
    shell.register_builtin("read", Box::new(ReadBuiltin));
    shell.register_builtin("getopts", Box::new(Getopts));
    shell.register_builtin("printf", Box::new(Printf));
    shell.register_builtin("kill", Box::new(Kill));
    shell.register_builtin("declare", Box::new(Declare));
    shell.register_builtin("typeset", Box::new(Declare));
    shell.register_special_builtin("exec", Box::new(Exec));
    shell.register_builtin("alias", Box::new(Alias));
    shell.register_builtin("unalias", Box::new(Unalias));
}

struct Cd;
impl Builtin for Cd {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let target = match args.first() {
            Some(dir) => dir.clone(),
            None => match shell.get_var("HOME") {
                Some(home) => home.to_string(),
                None => {
                    let _ = writeln!(stderr, "cd: HOME not set");
                    return 1;
                }
            },
        };

        match std::env::set_current_dir(&target) {
            Ok(()) => {
                shell.cwd = match std::env::current_dir() {
                    Ok(cwd) => cwd,
                    Err(err) => {
                        let _ = writeln!(stderr, "cd: {err}");
                        return 1;
                    }
                };
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "cd: {target}: {err}");
                1
            }
        }
    }
}

struct Exit;
impl Builtin for Exit {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        // POSIX: `exit [n]` — if n is omitted, use the exit status of the
        // last command executed, not 0.
        let code = args
            .first()
            .and_then(|arg| arg.parse::<i32>().ok())
            .unwrap_or(shell.last_status);

        // POSIX: "the trap on EXIT shall be executed before the shell
        // terminates" -- every path that ends this process (this
        // builtin, falling off the end of a `-c`/script, or the
        // interactive loop ending) needs this same call; see
        // `Shell::run_exit_trap`'s own docs for why it's safe to call
        // even when nothing was ever trapped (a no-op) or when the trap
        // action itself calls `exit` again (already-taken, so it can't
        // recurse into itself).
        shell.run_exit_trap();

        // Unconditional: this always terminates the real process, no
        // signaling/indirection needed. That's correct even for `exit`
        // run inside a subshell — confirmed against real bash it only
        // terminates that subshell, not the parent — because
        // `conch-shell-core::exec`'s subshell execution runs the
        // subshell as a genuinely separate child process (re-execing
        // `conch -c <body>`), so *this* call, from inside that child,
        // naturally only ends the child. See that function's doc
        // comment for the full reasoning (and why an in-process
        // subshell design — an earlier candidate — couldn't make this
        // builtin correct no matter how it signaled).
        std::process::exit(code);
    }
}

/// `readonly [-p] [name[=value]...]` (POSIX 2.9.1 special builtin) — a
/// bare `name` (no `=value`) marks an *already-assigned* variable
/// readonly in place, leaving its current value untouched; `name=value`
/// assigns first, then marks it readonly. See [`Shell::readonly_vars`]'s
/// own docs for exactly where (and where not) this is enforced, and the
/// one confirmed-against-real-bash subtlety this only approximates
/// (aborting just the current *source line* vs. the current
/// *list-item-by-list-item* granularity this crate's parser actually
/// preserves).
struct Readonly;
impl Builtin for Readonly {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let args: Vec<&String> = args.iter().filter(|a| a.as_str() != "-p").collect();

        if args.is_empty() {
            let mut names: Vec<&String> = shell.readonly_vars.iter().collect();
            names.sort();
            for name in names {
                let value = shell.get_var(name).unwrap_or("");
                let _ = writeln!(stdout, "readonly {name}='{value}'");
            }
            return 0;
        }

        let mut status = 0;
        for arg in args {
            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            if !is_valid_identifier(name) {
                let _ = writeln!(stderr, "readonly: `{arg}': not a valid identifier");
                status = 1;
                continue;
            }
            if let Some(value) = value {
                if shell.env_vars.contains_key(name) {
                    shell.env_vars.insert(name.to_string(), value);
                } else {
                    shell.shell_vars.insert(name.to_string(), value);
                }
            }
            shell.readonly_vars.insert(name.to_string());
        }
        status
    }
}

struct Export;
impl Builtin for Export {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        for arg in args {
            match arg.split_once('=') {
                Some((name, value)) => {
                    shell.env_vars.insert(name.to_string(), value.to_string());
                }
                None => {
                    // Bare `export NAME` promotes an existing shell
                    // variable to an exported one.
                    if let Some(value) = shell.shell_vars.remove(arg) {
                        shell.env_vars.insert(arg.clone(), value);
                    } else {
                        shell.env_vars.entry(arg.clone()).or_default();
                    }
                }
            }
        }
        0
    }
}

struct Echo;
impl Builtin for Echo {
    fn run(
        &self,
        _shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let _ = writeln!(stdout, "{}", args.join(" "));
        0
    }
}

struct Pwd;
impl Builtin for Pwd {
    fn run(
        &self,
        shell: &mut Shell,
        _args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let _ = writeln!(stdout, "{}", shell.cwd.display());
        0
    }
}

/// `break [n]` / `continue [n]` (POSIX special builtins) share
/// everything except which [`ControlFlow`] variant they request — both
/// need the same `[n]` parsing (defaults to `1`; must be a positive
/// integer) and the same "only meaningful inside a loop" guard, matching
/// real bash's own wording for both.
fn run_loop_control(
    shell: &mut Shell,
    args: &[String],
    stderr: &mut dyn Write,
    name: &str,
    make: fn(u32) -> ControlFlow,
) -> i32 {
    let level = match args.first() {
        None => 1,
        Some(arg) => match arg.parse::<u32>() {
            // POSIX: n must be >= 1; bash additionally clamps an
            // out-of-range n down to the current nesting depth rather
            // than erroring (confirmed against real bash: `break 5` two
            // loops deep still breaks both, with the same "only
            // meaningful..." diagnostic for the remainder — see
            // conch-shell-core::exec's module docs) rather than treating
            // n itself as invalid, so only `0` and non-numeric/negative
            // text are rejected here.
            Ok(0) | Err(_) => {
                let _ = writeln!(stderr, "{name}: {arg}: loop count out of range");
                return 1;
            }
            Ok(n) => n,
        },
    };

    // Confirmed against real bash: `break`/`continue` outside any
    // enclosing loop is a harmless no-op (script execution continues
    // normally afterward) that only prints a diagnostic — it must *not*
    // set a pending signal a later, unrelated loop could mistakenly
    // catch.
    if shell.loop_depth == 0 {
        let _ = writeln!(
            stderr,
            "{name}: only meaningful in a `for', `while', or `until' loop"
        );
        return 0;
    }

    shell.pending_control_flow = Some(make(level));
    0
}

struct Break;
impl Builtin for Break {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        run_loop_control(shell, args, stderr, "break", ControlFlow::Break)
    }
}

struct Continue;
impl Builtin for Continue {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        run_loop_control(shell, args, stderr, "continue", ControlFlow::Continue)
    }
}

/// `local [NAME[=VALUE]]...` (bash extension — not POSIX). Only valid
/// inside a function call (confirmed against real bash: `local: can only
/// be used in a function`, `$?` = 1, and — confirmed separately — no
/// partial effect at all in that case, not even for a bare `local` with
/// zero arguments).
///
/// A single `local` invocation still processes every argument even after
/// one is rejected as an invalid name (confirmed against real bash:
/// `local 1x=2 y=3` still declares `y`, with `$?` = 1 only because of the
/// earlier, unrelated failure) — this doesn't `return` early on that
/// error, just records it and keeps going.
struct Local;
impl Builtin for Local {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if !shell.in_function_call() {
            let _ = writeln!(stderr, "local: can only be used in a function");
            return 1;
        }

        let mut status = 0;
        for arg in args {
            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            if !is_valid_identifier(name) {
                let _ = writeln!(stderr, "local: `{arg}': not a valid identifier");
                status = 1;
                continue;
            }
            let previous = shell.capture_var(name);
            shell.record_local(name.to_string(), previous);
            shell.set_local(name, value);
        }
        status
    }
}

/// The same `NAME` rule `conch-shell-parser` enforces for a function
/// definition/`for` loop variable (`[A-Za-z_][A-Za-z0-9_]*`) — duplicated
/// here (rather than depending on `conch-shell-parser` just for this)
/// since it's a five-line, self-contained rule and `conch-shell-builtins`
/// otherwise has no reason to depend on the parser crate at all.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `return [n]` (POSIX special builtin). Valid inside a function call
/// *or* a `.`/`source` invocation ([`Shell::in_return_scope`]) —
/// confirmed against real bash this is a genuine error otherwise (unlike
/// `break`/`continue` outside a loop, which silently no-op): `return: can
/// only `return' from a function or sourced script`, `$?` = 2, and
/// nothing after it in the same list runs (matching an ordinary command
/// failure, not a loop-control no-op).
struct Return;
impl Builtin for Return {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if !shell.in_return_scope() {
            let _ = writeln!(
                stderr,
                "return: can only `return' from a function or sourced script"
            );
            return 2;
        }

        // POSIX: `n` omitted -> the exit status of the last command
        // executed (same convention `exit`, conch-shell-builtins, already
        // follows). Confirmed against real bash a non-numeric `n` is a
        // real error ("numeric argument required", `$?` = 2) that still
        // stops the function immediately, exactly like a valid `return`
        // does -- it doesn't fall through and keep running the rest of
        // the function body.
        let code = match args.first() {
            None => shell.last_status,
            Some(arg) => match arg.parse::<i32>() {
                Ok(code) => code,
                Err(_) => {
                    let _ = writeln!(stderr, "return: {arg}: numeric argument required");
                    shell.pending_control_flow = Some(ControlFlow::Return(2));
                    return 2;
                }
            },
        };
        // Confirmed against real bash `return`'s argument wraps into an
        // unsigned byte immediately (observable via `$?` inside the very
        // same shell, not just the final process exit code) -- `return
        // 300` leaves `$?` at 44, `return -1` leaves it at 255 -- the
        // same `rem_euclid(256)` conch's binary crate's `main` already
        // uses for the whole *process's* own final exit code, applied
        // here instead so it's correct for an in-shell `$?` observation
        // too, not just a `-c`/script-file invocation's process exit.
        shell.pending_control_flow = Some(ControlFlow::Return(code.rem_euclid(256)));
        0
    }
}

/// `set [-e|+e] [-u|+u] [-x|+x] [-o name|+o name] [-- [arg...]]` /
/// bare `set` (POSIX special builtin). Positional-parameter replacement
/// (`set -- [arg...]`, including a bare `set --` clearing them) and the
/// three flags with real execution-engine behavior (`Shell::errexit`/
/// `Shell::nounset`/`Shell::xtrace`, all enforced in
/// `conch-shell-core::exec`/`::expand` — see each field's own docs) are
/// implemented; combined short flags (`set -eu`) work the same way
/// `read`'s option parsing does. Bare `set` (no arguments at all) lists
/// every currently-set variable as `name=value`, sorted, matching
/// POSIX's own default behavior. Every other flag (`-o` names besides
/// the three above, `-C`/`-n`/`-v`/...) is reported clearly rather than
/// silently accepted and ignored.
struct Set;
impl Builtin for Set {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.is_empty() {
            let mut names: Vec<&String> = shell
                .env_vars
                .keys()
                .chain(shell.shell_vars.keys())
                .collect();
            names.sort();
            for name in names {
                let value = shell.get_var(name).unwrap_or("");
                let _ = writeln!(stdout, "{name}={value}");
            }
            return 0;
        }

        let mut status = 0;
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.peek().copied() {
            if arg == "--" {
                iter.next();
                break;
            }
            let Some((sign, flags)) = arg
                .strip_prefix('-')
                .map(|f| (true, f))
                .or_else(|| arg.strip_prefix('+').map(|f| (false, f)))
            else {
                break;
            };
            if flags.is_empty() {
                break;
            }
            iter.next();
            for flag in flags.chars() {
                match flag {
                    'e' => shell.errexit = sign,
                    'u' => shell.nounset = sign,
                    'x' => shell.xtrace = sign,
                    'f' => shell.noglob = sign,
                    'C' => shell.noclobber = sign,
                    'o' => {
                        let Some(name) = iter.next() else {
                            let _ = writeln!(stderr, "set: -o: option requires an argument");
                            return 2;
                        };
                        match name.as_str() {
                            "errexit" => shell.errexit = sign,
                            "nounset" => shell.nounset = sign,
                            "xtrace" => shell.xtrace = sign,
                            "noglob" => shell.noglob = sign,
                            "noclobber" => shell.noclobber = sign,
                            other => {
                                let _ = writeln!(
                                    stderr,
                                    "set: -o: {other}: not yet supported (only errexit/nounset/xtrace/noglob/noclobber)"
                                );
                                status = 1;
                            }
                        }
                    }
                    other => {
                        let _ = writeln!(stderr, "set: -{other}: not yet supported");
                        status = 1;
                    }
                }
            }
        }

        let rest: Vec<String> = iter.cloned().collect();
        if !rest.is_empty() || args.last().map(String::as_str) == Some("--") {
            shell.positional_params = rest;
        }
        status
    }
}

/// `shift [n]` (POSIX special builtin) — removes the first `n` (default
/// `1`) positional parameters. Confirmed against real bash: `n` greater
/// than `$#` is an all-or-nothing error (`$?` = 1, positional parameters
/// left completely unchanged), as is a negative `n` (with an explicit
/// "shift count out of range" diagnostic in that case specifically,
/// unlike the silent `n > $#` case — this always prints one, which is
/// harmless either way since `tests/conch-difftest` only compares stderr
/// wording when a case opts into it); `shift 0` is a valid no-op.
struct Shift;
impl Builtin for Shift {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let n = match args.first() {
            None => 1,
            Some(arg) => match arg.parse::<i64>() {
                Ok(n) => n,
                Err(_) => {
                    let _ = writeln!(stderr, "shift: {arg}: numeric argument required");
                    return 1;
                }
            },
        };
        let out_of_range =
            n < 0 || usize::try_from(n).is_ok_and(|n| n > shell.positional_params.len());
        if out_of_range {
            let _ = writeln!(stderr, "shift: shift count out of range");
            return 1;
        }
        // `n` is already confirmed in `0..=positional_params.len()`.
        shell.positional_params.drain(0..n as usize);
        0
    }
}

// ---- Phase 5: unset / umask / type / command ------------------------------

/// The exact reserved-word set `conch-shell-parser`'s own (private)
/// `reserved_word` function recognizes — duplicated here (rather than
/// depending on that crate just for this) for the same "five-line,
/// self-contained rule" reason [`is_valid_identifier`] already is: `type`
/// is the one builtin that needs to report a name as "a shell keyword"
/// the way real bash does (`type if` -> "if is a shell keyword"), which
/// [`conch_shell_core::resolve_command`]'s function/builtin/`PATH` order
/// has no notion of at all (a keyword is neither).
const RESERVED_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "{", "}", "function",
];

/// `unset [-fv] [name...]` (POSIX special builtin) — without `-f`/`-v`,
/// tries a variable first and only falls back to a function of the same
/// name if no such variable existed (POSIX permits either priority;
/// matches real bash's own default). `-f`: only ever unsets a function;
/// `-v`: only ever unsets a variable. Unsetting a name that was never set
/// at all is a silent success (POSIX), not an error — only an invalid
/// identifier is.
///
/// Readonly enforcement (POSIX: unsetting a readonly variable is an
/// error) isn't implemented yet — there's no readonly bookkeeping in
/// `Shell` at all until `declare -r`/`readonly` land later in this same
/// phase; this will start enforcing it once that bookkeeping exists
/// rather than needing a second pass over this builtin.
struct Unset;
impl Builtin for Unset {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let mut only_functions = false;
        let mut only_variables = false;
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.peek() {
            match arg.as_str() {
                "-f" => {
                    only_functions = true;
                    iter.next();
                }
                "-v" => {
                    only_variables = true;
                    iter.next();
                }
                "--" => {
                    iter.next();
                    break;
                }
                _ => break,
            }
        }

        let mut status = 0;
        for name in iter {
            if !is_valid_identifier(name) {
                let _ = writeln!(stderr, "unset: `{name}': not a valid identifier");
                status = 1;
                continue;
            }
            if !only_functions && shell.readonly_vars.contains(name) {
                // POSIX: unsetting a readonly variable is an error.
                // Non-fatal, matching real bash's own default over
                // dash's stricter one -- see `Shell::readonly_vars`'s
                // own docs for the identical policy already applied to
                // an ordinary readonly reassignment.
                let _ = writeln!(stderr, "unset: {name}: cannot unset: readonly variable");
                status = 1;
                continue;
            }
            if only_functions {
                shell.functions.remove(name);
                continue;
            }
            let had_var =
                shell.env_vars.remove(name).is_some() || shell.shell_vars.remove(name).is_some();
            if !had_var && !only_variables {
                shell.functions.remove(name);
            }
        }
        status
    }
}

/// `umask [-S] [mode]` — reports or sets the process umask
/// ([`conch_shell_core::current_umask`]/[`conch_shell_core::set_umask`]).
/// `mode` must be an octal number (`022`, `0022`) — bash's `chmod`-style
/// symbolic mode (`u+w`, `a=rx`, ...) for *setting* a new mask isn't
/// implemented (a real parser of its own, out of scope for this pass) and
/// is reported clearly rather than silently ignored; `-S`'s *display*
/// form (the symbolic rendering of the *current* mask) is implemented,
/// since that direction is a simple bit-to-letter mapping, not a parser.
struct Umask;
impl Builtin for Umask {
    fn run(
        &self,
        _shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let symbolic = args.iter().any(|arg| arg == "-S");
        let mode_arg = args.iter().find(|arg| !arg.starts_with('-'));

        let Some(mode) = mode_arg else {
            let mask = conch_shell_core::current_umask();
            if symbolic {
                let _ = writeln!(stdout, "{}", format_symbolic_umask(mask));
            } else {
                let _ = writeln!(stdout, "{mask:04o}");
            }
            return 0;
        };

        match u32::from_str_radix(mode, 8) {
            Ok(mask) if mask <= 0o777 => {
                conch_shell_core::set_umask(mask);
                0
            }
            _ => {
                let _ = writeln!(
                    stderr,
                    "umask: {mode}: octal number out of range (symbolic mode is not yet supported)"
                );
                1
            }
        }
    }
}

/// `u=rwx,g=rx,o=r`-style rendering of `mask`'s *permitted* bits (the
/// complement of the mask itself, per POSIX's own definition of what a
/// umask means) — `umask -S`'s display form.
fn format_symbolic_umask(mask: u32) -> String {
    let permitted = !mask & 0o777;
    let category = |shift: u32| {
        let bits = (permitted >> shift) & 0o7;
        let mut s = String::new();
        if bits & 0o4 != 0 {
            s.push('r');
        }
        if bits & 0o2 != 0 {
            s.push('w');
        }
        if bits & 0o1 != 0 {
            s.push('x');
        }
        s
    };
    format!("u={},g={},o={}", category(6), category(3), category(0))
}

/// `type [-t] [-p] name...` (bash builtin) — reports what each `name`
/// resolves to, per the same function → builtin → `PATH` order
/// [`conch_shell_core::resolve_command`] gives an actual command lookup,
/// plus the one thing that order has no notion of: a shell reserved word
/// ([`RESERVED_WORDS`]). `-t`: print only the one-word category
/// (`function`/`builtin`/`file`/`keyword`); `-p`: print only the resolved
/// `PATH` entry for an external command (nothing at all for any other
/// category, matching real bash). `-a` (list *every* match, not just the
/// first) is accepted but not distinguished from the default — a
/// documented gap, since conch has no alias table yet and every other
/// category already has at most one resolution.
struct Type;
impl Builtin for Type {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let mut type_only = false;
        let mut path_only = false;
        let mut names = Vec::new();
        for arg in args {
            match arg.as_str() {
                "-t" => type_only = true,
                "-p" => path_only = true,
                "-a" => {}
                _ => names.push(arg.clone()),
            }
        }
        if names.is_empty() {
            let _ = writeln!(stderr, "type: usage: type name [name ...]");
            return 2;
        }

        let mut status = 0;
        for name in &names {
            if RESERVED_WORDS.contains(&name.as_str()) {
                if path_only {
                    continue;
                }
                if type_only {
                    let _ = writeln!(stdout, "keyword");
                } else {
                    let _ = writeln!(stdout, "{name} is a shell keyword");
                }
                continue;
            }
            match conch_shell_core::resolve_command(shell, name) {
                conch_shell_core::CommandResolution::Function => {
                    if path_only {
                        continue;
                    }
                    if type_only {
                        let _ = writeln!(stdout, "function");
                    } else {
                        let _ = writeln!(stdout, "{name} is a function");
                    }
                }
                conch_shell_core::CommandResolution::Builtin => {
                    if path_only {
                        continue;
                    }
                    if type_only {
                        let _ = writeln!(stdout, "builtin");
                    } else {
                        let _ = writeln!(stdout, "{name} is a shell builtin");
                    }
                }
                conch_shell_core::CommandResolution::External(path) => {
                    if type_only {
                        let _ = writeln!(stdout, "file");
                    } else {
                        let _ = writeln!(stdout, "{name} is {path}");
                    }
                }
                conch_shell_core::CommandResolution::NotFound => {
                    if !path_only {
                        let _ = writeln!(stderr, "type: {name}: not found");
                    }
                    status = 1;
                }
            }
        }
        status
    }
}

/// `command [-v|-V] [-p] name [arg...]` — POSIX 2.9.1.1: runs `name`
/// bypassing *function* lookup only (a regular builtin is still a
/// builtin — `command cd /tmp` still runs the `cd` builtin, matching
/// real bash). `-v`/`-V` report what `name` would resolve to instead of
/// running it (`-v`: bare path/name, matching `type -p`'s style but for
/// every category, not just external commands; `-V`: `type`'s own
/// human-readable sentences) rather than running it at all.
///
/// Known simplification for the bare (no `-v`/`-V`) execution form, when
/// `name` resolves to an *external* command specifically: it's spawned
/// with real inherited stdio (`Stdio::inherit()`) rather than through
/// this crate's usual nested stdin/stdout-buffer plumbing — correct for
/// the common case (`command ls`, `command cat file`) but means a
/// redirect/pipe on the *outer* `command` invocation itself
/// (`command ls > file`) doesn't compose the way it does for a direct
/// `ls > file` — the same category of documented gap
/// `conch-shell-core::exec`'s own module docs already carry for a
/// function call's combined output (`exec_function`'s docs). A *builtin*
/// target doesn't have this gap: it's called through the exact same
/// `stdin`/`stdout`/`stderr` this builtin itself received, so nesting
/// composes correctly there.
struct Command;
impl Builtin for Command {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let mut mode = CommandMode::Run;
        let mut rest = args;
        loop {
            match rest.first().map(String::as_str) {
                Some("-v") => {
                    mode = CommandMode::PrintPath;
                    rest = &rest[1..];
                }
                Some("-V") => {
                    mode = CommandMode::Describe;
                    rest = &rest[1..];
                }
                Some("-p") => {
                    // Accepted, but see find_in_path -- always searches
                    // the shell's current $PATH, not a fixed default one.
                    rest = &rest[1..];
                }
                _ => break,
            }
        }
        let Some((name, call_args)) = rest.split_first() else {
            let _ = writeln!(stderr, "command: usage: command [-v|-V] [-p] name [arg...]");
            return 2;
        };

        match mode {
            CommandMode::PrintPath => match conch_shell_core::resolve_command(shell, name) {
                conch_shell_core::CommandResolution::Function
                | conch_shell_core::CommandResolution::Builtin => {
                    let _ = writeln!(stdout, "{name}");
                    0
                }
                conch_shell_core::CommandResolution::External(path) => {
                    let _ = writeln!(stdout, "{path}");
                    0
                }
                conch_shell_core::CommandResolution::NotFound => 1,
            },
            CommandMode::Describe => match conch_shell_core::resolve_command(shell, name) {
                conch_shell_core::CommandResolution::Function => {
                    let _ = writeln!(stdout, "{name} is a function");
                    0
                }
                conch_shell_core::CommandResolution::Builtin => {
                    let _ = writeln!(stdout, "{name} is a shell builtin");
                    0
                }
                conch_shell_core::CommandResolution::External(path) => {
                    let _ = writeln!(stdout, "{name} is {path}");
                    0
                }
                conch_shell_core::CommandResolution::NotFound => {
                    let _ = writeln!(stderr, "command: {name}: not found");
                    1
                }
            },
            CommandMode::Run => {
                run_command_bypassing_functions(shell, name, call_args, stdin, stdout, stderr)
            }
        }
    }
}

enum CommandMode {
    Run,
    PrintPath,
    Describe,
}

/// `command name [arg...]`'s bare execution form — see [`Command`]'s own
/// docs for what "bypassing functions" means and the external-command
/// stdio simplification.
fn run_command_bypassing_functions(
    shell: &mut Shell,
    name: &str,
    args: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if shell.builtin(name).is_some() {
        let builtin = shell
            .take_builtin(name)
            .expect("just checked this builtin exists");
        let status = builtin.run(shell, args, stdin, stdout, stderr);
        shell.register_builtin(name.to_string(), builtin);
        return status;
    }

    match conch_shell_core::find_in_path(shell, name) {
        Some(_path) => {
            let status = std::process::Command::new(name)
                .args(args)
                .current_dir(&shell.cwd)
                .env_clear()
                .envs(&shell.env_vars)
                .status();
            match status {
                Ok(status) => status.code().unwrap_or(127),
                Err(err) => {
                    let _ = writeln!(stderr, "command: {name}: {err}");
                    127
                }
            }
        }
        None => {
            let _ = writeln!(stderr, "command: {name}: not found");
            127
        }
    }
}

// ---- eval / . (source) -----------------------------------------------------

/// `eval [argument...]` (POSIX special builtin) — joins every argument
/// with a single space (POSIX 2.14: "as if it were the input to `sh
/// -c`"), then parses and runs the result *directly against the current
/// `Shell`* — unlike every other "run some parsed text" path in this
/// codebase (a subshell, command substitution, and a backgrounded list
/// all re-exec `conch -c <text>` as a genuine child process — see
/// `conch-shell-core::exec`'s module docs for why *those* specifically
/// need real process isolation), `eval`'s text must NOT be isolated: a
/// variable assignment or `cd` inside `eval "..."` has to persist in the
/// calling shell exactly as if that text had been typed there directly
/// (POSIX 2.14's own wording: "in the current shell environment").
///
/// Critically, `eval` introduces **no execution boundary whatsoever** —
/// confirmed against real bash: `eval break` inside a loop breaks that
/// *enclosing* loop, and `eval X=1` inside a function mutates whatever
/// `X` binding is currently live (an enclosing `local`, if any, exactly
/// like a plain `X=1` typed at that same spot would). That's exactly why
/// this calls [`conch_shell_core::exec_command_list`] directly rather
/// than [`conch_shell_core::exec_program`] (an earlier version of this
/// builtin did use `exec_program`, which is wrong here: it *also*
/// unconditionally discards-and-warns on any `break`/`continue`/`return`
/// still pending afterward and clears `foreground_interrupt` — exactly
/// the top-level-program-only backstop behavior that must *not* apply to
/// `eval`, which needs a pending signal to survive completely untouched
/// so the *real* enclosing loop/function-call boundary, wherever it
/// actually is, still gets to consume it) — no positional-parameter
/// swap, no `loop_depth` reset, no fresh `local_stack` frame, nothing
/// `exec_function_call`/`.`/a subshell each do for their own, genuine
/// boundaries.
///
/// A syntax error in the eval'd text is a genuine shell error (POSIX),
/// not silently swallowed — reported to stderr and `$?` = 2 — but
/// deliberately *not* fatal to a non-interactive shell (confirmed against
/// real bash, as distinct from dash, which does abort: POSIX permits but
/// does not require aborting on a special builtin's error, and this
/// project tracks bash's own default over dash's stricter one, matching
/// the established "special-builtin-shadowing"/"bash arithmetic
/// extensions" precedent elsewhere of preferring bash's actual behavior
/// over merely-POSIX-permitted stricter alternatives).
///
/// Zero arguments (`eval` alone, or every argument expanding to nothing)
/// is a no-op, exit status `0` (POSIX).
///
/// [`Shell::eval_depth`] guards against a self-referential `eval` (`x='eval
/// $x'; eval "$x"`) recursing through real Rust stack frames with no
/// natural bound — see that field's own docs.
///
/// Known simplification, shared with a function call's own combined
/// output (`conch-shell-core::exec::exec_function`'s own docs): a
/// builtin/external command nested inside the eval'd text writes to the
/// *real* inherited stdout, not through this builtin's own captured
/// `stdout` buffer — so `eval 'echo hi' > file` doesn't redirect `hi`
/// into `file` the way a direct `echo hi > file` would. Fixing this needs
/// the same "genuine OS-level fd redirection around an arbitrarily deep
/// nested execution" machinery that gap is *already* waiting on, so it's
/// not attempted separately here.
struct Eval;

/// See [`Shell::eval_depth`]'s own docs — mirrors
/// `conch_shell_core::expand`'s `MAX_ARITH_RECURSION` precedent exactly;
/// the specific bound doesn't need to match bash's own, only to turn
/// runaway self-referential recursion into a clean error instead of a
/// stack overflow.
const MAX_EVAL_RECURSION: u32 = 200;

impl Builtin for Eval {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.is_empty() {
            return 0;
        }
        if shell.eval_depth >= MAX_EVAL_RECURSION {
            let _ = writeln!(stderr, "eval: maximum eval nesting depth exceeded");
            return 1;
        }
        let source = args.join(" ");
        let list = match conch_shell_parser::parse(&source) {
            Ok(list) => list,
            Err(err) => {
                let _ = writeln!(stderr, "eval: {err}");
                return 2;
            }
        };
        shell.eval_depth += 1;
        let status = conch_shell_core::exec_command_list(&list, shell);
        shell.eval_depth -= 1;
        status
    }
}

/// `. file [argument...]` (POSIX special builtin; `source` is the bash
/// synonym, both registered against this same implementation) — reads
/// `file` and runs its contents directly against the *current* `Shell`,
/// for the identical "no process isolation, must persist in the calling
/// context" reason [`Eval`] does (see that type's own docs); `file`'s
/// content is read from disk rather than taken as an argument, and (per
/// POSIX/real bash) any `argument`s beyond the first temporarily replace
/// the positional parameters for the duration, mirroring
/// `exec_function_call`'s own save/replace/restore of
/// [`Shell::positional_params`] — restored via an `assignments`-shaped
/// exit-scope pattern here, not by reusing that function directly, since
/// a sourced script is *not* a function call in every other respect
/// (see [`Shell::source_depth`]'s own docs for the one respect that
/// matters most: it does *not* reset [`Shell::loop_depth`], so a `break`
/// inside a sourced file correctly escapes to whatever loop already
/// enclosed the `. file` call itself, exactly as if the file's text had
/// been pasted in place).
///
/// A bare `return [n]` inside the sourced file is valid (POSIX) and ends
/// the sourced file right there (not the calling script) with status
/// `n` — [`Shell::source_depth`] is what makes [`Shell::in_return_scope`]
/// true for the duration so the `return` builtin doesn't reject it, and
/// this builtin is what actually consumes the resulting
/// [`ControlFlow::Return`] once `exec_command_list` returns, converting
/// it into this call's own status exactly like `exec_function_call`
/// does for a function body. A `break`/`continue` left pending instead is
/// deliberately *not* touched here — see the loop_depth paragraph above —
/// it stays pending for whatever loop already encloses the `. file` call
/// itself to consume.
///
/// `file` is searched for on `PATH` if it contains no `/` (POSIX,
/// read-only — unlike an ordinary command lookup, execute permission is
/// not required), falling back to the current directory if `PATH` search
/// finds nothing — bash's own real (non-`--posix`) default, which this
/// project tracks over strict POSIX-only where the two diverge (POSIX
/// itself deliberately omits that fallback, citing trojan-horse risk —
/// see this builtin's own PATH-search helper for the citation) — matching
/// this codebase's established precedent elsewhere (e.g. the arithmetic
/// module's own docs) for the same policy. A file that still isn't found,
/// or isn't readable, is a genuine shell error (`$?` = 1) — matching real
/// bash, not a silent no-op.
struct Source;
impl Builtin for Source {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let Some(path_arg) = args.first() else {
            let _ = writeln!(stderr, ".: filename argument required");
            return 2;
        };

        let (resolved, source) = match read_source_file(shell, path_arg) {
            Ok(pair) => pair,
            Err(err) => {
                let _ = writeln!(stderr, ".: {path_arg}: {err}");
                return 1;
            }
        };
        let list = match conch_shell_parser::parse(&source) {
            Ok(list) => list,
            Err(err) => {
                let _ = writeln!(stderr, ".: {resolved}: {err}");
                return 2;
            }
        };

        // POSIX/real bash: extra operands become the sourced script's own
        // positional parameters for its duration; with none, the caller's
        // own positional parameters (and `$0`) are left completely
        // untouched (confirmed against real bash: `. file` with no
        // extra arguments still sees the *caller's* `$1`/`$@`/`$#`).
        let previous_positional = (args.len() > 1)
            .then(|| std::mem::replace(&mut shell.positional_params, args[1..].to_vec()));

        shell.source_depth += 1;
        let status = conch_shell_core::exec_command_list(&list, shell);
        // See this type's own docs: a pending `Return` is this call's own
        // boundary to consume; a pending `Break`/`Continue` is
        // deliberately left untouched for the caller's own enclosing loop.
        let status = if let Some(ControlFlow::Return(code)) = shell.pending_control_flow {
            shell.pending_control_flow = None;
            code
        } else {
            status
        };
        shell.source_depth -= 1;

        if let Some(previous) = previous_positional {
            shell.positional_params = previous;
        }
        status
    }
}

/// `.`/`source`'s own `PATH`-search-and-read (POSIX 2.14) — deliberately
/// separate from [`conch_shell_core::find_in_path`] (used by `type`/
/// `command`): that one requires an *executable* match (ordinary command
/// lookup) and only ever reports a path, never reads it; this one only
/// ever requires a *readable* regular file, and falls back to the
/// current directory if `PATH` search finds nothing at all (bash's real,
/// non-`--posix` default — POSIX itself deliberately omits this
/// fallback: "Some older implementations searched the current directory
/// for the file... This behavior was omitted... due to concerns about
/// introducing the susceptibility to trojan horses" — confirmed this
/// project tracks bash's own default here via a direct empirical check
/// against real bash, not just the manual's own wording, since the two
/// read slightly differently in isolation).
///
/// Deliberately combines the search *and* the read into one
/// `std::fs::read_to_string` attempt per candidate, rather than a
/// separate "does this exist" probe followed by a later, distinct open —
/// caught by a security review: a check-then-open split has a real
/// TOCTOU window (the file could be replaced, e.g. via a symlink swap,
/// between the check and the later open), matching the same
/// check-and-use-must-be-one-syscall pattern already followed everywhere
/// else in this codebase that resolves a path before using it.
fn read_source_file(shell: &Shell, name: &str) -> Result<(String, String), std::io::Error> {
    if name.contains('/') {
        return std::fs::read_to_string(name).map(|contents| (name.to_string(), contents));
    }
    let path_var = shell.get_var("PATH").unwrap_or("");
    for dir in path_var.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = std::path::Path::new(dir).join(name);
        if let Ok(contents) = std::fs::read_to_string(&candidate) {
            return Ok((candidate.to_string_lossy().into_owned(), contents));
        }
    }
    std::fs::read_to_string(name).map(|contents| (name.to_string(), contents))
}

// ---- job control: jobs / fg / bg / wait / trap ----------------------------

/// `jobs [-p] [job_spec...]` — lists background/stopped jobs.
/// `job_spec` arguments (beyond `-p`) are accepted but not (yet) used to
/// filter the listing — every currently tracked job is always shown,
/// matching bare `jobs`' own behavior and a harmless superset of a
/// filtered listing's.
///
/// After listing, purges every job that was *already* finished *and*
/// already reported once before (see
/// [`Shell::purge_finished_notified_jobs`]) — the same "show a completed
/// job's status exactly once, then forget it" policy real bash follows,
/// applied here since `jobs` is one of the two places (the other: the
/// interactive prompt loop's own per-prompt notification pass, `conch`)
/// that "reports" a job's state at all.
struct Jobs;
impl Builtin for Jobs {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        _stderr: &mut dyn Write,
    ) -> i32 {
        let pids_only = args.iter().any(|arg| arg == "-p");
        let mut lines = Vec::new();
        for job in shell.job_table.iter() {
            if pids_only {
                lines.push(job.leader.to_string());
            } else {
                lines.push(format!(
                    "[{}]  {:<11} {}",
                    job.id,
                    job.state.label(),
                    job.command
                ));
            }
            // `jobs` itself is what marks a *currently* `Running`/
            // `Stopped` job's state as "already reported" — a completed
            // job it prints here is exactly the one report real bash
            // guarantees before forgetting it.
        }
        for line in &lines {
            let _ = writeln!(stdout, "{line}");
        }
        // Every job just listed above has now had its current state
        // reported once — mark it `notified` so a `Done`/`Signaled` one
        // gets purged below (and a `Running`/`Stopped` one simply
        // doesn't get re-announced by the interactive prompt loop's own
        // notification pass for the *same* state again).
        let ids: Vec<u32> = shell.job_table.iter().map(|job| job.id).collect();
        for id in ids {
            if let Some(job) = shell.job_table.get_mut(id) {
                job.notified = true;
            }
        }
        shell.purge_finished_notified_jobs();
        0
    }
}

/// Resolves a `fg`/`bg`-style optional job-spec argument, reporting
/// real bash's own "no job control" error first when appropriate (both
/// builtins need this exact same guard-then-resolve sequence).
fn resolve_fg_bg_job(
    shell: &Shell,
    args: &[String],
    name: &str,
    stderr: &mut dyn Write,
) -> Result<u32, i32> {
    if !shell.job_control_active {
        let _ = writeln!(stderr, "{name}: no job control in this shell");
        return Err(1);
    }
    shell
        .job_table
        .resolve_spec(args.first().map(String::as_str))
        .map_err(|err| {
            let _ = writeln!(stderr, "{name}: {err}");
            1
        })
}

/// `fg [job_spec]` — resumes a stopped-or-backgrounded job in the
/// foreground (terminal ownership, blocking wait, stoppable via Ctrl-Z
/// again) — see [`conch_shell_core::resume_job_in_foreground`].
struct Fg;
impl Builtin for Fg {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let id = match resolve_fg_bg_job(shell, args, "fg", stderr) {
            Ok(id) => id,
            Err(status) => return status,
        };
        match conch_shell_core::resume_job_in_foreground(shell, id) {
            Ok(status) => status,
            Err(err) => {
                let _ = writeln!(stderr, "{err}");
                1
            }
        }
    }
}

/// `bg [job_spec]` — resumes a stopped job in the background (`SIGCONT`,
/// no terminal handoff, doesn't block) — see
/// [`Shell::resume_job_in_background`].
struct Bg;
impl Builtin for Bg {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let id = match resolve_fg_bg_job(shell, args, "bg", stderr) {
            Ok(id) => id,
            Err(status) => return status,
        };
        let command = shell
            .job_table
            .get(id)
            .map(|job| job.command.clone())
            .unwrap_or_default();
        match shell.resume_job_in_background(id) {
            Ok(()) => {
                let _ = writeln!(stdout, "[{id}]  {command} &");
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "{err}");
                1
            }
        }
    }
}

/// `wait [pid...]` (POSIX special builtin) — with operands, blocks until
/// each named job finishes (in argument order) and reports the *last*
/// one's exit status as `$?`; with none, blocks until every currently
/// running background job finishes and always reports `0` — see
/// [`Shell::wait_for_pid`]/[`Shell::wait_for_all_background_jobs`] for
/// the actual blocking mechanics.
struct Wait;
impl Builtin for Wait {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.is_empty() {
            shell.wait_for_all_background_jobs();
            shell.purge_finished_notified_jobs();
            return 0;
        }

        let mut status = 0;
        for arg in args {
            let Ok(pid) = arg.parse::<i32>() else {
                let _ = writeln!(stderr, "wait: {arg}: arguments must be process or job IDs");
                status = 127;
                continue;
            };
            match shell.wait_for_pid(pid) {
                Some(code) => status = code,
                None => {
                    let _ = writeln!(stderr, "wait: pid {pid} is not a child of this shell");
                    status = 127;
                }
            }
        }
        shell.purge_finished_notified_jobs();
        status
    }
}

/// `trap [-p] [action] [signal...]` (POSIX special builtin) — see
/// [`Shell::trap_set_by_name`]/[`Shell::trap_describe_by_name`]/
/// [`Shell::trap_list`] for the actual signal-name parsing/disposition
/// bookkeeping this wraps.
///
/// `action`: `-` resets each named signal to its original disposition
/// ([`TrapAction::Default`]); any other first non-flag argument is the
/// command text to run ([`TrapAction::Command`]) — including an empty
/// string, which is *not* a no-op text to run but [`TrapAction::Ignore`]
/// (POSIX: `trap '' SIG` ignores the signal, distinct from never
/// trapping it at all — see that variant's own docs for why the
/// distinction matters).
struct Trap;
impl Builtin for Trap {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.first().map(String::as_str) == Some("-p") {
            let specs = &args[1..];
            if specs.is_empty() {
                for line in shell.trap_list() {
                    let _ = writeln!(stdout, "{line}");
                }
                return 0;
            }
            let mut status = 0;
            for spec in specs {
                match shell.trap_describe_by_name(spec) {
                    Ok(Some(line)) => {
                        let _ = writeln!(stdout, "{line}");
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let _ = writeln!(stderr, "{err}");
                        status = 1;
                    }
                }
            }
            return status;
        }

        let Some(action_arg) = args.first() else {
            // Bare `trap`: list every currently registered trap.
            for line in shell.trap_list() {
                let _ = writeln!(stdout, "{line}");
            }
            return 0;
        };

        let action = if action_arg == "-" {
            TrapAction::Default
        } else if action_arg.is_empty() {
            TrapAction::Ignore
        } else {
            TrapAction::Command(action_arg.clone())
        };

        let specs = &args[1..];
        if specs.is_empty() {
            let _ = writeln!(stderr, "trap: usage: trap [-p] [action] [signal...]");
            return 2;
        }

        let mut status = 0;
        for spec in specs {
            if let Err(err) = shell.trap_set_by_name(spec, action.clone()) {
                let _ = writeln!(stderr, "{err}");
                status = 1;
            }
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(builtin: &dyn Builtin, shell: &mut Shell, args: &[String]) -> (i32, String, String) {
        run_with_stdin(builtin, shell, args, &[])
    }

    fn run_with_stdin(
        builtin: &dyn Builtin,
        shell: &mut Shell,
        args: &[String],
        stdin: &[u8],
    ) -> (i32, String, String) {
        let mut stdin = std::io::Cursor::new(stdin.to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = builtin.run(shell, args, &mut stdin, &mut stdout, &mut stderr);
        (
            status,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn register_all_registers_every_builtin() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        for name in [
            "cd", "exit", "export", "echo", "pwd", "break", "continue", "local", "return", "set",
            "shift", "jobs", "fg", "bg", "wait", "trap", "unset", "umask", "type", "command",
            "eval", ".", "source", "test", "[", "read", "getopts", "printf", "kill", "declare",
            "typeset", "exec", "alias", "unalias", "readonly",
        ] {
            assert!(shell.builtin(name).is_some(), "missing builtin: {name}");
        }
    }

    #[test]
    fn register_all_classifies_special_vs_regular_builtins_per_posix_2_9_1() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        for name in [
            "break", "continue", "exit", "export", "return", "set", "shift", "unset", "eval", ".",
            "exec", "readonly",
        ] {
            assert!(
                shell.is_special_builtin(name),
                "expected {name} to be special"
            );
        }
        for name in [
            "cd", "echo", "pwd", "local", "umask", "type", "command", "test", "[", "read",
            "getopts", "printf", "kill", "declare", "typeset", "alias", "unalias",
        ] {
            assert!(
                !shell.is_special_builtin(name),
                "expected {name} to be regular"
            );
        }
    }

    #[test]
    fn export_with_value_sets_env_var() {
        let mut shell = Shell::new();
        run(&Export, &mut shell, &["FOO=bar".to_string()]);
        assert_eq!(shell.get_var("FOO"), Some("bar"));
    }

    #[test]
    fn bare_export_promotes_shell_var() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("FOO".to_string(), "bar".to_string());
        run(&Export, &mut shell, &["FOO".to_string()]);
        assert_eq!(shell.get_var("FOO"), Some("bar"));
        assert!(!shell.shell_vars.contains_key("FOO"));
    }

    // ---- readonly ------------------------------------------------------------

    #[test]
    fn readonly_assigns_and_marks_readonly() {
        let mut shell = Shell::new();
        run(&Readonly, &mut shell, &["x=5".to_string()]);
        assert_eq!(shell.get_var("x"), Some("5"));
        assert!(shell.readonly_vars.contains("x"));
    }

    #[test]
    fn readonly_bare_name_marks_an_existing_variable_without_changing_its_value() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("x".to_string(), "hi".to_string());
        let (status, _, _) = run(&Readonly, &mut shell, &["x".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("x"), Some("hi"));
        assert!(shell.readonly_vars.contains("x"));
    }

    #[test]
    fn readonly_lists_every_readonly_variable_with_no_args() {
        let mut shell = Shell::new();
        run(&Readonly, &mut shell, &["x=5".to_string()]);
        let (status, stdout, _) = run(&Readonly, &mut shell, &[]);
        assert_eq!(status, 0);
        assert_eq!(stdout, "readonly x='5'\n");
    }

    #[test]
    fn reassigning_a_readonly_variable_via_a_bare_assignment_is_rejected() {
        // Exercises the real enforcement point directly
        // (`conch-shell-core::exec::exec_simple`'s bare-assignment
        // branch), not just the `readonly` builtin's own bookkeeping.
        let mut shell = Shell::new();
        run(&Readonly, &mut shell, &["x=5".to_string()]);
        let status = conch_shell_core::exec_command_list(
            &conch_shell_parser::parse("x=6").unwrap(),
            &mut shell,
        );
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("x"), Some("5"));
    }

    #[test]
    fn unsetting_a_readonly_variable_is_rejected_and_non_fatal() {
        let mut shell = Shell::new();
        run(&Readonly, &mut shell, &["x=5".to_string()]);
        let (status, _, stderr) = run(&Unset, &mut shell, &["x".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("readonly"));
        assert_eq!(shell.get_var("x"), Some("5"));
    }

    #[test]
    fn cd_updates_cwd() {
        let mut shell = Shell::new();
        let original = shell.cwd.clone();
        let (status, _, _) = run(&Cd, &mut shell, &["/tmp".to_string()]);
        assert_eq!(status, 0);
        assert_ne!(shell.cwd, original);
        // Restore so other tests running in-process aren't affected.
        std::env::set_current_dir(&original).unwrap();
    }

    #[test]
    fn cd_nonexistent_dir_fails() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Cd, &mut shell, &["/no/such/path/hopefully".to_string()]);
        assert_eq!(status, 1);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn echo_writes_args_to_stdout() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&Echo, &mut shell, &["hi".to_string(), "there".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(stdout, "hi there\n");
    }

    #[test]
    fn pwd_writes_cwd_to_stdout() {
        let mut shell = Shell::new();
        let expected = format!("{}\n", shell.cwd.display());
        let (status, stdout, _) = run(&Pwd, &mut shell, &[]);
        assert_eq!(status, 0);
        assert_eq!(stdout, expected);
    }

    #[test]
    fn break_inside_a_loop_sets_pending_control_flow() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        let (status, _, stderr) = run(&Break, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(stderr.is_empty());
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Break(1)));
    }

    #[test]
    fn break_with_explicit_level() {
        let mut shell = Shell::new();
        shell.loop_depth = 3;
        run(&Break, &mut shell, &["2".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Break(2)));
    }

    #[test]
    fn continue_inside_a_loop_sets_pending_control_flow() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        run(&Continue, &mut shell, &[]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Continue(1)));
    }

    #[test]
    fn break_outside_any_loop_is_a_harmless_no_op() {
        // Confirmed against real bash: `break` outside a loop just
        // prints a diagnostic and does *not* stop the rest of the
        // script -- it must never set a pending signal here.
        let mut shell = Shell::new();
        assert_eq!(shell.loop_depth, 0);
        let (status, _, stderr) = run(&Break, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(!stderr.is_empty());
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn break_zero_is_out_of_range() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        let (status, _, stderr) = run(&Break, &mut shell, &["0".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("out of range"));
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn break_non_numeric_argument_is_out_of_range() {
        let mut shell = Shell::new();
        shell.loop_depth = 1;
        let (status, _, stderr) = run(&Break, &mut shell, &["nope".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("out of range"));
    }

    // ---- local ---------------------------------------------------------------

    #[test]
    fn local_outside_a_function_errors_and_has_no_effect() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Local, &mut shell, &["X=1".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("can only be used in a function"));
        assert_eq!(shell.get_var("X"), None);
    }

    #[test]
    fn local_with_value_shadows_and_is_recorded_for_restoration() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        let (status, _, _) = run(&Local, &mut shell, &["X=inner".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("X"), Some("inner"));
        shell.pop_local_frame();
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn bare_local_with_no_value_makes_the_name_genuinely_unset() {
        // Confirmed against real bash: `local X` (no `=`) makes
        // `${X+is-set}` empty -- genuinely unset, not merely `""`.
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        run(&Local, &mut shell, &["X".to_string()]);
        assert_eq!(shell.get_var("X"), None);
        shell.pop_local_frame();
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn local_of_an_already_exported_variable_stays_exported() {
        // Confirmed against real bash: `export X=1; f() { local X=2; };
        // f` still shows `X=2` in a child process's inherited
        // environment -- `local` inherits the export attribute of an
        // existing same-named global, it doesn't strip it.
        let mut shell = Shell::new();
        shell.env_vars.insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        run(&Local, &mut shell, &["X=inner".to_string()]);
        assert!(shell.env_vars.contains_key("X"));
        assert_eq!(shell.get_var("X"), Some("inner"));
    }

    #[test]
    fn local_invalid_identifier_errors_but_still_processes_later_arguments() {
        // Confirmed against real bash: `local 1x=2 y=3` still declares
        // `y`, with `$?` = 1 only because of the earlier, unrelated
        // failure.
        let mut shell = Shell::new();
        shell.push_local_frame();
        let (status, _, stderr) = run(&Local, &mut shell, &["1x=2".to_string(), "y=3".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("not a valid identifier"));
        assert_eq!(shell.get_var("y"), Some("3"));
    }

    // ---- return ----------------------------------------------------------------

    #[test]
    fn return_outside_a_function_errors_without_setting_pending_control_flow() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Return, &mut shell, &[]);
        assert_eq!(status, 2);
        assert!(stderr.contains("can only `return'"));
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn return_with_explicit_code_sets_pending_control_flow() {
        let mut shell = Shell::new();
        shell.push_local_frame();
        run(&Return, &mut shell, &["5".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(5)));
    }

    #[test]
    fn return_with_no_argument_uses_the_last_status() {
        let mut shell = Shell::new();
        shell.last_status = 7;
        shell.push_local_frame();
        run(&Return, &mut shell, &[]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(7)));
    }

    #[test]
    fn return_wraps_an_out_of_range_code_into_a_byte_like_bash_does() {
        // Confirmed against real bash: `return 300` leaves `$?` at 44,
        // `return -1` leaves it at 255 -- wrapped immediately, observable
        // in the very same shell, not deferred to process exit.
        let mut shell = Shell::new();
        shell.push_local_frame();
        run(&Return, &mut shell, &["300".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(44)));

        let mut shell = Shell::new();
        shell.push_local_frame();
        run(&Return, &mut shell, &["-1".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(255)));
    }

    #[test]
    fn return_non_numeric_argument_still_stops_the_call() {
        let mut shell = Shell::new();
        shell.push_local_frame();
        let (status, _, stderr) = run(&Return, &mut shell, &["abc".to_string()]);
        assert_eq!(status, 2);
        assert!(stderr.contains("numeric argument required"));
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Return(2)));
    }

    // ---- set -- / shift ----------------------------------------------------------

    #[test]
    fn set_dash_dash_replaces_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["old".to_string()];
        let (status, _, _) = run(
            &Set,
            &mut shell,
            &["--".to_string(), "a".to_string(), "b".to_string()],
        );
        assert_eq!(status, 0);
        assert_eq!(
            shell.positional_params,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn set_dash_dash_alone_clears_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["old".to_string()];
        run(&Set, &mut shell, &["--".to_string()]);
        assert!(shell.positional_params.is_empty());
    }

    #[test]
    fn set_dash_e_enables_errexit() {
        let mut shell = Shell::new();
        assert!(!shell.errexit);
        let (status, _, _) = run(&Set, &mut shell, &["-e".to_string()]);
        assert_eq!(status, 0);
        assert!(shell.errexit);
    }

    #[test]
    fn set_plus_e_disables_errexit() {
        let mut shell = Shell::new();
        shell.errexit = true;
        run(&Set, &mut shell, &["+e".to_string()]);
        assert!(!shell.errexit);
    }

    #[test]
    fn set_combined_short_flags() {
        let mut shell = Shell::new();
        run(&Set, &mut shell, &["-eux".to_string()]);
        assert!(shell.errexit);
        assert!(shell.nounset);
        assert!(shell.xtrace);
    }

    #[test]
    fn set_dash_o_by_name() {
        let mut shell = Shell::new();
        run(&Set, &mut shell, &["-o".to_string(), "errexit".to_string()]);
        assert!(shell.errexit);
    }

    #[test]
    fn set_dash_f_enables_noglob() {
        let mut shell = Shell::new();
        run(&Set, &mut shell, &["-f".to_string()]);
        assert!(shell.noglob);
    }

    #[test]
    fn set_dash_capital_c_enables_noclobber() {
        let mut shell = Shell::new();
        run(&Set, &mut shell, &["-C".to_string()]);
        assert!(shell.noclobber);
    }

    #[test]
    fn set_dash_o_unrecognized_name_is_an_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(
            &Set,
            &mut shell,
            &["-o".to_string(), "notarealoption".to_string()],
        );
        assert_eq!(status, 1);
        assert!(stderr.contains("not yet supported"));
    }

    #[test]
    fn set_flags_followed_by_operands_still_replace_positional_parameters() {
        // POSIX: once every recognized option flag is consumed, any
        // remaining arguments *are* new positional parameters, even
        // without an explicit `--` (that's only needed to disambiguate
        // an operand that itself looks like a flag).
        let mut shell = Shell::new();
        run(
            &Set,
            &mut shell,
            &["-e".to_string(), "a".to_string(), "b".to_string()],
        );
        assert!(shell.errexit);
        assert_eq!(
            shell.positional_params,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn set_with_only_flags_leaves_positional_parameters_untouched() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["untouched".to_string()];
        run(&Set, &mut shell, &["-e".to_string()]);
        assert_eq!(shell.positional_params, vec!["untouched".to_string()]);
    }

    #[test]
    fn bare_set_lists_every_variable() {
        let mut shell = Shell::new();
        shell.shell_vars.clear();
        shell.env_vars.clear();
        shell.shell_vars.insert("X".to_string(), "1".to_string());
        let (status, stdout, _) = run(&Set, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(stdout.contains("X=1\n"));
    }

    #[test]
    fn shift_default_removes_one_positional_parameter() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (status, _, _) = run(&Shift, &mut shell, &[]);
        assert_eq!(status, 0);
        assert_eq!(
            shell.positional_params,
            vec!["b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn shift_n_removes_n_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        run(&Shift, &mut shell, &["2".to_string()]);
        assert_eq!(shell.positional_params, vec!["c".to_string()]);
    }

    #[test]
    fn shift_zero_is_a_no_op() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string()];
        let (status, _, _) = run(&Shift, &mut shell, &["0".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(shell.positional_params, vec!["a".to_string()]);
    }

    #[test]
    fn shift_beyond_available_params_errors_without_changing_them() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string(), "b".to_string()];
        let (status, _, _) = run(&Shift, &mut shell, &["5".to_string()]);
        assert_eq!(status, 1);
        assert_eq!(
            shell.positional_params,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn shift_negative_count_errors() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["a".to_string()];
        let (status, _, stderr) = run(&Shift, &mut shell, &["-1".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("out of range"));
    }

    // ---- job control: jobs / fg / bg / wait / trap ---------------------
    //
    // The actual job-table-populated, real-process-backed behavior of
    // these builtins (a real backgrounded job showing up in `jobs`,
    // `fg`/`bg` actually resuming one, `wait` actually blocking) needs a
    // genuinely spawned process to exercise meaningfully — like
    // `exec_subshell`'s own execution (see `conch-shell-core::exec`'s
    // test module docs), that's `tests/conch-difftest`'s job, not a
    // unit test here. What's covered here is everything reachable
    // without spawning anything: error paths, and `trap`'s pure
    // string-table bookkeeping (already itself exercised more directly
    // in `conch-shell-core::signals`' own tests; these confirm the
    // builtin wires arguments to it correctly).

    #[test]
    fn fg_without_job_control_is_a_clear_error() {
        let mut shell = Shell::new();
        assert!(!shell.job_control_active);
        let (status, _, stderr) = run(&Fg, &mut shell, &[]);
        assert_eq!(status, 1);
        assert!(stderr.contains("no job control"));
    }

    #[test]
    fn bg_without_job_control_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Bg, &mut shell, &[]);
        assert_eq!(status, 1);
        assert!(stderr.contains("no job control"));
    }

    #[test]
    fn wait_on_an_unknown_pid_is_a_clear_error_without_blocking() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Wait, &mut shell, &["999999".to_string()]);
        assert_eq!(status, 127);
        assert!(stderr.contains("not a child"));
    }

    #[test]
    fn wait_with_a_non_numeric_argument_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Wait, &mut shell, &["not-a-pid".to_string()]);
        assert_eq!(status, 127);
        assert!(stderr.contains("must be process or job IDs"));
    }

    #[test]
    fn wait_with_no_operands_and_no_background_jobs_is_an_immediate_no_op() {
        let mut shell = Shell::new();
        let (status, _, _) = run(&Wait, &mut shell, &[]);
        assert_eq!(status, 0);
    }

    #[test]
    fn jobs_with_an_empty_table_prints_nothing() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&Jobs, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(stdout.is_empty());
    }

    #[test]
    fn trap_registers_a_command_and_lists_it() {
        let mut shell = Shell::new();
        let (status, _, _) = run(
            &Trap,
            &mut shell,
            &["echo caught".to_string(), "USR1".to_string()],
        );
        assert_eq!(status, 0);
        let (status, stdout, _) = run(&Trap, &mut shell, &[]);
        assert_eq!(status, 0);
        assert!(stdout.contains("echo caught"));
    }

    #[test]
    fn trap_dash_p_reports_nothing_for_an_untrapped_signal() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&Trap, &mut shell, &["-p".to_string(), "USR2".to_string()]);
        assert_eq!(status, 0);
        assert!(stdout.is_empty());
    }

    #[test]
    fn trap_dash_p_reports_a_registered_trap() {
        let mut shell = Shell::new();
        run(
            &Trap,
            &mut shell,
            &["echo hi".to_string(), "USR2".to_string()],
        );
        let (status, stdout, _) = run(&Trap, &mut shell, &["-p".to_string(), "USR2".to_string()]);
        assert_eq!(status, 0);
        assert!(stdout.contains("echo hi"));
    }

    #[test]
    fn trap_with_an_unrecognized_signal_errors() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(
            &Trap,
            &mut shell,
            &["echo hi".to_string(), "NOTASIGNAL".to_string()],
        );
        assert_eq!(status, 1);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn trap_exit_registers_the_exit_trap() {
        let mut shell = Shell::new();
        run(
            &Trap,
            &mut shell,
            &["echo bye".to_string(), "EXIT".to_string()],
        );
        assert_eq!(shell.exit_trap(), Some("echo bye"));
    }

    // ---- unset -----------------------------------------------------------

    #[test]
    fn unset_removes_a_shell_variable() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("X".to_string(), "1".to_string());
        let (status, _, _) = run(&Unset, &mut shell, &["X".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("X"), None);
    }

    #[test]
    fn unset_removes_an_exported_variable() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".to_string(), "1".to_string());
        run(&Unset, &mut shell, &["X".to_string()]);
        assert_eq!(shell.get_var("X"), None);
    }

    #[test]
    fn unset_of_an_unset_name_is_a_silent_success() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Unset, &mut shell, &["NEVER_SET".to_string()]);
        assert_eq!(status, 0);
        assert!(stderr.is_empty());
    }

    #[test]
    fn unset_falls_back_to_a_function_when_no_variable_exists() {
        let mut shell = Shell::new();
        shell.functions.insert("f".to_string(), dummy_function());
        run(&Unset, &mut shell, &["f".to_string()]);
        assert!(!shell.functions.contains_key("f"));
    }

    #[test]
    fn unset_dash_f_only_ever_targets_a_function() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("f".to_string(), "1".to_string());
        shell.functions.insert("f".to_string(), dummy_function());
        run(&Unset, &mut shell, &["-f".to_string(), "f".to_string()]);
        assert!(!shell.functions.contains_key("f"));
        // The variable is untouched -- `-f` never falls back to it.
        assert_eq!(shell.get_var("f"), Some("1"));
    }

    #[test]
    fn unset_invalid_identifier_is_an_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Unset, &mut shell, &["1x".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("not a valid identifier"));
    }

    /// A minimal, valid [`conch_shell_parser::FunctionDefinition`] (an
    /// empty brace-group body) — enough for tests that only need *some*
    /// function registered under a name, not anything about what it does.
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

    // ---- umask -------------------------------------------------------------
    //
    // The process umask is genuine process-wide (not per-`Shell`) state --
    // `cargo test` runs every test in this binary on a shared pool of
    // threads by default, so two umask tests running concurrently could
    // otherwise observe/clobber each other's mask mid-assertion (the same
    // category of cross-test-parallelism hazard `conch-shell-core::job`'s
    // and `::signals`' own test modules already call out and design
    // around for their own real-process-state tests). Serialized here via
    // a shared lock rather than relying on it being unlikely in practice.
    static UMASK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn umask_with_no_args_prints_the_current_mask() {
        let _guard = UMASK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut shell = Shell::new();
        // Set a known mask first so this test doesn't depend on whatever
        // the surrounding test process happened to inherit.
        run(&Umask, &mut shell, &["022".to_string()]);
        let (status, stdout, _) = run(&Umask, &mut shell, &[]);
        assert_eq!(status, 0);
        assert_eq!(stdout.trim(), "0022");
    }

    #[test]
    fn umask_sets_and_reads_back_an_octal_mask() {
        let _guard = UMASK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut shell = Shell::new();
        run(&Umask, &mut shell, &["027".to_string()]);
        let (_, stdout, _) = run(&Umask, &mut shell, &[]);
        assert_eq!(stdout.trim(), "0027");
        // Restore a conventional mask so later tests in this same process
        // (umask is genuinely process-global) aren't affected.
        run(&Umask, &mut shell, &["022".to_string()]);
    }

    #[test]
    fn umask_dash_s_prints_symbolic_form() {
        let _guard = UMASK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut shell = Shell::new();
        run(&Umask, &mut shell, &["022".to_string()]);
        let (status, stdout, _) = run(&Umask, &mut shell, &["-S".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(stdout.trim(), "u=rwx,g=rx,o=rx");
    }

    #[test]
    fn umask_rejects_a_symbolic_mode_clearly() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Umask, &mut shell, &["u+w".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("not yet supported"));
    }

    // ---- type ----------------------------------------------------------------

    #[test]
    fn type_reports_a_builtin() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        let (status, stdout, _) = run(&Type, &mut shell, &["cd".to_string()]);
        assert_eq!(status, 0);
        assert!(stdout.contains("cd is a shell builtin"));
    }

    #[test]
    fn type_reports_a_keyword() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(&Type, &mut shell, &["if".to_string()]);
        assert_eq!(status, 0);
        assert!(stdout.contains("if is a shell keyword"));
    }

    #[test]
    fn type_reports_not_found() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Type, &mut shell, &["nope_not_a_thing".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("not found"));
    }

    #[test]
    fn type_dash_t_prints_only_the_category_word() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        let (_, stdout, _) = run(&Type, &mut shell, &["-t".to_string(), "cd".to_string()]);
        assert_eq!(stdout.trim(), "builtin");
    }

    // ---- command -------------------------------------------------------------

    #[test]
    fn command_dash_v_reports_a_builtin_by_name() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        let (status, stdout, _) = run(&Command, &mut shell, &["-v".to_string(), "cd".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(stdout.trim(), "cd");
    }

    #[test]
    fn command_dash_v_on_an_unknown_name_is_a_silent_failure() {
        let mut shell = Shell::new();
        let (status, stdout, _) = run(
            &Command,
            &mut shell,
            &["-v".to_string(), "nope_not_a_thing".to_string()],
        );
        assert_eq!(status, 1);
        assert!(stdout.is_empty());
    }

    #[test]
    fn command_runs_a_builtin_directly() {
        let mut shell = Shell::new();
        register_all(&mut shell);
        let (status, stdout, _) = run(&Command, &mut shell, &["pwd".to_string()]);
        assert_eq!(status, 0);
        assert_eq!(stdout.trim(), shell.cwd.display().to_string());
    }

    // ---- eval --------------------------------------------------------------

    #[test]
    fn eval_with_no_arguments_is_a_no_op() {
        let mut shell = Shell::new();
        let (status, _, _) = run(&Eval, &mut shell, &[]);
        assert_eq!(status, 0);
    }

    #[test]
    fn eval_runs_a_variable_assignment_against_the_current_shell() {
        let mut shell = Shell::new();
        run(&Eval, &mut shell, &["X=set_by_eval".to_string()]);
        assert_eq!(shell.get_var("X"), Some("set_by_eval"));
    }

    #[test]
    fn eval_joins_multiple_arguments_with_a_space() {
        // `eval X=1 ';' echo "$X"` -- also confirms the runtime-constructed
        // string is genuinely re-lexed (the `;` argument only becomes an
        // operator once joined into "X=1 ; echo $X" and re-parsed).
        let mut shell = Shell::new();
        run(
            &Eval,
            &mut shell,
            &["X=1".to_string(), ";".to_string(), "Y=$X".to_string()],
        );
        assert_eq!(shell.get_var("Y"), Some("1"));
    }

    #[test]
    fn eval_propagates_the_evaluated_commands_exit_status() {
        let mut shell = Shell::new();
        let (status, _, _) = run(&Eval, &mut shell, &["false".to_string()]);
        assert_eq!(status, 1);
    }

    #[test]
    fn eval_syntax_error_is_a_shell_error_not_silently_swallowed() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Eval, &mut shell, &["if".to_string()]);
        assert_eq!(status, 2);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn eval_introduces_no_execution_boundary_break_escapes_to_the_real_enclosing_loop() {
        // Confirmed against real bash: `eval break` inside a loop breaks
        // that enclosing loop -- `eval` must never itself consume the
        // pending `ControlFlow::Break` the way `exec_program`'s top-level
        // backstop would (an earlier, buggy version of this builtin used
        // `exec_program` and failed exactly this check).
        let mut shell = Shell::new();
        register_all(&mut shell);
        shell.loop_depth = 1;
        run(&Eval, &mut shell, &["break".to_string()]);
        assert_eq!(shell.pending_control_flow, Some(ControlFlow::Break(1)));
    }

    #[test]
    fn eval_assignment_respects_an_enclosing_local_scope() {
        // Confirmed against real bash: `eval X=1` inside a function
        // mutates whatever `X` binding is currently live, respecting an
        // enclosing `local` exactly like a plain `X=1` typed at that same
        // spot would -- `eval` shares the caller's variable scope
        // entirely, it doesn't get its own.
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.push_local_frame();
        let previous = shell.capture_var("X");
        shell.record_local("X".to_string(), previous);
        shell.set_local("X", Some("inner".to_string()));

        run(&Eval, &mut shell, &["X=changed".to_string()]);
        assert_eq!(shell.get_var("X"), Some("changed"));

        shell.pop_local_frame();
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn eval_recursion_guard_reports_a_clean_error_instead_of_overflowing_the_stack() {
        // Simulates having already recursed to the limit (a real
        // self-referential `eval $x` with `x` containing `eval $x`
        // would otherwise recurse through genuine Rust stack frames with
        // no natural bound -- see `Shell::eval_depth`'s own docs) rather
        // than actually constructing such a value and waiting for a
        // real stack overflow in a unit test.
        let mut shell = Shell::new();
        shell.eval_depth = MAX_EVAL_RECURSION;
        let (status, _, stderr) = run(&Eval, &mut shell, &["true".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("maximum eval nesting depth"));
    }

    // ---- . / source ----------------------------------------------------------

    #[test]
    fn source_runs_a_file_against_the_current_shell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, "X=set_by_source\n").unwrap();
        let mut shell = Shell::new();
        let (status, _, _) = run(&Source, &mut shell, &[path.display().to_string()]);
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("X"), Some("set_by_source"));
    }

    #[test]
    fn source_sets_positional_parameters_from_extra_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, "Y=\"$1:$2:$#\"\n").unwrap();
        let mut shell = Shell::new();
        shell.positional_params = vec!["caller1".to_string()];
        run(
            &Source,
            &mut shell,
            &[path.display().to_string(), "a".to_string(), "b".to_string()],
        );
        assert_eq!(shell.get_var("Y"), Some("a:b:2"));
        // Restored afterward -- the caller's own positional parameters
        // are untouched once the sourced script finishes.
        assert_eq!(shell.positional_params, vec!["caller1".to_string()]);
    }

    #[test]
    fn source_return_ends_the_sourced_script_with_that_status_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, "X=before\nreturn 7\nX=after\n").unwrap();
        let mut shell = Shell::new();
        register_all(&mut shell);
        let (status, _, _) = run(&Source, &mut shell, &[path.display().to_string()]);
        assert_eq!(status, 7);
        assert_eq!(shell.get_var("X"), Some("before"));
        // A sourced script's `return` doesn't leave anything pending for
        // the caller to trip over afterward.
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn source_of_a_nonexistent_file_is_a_clear_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Source, &mut shell, &["/no/such/script.sh".to_string()]);
        assert_eq!(status, 1);
        assert!(stderr.contains("No such file"));
    }

    #[test]
    fn source_with_no_argument_is_a_usage_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Source, &mut shell, &[]);
        assert_eq!(status, 2);
        assert!(!stderr.is_empty());
    }

    #[test]
    fn source_break_propagates_to_the_callers_own_enclosing_loop() {
        // Confirmed against real bash: `break` inside a sourced file
        // escapes to whatever loop already encloses the `. file` call
        // itself -- a sourced script is *not* its own break/continue
        // boundary the way a function call is.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, "break\n").unwrap();
        let mut shell = Shell::new();
        register_all(&mut shell);
        shell.loop_depth = 1;
        let (status, _, _) = run(&Source, &mut shell, &[path.display().to_string()]);
        assert_eq!(status, 0);
        assert_eq!(
            shell.pending_control_flow,
            Some(ControlFlow::Break(1)),
            "break must still be pending for the caller's own loop to consume"
        );
    }
}
