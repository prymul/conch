//! `test`/`[` (POSIX 2.9.4) — a small grammar and evaluator of its own,
//! operating entirely on already-expanded `&[String]` arguments (word
//! expansion has already happened by the time a builtin ever sees its
//! `args` — see `conch_shell_core::exec::exec_simple`'s own docs), so
//! none of this needs any `conch-shell-lexer`/`conch-shell-parser`
//! involvement.
//!
//! # Grounding: the argument-count-driven algorithm
//!
//! POSIX 2.9.4 (and its historical XSI-extended form, which is what real
//! bash actually implements — see below) specifies `test`'s evaluation
//! *by argument count*, not as one uniform grammar:
//!
//! - 0 arguments: false (exit 1).
//! - 1 argument: true (exit 0) iff it is not the empty string.
//! - 2 arguments: `!` negates a null-test of the second; otherwise the
//!   first must be a unary primary applied to the second.
//! - 3 arguments: **if the second argument is a binary primary, `$1 OP
//!   $3` is evaluated — checked *before* testing whether `$1` is `!`.**
//!   This priority order is the one genuinely load-bearing subtlety
//!   here: it's what makes `x='!'; [ "$x" = foo ]` correctly evaluate the
//!   string comparison `"!" = "foo"` instead of misreading the *value*
//!   `"!"` as the negation operator — `test`/`[` cannot see that `$x` was
//!   quoted by the time they run; only this priority order gets it right
//!   regardless. Only if `$2` isn't a binary primary does `$1 == '!'`
//!   (negating the 2-argument evaluation of `$2 $3`) get tried, and only
//!   after *that* does `$1 == '(' && $3 == ')'` (grouping, reducing to
//!   the 1-argument non-null test of `$2`).
//! - 4 arguments: `$1 == '!'` negates the 3-argument evaluation of `$2 $3
//!   $4`; else `$1 == '(' && $4 == ')'` reduces to the 2-argument
//!   evaluation of `$2 $3`.
//! - More than 4 arguments: POSIX itself says "the results are
//!   unspecified." Real bash (the differential oracle this project
//!   tracks over strict POSIX-only where the two diverge — see e.g. the
//!   arithmetic module's own docs for the same policy elsewhere) actually
//!   implements a real left-to-right expression grammar here: `!`/`-a`/
//!   `-o`/`(...)` combine arbitrarily, with `-a` binding tighter than
//!   `-o` (both left-associative) and unary/binary primaries binding
//!   tightest of all — [`parse_or`]/[`parse_and`]/[`parse_unary`]/
//!   [`parse_primary`] implement exactly that precedence climb, and are
//!   reused for the 4-argument case too when neither of its two special
//!   forms above matches (e.g. `-f a -a -f b`, which isn't `!`- or
//!   paren-wrapped but is still valid `-a`-combined bash usage).
//!
//! `-a`/`-o` (and, by extension, this whole >4-argument grammar) were
//! removed from POSIX itself in Issue 8 (2024) — flagged here explicitly
//! as a bash-extension-over-current-POSIX situation, not silently
//! treated as still-POSIX, per this agent's own grounding rules — but
//! real bash still implements them (its `test`/`[` predates the removal
//! and never dropped them), so they're in scope here.
//!
//! # Operators implemented
//!
//! Unary (file tests): `-b -c -d -e -f -g -h -L -k -p -r -S -s -t -u -w
//! -x` (POSIX baseline, plus `-k`/`-L` as documented historical/bash
//! additions). Unary (string/shell): `-n -z` (POSIX), `-v` (bash
//! extension: variable is set). Binary: `-eq -ne -lt -le -gt -ge`
//! (integer comparison, POSIX), `= != < >` (string comparison, POSIX;
//! `==` accepted as a bash-extension synonym for `=`), `-nt -ot -ef`
//! (file comparison, POSIX). `-o optname` (bash extension: shell option
//! enabled) and `-G`/`-O`/`-N` (bash extensions: group/owner/mtime-vs-
//! atime) are **not** implemented — flagged as a documented gap rather
//! than silently misevaluating, since `-o` here would need this crate to
//! decide what `set -o`/`-e`/`-x` state even means yet (a separate,
//! not-yet-landed piece of this same phase).

use std::io::{Read, Write};

use conch_shell_core::{Builtin, Shell};

/// `test expr...` (POSIX regular builtin — not special; `[` is a
/// distinct registration of this exact same implementation, see
/// [`Bracket`]).
pub struct Test;
impl Builtin for Test {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        run_test(shell, args, stderr)
    }
}

/// `[ expr... ]` — identical to [`Test`] except it's invoked with a
/// trailing literal `]` (since `[` is an ordinary command name, POSIX
/// requires that closing bracket as the *last* argument, stripped before
/// evaluation) — confirmed against real bash: a missing/misplaced `]` is
/// its own distinct usage error, before the expression itself is ever
/// evaluated.
pub struct Bracket;
impl Builtin for Bracket {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        let Some((last, rest)) = args.split_last() else {
            let _ = writeln!(stderr, "[: missing `]'");
            return 2;
        };
        if last != "]" {
            let _ = writeln!(stderr, "[: missing `]'");
            return 2;
        }
        run_test(shell, rest, stderr)
    }
}

fn run_test(shell: &Shell, args: &[String], stderr: &mut dyn Write) -> i32 {
    match eval_test_args(args, shell) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(message) => {
            let _ = writeln!(stderr, "test: {message}");
            2
        }
    }
}

fn eval_test_args(args: &[String], shell: &Shell) -> Result<bool, String> {
    match args.len() {
        0 => Ok(false),
        1 => Ok(one_arg(&args[0])),
        2 => two_arg(&args[0], &args[1], shell),
        3 => three_arg(&args[0], &args[1], &args[2], shell),
        4 => four_arg(&args[0], &args[1], &args[2], &args[3], shell),
        _ => {
            let mut pos = 0;
            let result = parse_or(args, &mut pos, shell)?;
            if pos != args.len() {
                return Err(format!("`{}': extra argument", args[pos]));
            }
            Ok(result)
        }
    }
}

fn one_arg(a: &str) -> bool {
    !a.is_empty()
}

fn two_arg(a: &str, b: &str, shell: &Shell) -> Result<bool, String> {
    if a == "!" {
        return Ok(b.is_empty());
    }
    match unary_op(a) {
        Some(op) => eval_unary(op, b, shell),
        None => Err(format!("{a}: unary operator expected")),
    }
}

fn three_arg(a: &str, b: &str, c: &str, shell: &Shell) -> Result<bool, String> {
    // Binary-primary check first -- see the module docs for why this
    // exact priority (over the `!`-negation check just below) is the
    // one genuinely load-bearing subtlety in this whole algorithm.
    if let Some(op) = binary_op(b) {
        return eval_binary(a, op, c);
    }
    if a == "!" {
        return two_arg(b, c, shell).map(|r| !r);
    }
    if a == "(" && c == ")" {
        return Ok(one_arg(b));
    }
    Err(format!("{b}: binary operator expected"))
}

fn four_arg(a: &str, b: &str, c: &str, d: &str, shell: &Shell) -> Result<bool, String> {
    if a == "!" {
        return three_arg(b, c, d, shell).map(|r| !r);
    }
    if a == "(" && d == ")" {
        return two_arg(b, c, shell);
    }
    // Bash extension beyond POSIX's own two special forms above: an
    // ordinary `-a`/`-o`-combined 4-token expression that isn't `!`- or
    // paren-wrapped at all (`-f a -a -f b`) — the general precedence-climb
    // parser handles this uniformly with the >4-argument case.
    let owned: Vec<String> = [a, b, c, d].into_iter().map(str::to_string).collect();
    let mut pos = 0;
    let result = parse_or(&owned, &mut pos, shell)?;
    if pos != owned.len() {
        return Err(format!("`{}': extra argument", owned[pos]));
    }
    Ok(result)
}

// ---- the general >4-argument grammar: `-o` < `-a` < `!` < primary ---------

fn parse_or(args: &[String], pos: &mut usize, shell: &Shell) -> Result<bool, String> {
    let mut left = parse_and(args, pos, shell)?;
    while args.get(*pos).map(String::as_str) == Some("-o") {
        *pos += 1;
        let right = parse_and(args, pos, shell)?;
        left = left || right;
    }
    Ok(left)
}

fn parse_and(args: &[String], pos: &mut usize, shell: &Shell) -> Result<bool, String> {
    let mut left = parse_unary(args, pos, shell)?;
    while args.get(*pos).map(String::as_str) == Some("-a") {
        *pos += 1;
        let right = parse_unary(args, pos, shell)?;
        left = left && right;
    }
    Ok(left)
}

fn parse_unary(args: &[String], pos: &mut usize, shell: &Shell) -> Result<bool, String> {
    if args.get(*pos).map(String::as_str) == Some("!") {
        *pos += 1;
        let inner = parse_unary(args, pos, shell)?;
        return Ok(!inner);
    }
    parse_primary(args, pos, shell)
}

fn parse_primary(args: &[String], pos: &mut usize, shell: &Shell) -> Result<bool, String> {
    let Some(tok) = args.get(*pos) else {
        return Err("argument expected".to_string());
    };

    if tok == "(" {
        *pos += 1;
        let inner = parse_or(args, pos, shell)?;
        match args.get(*pos) {
            Some(t) if t == ")" => {
                *pos += 1;
                return Ok(inner);
            }
            _ => return Err("expected `)'".to_string()),
        }
    }

    if let Some(op) = unary_op(tok) {
        *pos += 1;
        let operand = args
            .get(*pos)
            .ok_or_else(|| format!("{tok}: argument expected"))?;
        *pos += 1;
        return eval_unary(op, operand, shell);
    }

    let word1 = tok.clone();
    *pos += 1;
    if let Some(op_tok) = args.get(*pos)
        && let Some(op) = binary_op(op_tok)
    {
        *pos += 1;
        let word2 = args
            .get(*pos)
            .ok_or_else(|| format!("{op_tok}: argument expected"))?
            .clone();
        *pos += 1;
        return eval_binary(&word1, op, &word2);
    }
    Ok(one_arg(&word1))
}

// ---- operator tables and evaluation ----------------------------------------

#[derive(Clone, Copy)]
enum UnaryOp {
    BlockSpecial,
    CharSpecial,
    Directory,
    Exists,
    RegularFile,
    SetGid,
    SymLink,
    Sticky,
    NamedPipe,
    Readable,
    Socket,
    SizeNonZero,
    Terminal,
    SetUid,
    Writable,
    Executable,
    StringEmpty,
    StringNonEmpty,
    VarSet,
}

fn unary_op(tok: &str) -> Option<UnaryOp> {
    Some(match tok {
        "-b" => UnaryOp::BlockSpecial,
        "-c" => UnaryOp::CharSpecial,
        "-d" => UnaryOp::Directory,
        "-e" => UnaryOp::Exists,
        "-f" => UnaryOp::RegularFile,
        "-g" => UnaryOp::SetGid,
        "-h" | "-L" => UnaryOp::SymLink,
        "-k" => UnaryOp::Sticky,
        "-p" => UnaryOp::NamedPipe,
        "-r" => UnaryOp::Readable,
        "-S" => UnaryOp::Socket,
        "-s" => UnaryOp::SizeNonZero,
        "-t" => UnaryOp::Terminal,
        "-u" => UnaryOp::SetUid,
        "-w" => UnaryOp::Writable,
        "-x" => UnaryOp::Executable,
        "-z" => UnaryOp::StringEmpty,
        "-n" => UnaryOp::StringNonEmpty,
        "-v" => UnaryOp::VarSet,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum BinaryOp {
    StrEq,
    StrNe,
    StrLt,
    StrGt,
    IntEq,
    IntNe,
    IntLt,
    IntLe,
    IntGt,
    IntGe,
    NewerThan,
    OlderThan,
    SameFile,
}

fn binary_op(tok: &str) -> Option<BinaryOp> {
    Some(match tok {
        "=" | "==" => BinaryOp::StrEq,
        "!=" => BinaryOp::StrNe,
        "<" => BinaryOp::StrLt,
        ">" => BinaryOp::StrGt,
        "-eq" => BinaryOp::IntEq,
        "-ne" => BinaryOp::IntNe,
        "-lt" => BinaryOp::IntLt,
        "-le" => BinaryOp::IntLe,
        "-gt" => BinaryOp::IntGt,
        "-ge" => BinaryOp::IntGe,
        "-nt" => BinaryOp::NewerThan,
        "-ot" => BinaryOp::OlderThan,
        "-ef" => BinaryOp::SameFile,
        _ => return None,
    })
}

fn eval_unary(op: UnaryOp, operand: &str, shell: &Shell) -> Result<bool, String> {
    use std::io::IsTerminal as _;
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};

    Ok(match op {
        UnaryOp::StringEmpty => operand.is_empty(),
        UnaryOp::StringNonEmpty => !operand.is_empty(),
        UnaryOp::VarSet => shell.get_var(operand).is_some(),
        UnaryOp::Terminal => match operand.parse::<i32>() {
            Ok(0) => std::io::stdin().is_terminal(),
            Ok(1) => std::io::stdout().is_terminal(),
            Ok(2) => std::io::stderr().is_terminal(),
            // Any other fd number: not supported without a raw `isatty`
            // wrapper this crate has no dependency for (`conch-shell-
            // builtins` deliberately has no direct `nix` dependency —
            // see the crate module docs) — a documented gap, not a
            // silent wrong answer for the overwhelmingly common 0/1/2
            // case above.
            _ => false,
        },
        UnaryOp::Exists => std::fs::symlink_metadata(operand).is_ok(),
        UnaryOp::SymLink => {
            std::fs::symlink_metadata(operand).is_ok_and(|meta| meta.file_type().is_symlink())
        }
        UnaryOp::Directory => std::fs::metadata(operand).is_ok_and(|meta| meta.is_dir()),
        UnaryOp::RegularFile => std::fs::metadata(operand).is_ok_and(|meta| meta.is_file()),
        UnaryOp::BlockSpecial => {
            std::fs::metadata(operand).is_ok_and(|meta| meta.file_type().is_block_device())
        }
        UnaryOp::CharSpecial => {
            std::fs::metadata(operand).is_ok_and(|meta| meta.file_type().is_char_device())
        }
        UnaryOp::NamedPipe => {
            std::fs::metadata(operand).is_ok_and(|meta| meta.file_type().is_fifo())
        }
        UnaryOp::Socket => {
            std::fs::metadata(operand).is_ok_and(|meta| meta.file_type().is_socket())
        }
        UnaryOp::SizeNonZero => std::fs::metadata(operand).is_ok_and(|meta| meta.len() > 0),
        UnaryOp::SetGid => {
            std::fs::metadata(operand).is_ok_and(|meta| meta.permissions().mode() & 0o2000 != 0)
        }
        UnaryOp::SetUid => {
            std::fs::metadata(operand).is_ok_and(|meta| meta.permissions().mode() & 0o4000 != 0)
        }
        UnaryOp::Sticky => {
            std::fs::metadata(operand).is_ok_and(|meta| meta.permissions().mode() & 0o1000 != 0)
        }
        UnaryOp::Readable => conch_shell_core::path_readable(operand),
        UnaryOp::Writable => conch_shell_core::path_writable(operand),
        UnaryOp::Executable => conch_shell_core::path_executable(operand),
    })
}

fn eval_binary(a: &str, op: BinaryOp, b: &str) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(match op {
        BinaryOp::StrEq => a == b,
        BinaryOp::StrNe => a != b,
        BinaryOp::StrLt => a < b,
        BinaryOp::StrGt => a > b,
        BinaryOp::IntEq
        | BinaryOp::IntNe
        | BinaryOp::IntLt
        | BinaryOp::IntLe
        | BinaryOp::IntGt
        | BinaryOp::IntGe => {
            let a = parse_test_integer(a)?;
            let b = parse_test_integer(b)?;
            match op {
                BinaryOp::IntEq => a == b,
                BinaryOp::IntNe => a != b,
                BinaryOp::IntLt => a < b,
                BinaryOp::IntLe => a <= b,
                BinaryOp::IntGt => a > b,
                BinaryOp::IntGe => a >= b,
                _ => unreachable!(),
            }
        }
        BinaryOp::NewerThan => match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => {
                a.mtime() > b.mtime() || (a.mtime() == b.mtime() && a.mtime_nsec() > b.mtime_nsec())
            }
            // Confirmed against real bash: `a -nt b` is true if `a`
            // exists and `b` does not (and false the other way around).
            (Ok(_), Err(_)) => true,
            _ => false,
        },
        BinaryOp::OlderThan => match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => {
                a.mtime() < b.mtime() || (a.mtime() == b.mtime() && a.mtime_nsec() < b.mtime_nsec())
            }
            (Err(_), Ok(_)) => true,
            _ => false,
        },
        BinaryOp::SameFile => match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        },
    })
}

/// POSIX 2.9.4's integer-comparison operands: an optionally-signed
/// decimal integer, nothing else — a non-numeric operand (`[ x -eq 1 ]`)
/// is a genuine syntax error (`test: x: integer expression expected`,
/// exit 2), not a silent `false`, matching real bash.
fn parse_test_integer(s: &str) -> Result<i64, String> {
    s.trim()
        .parse::<i64>()
        .map_err(|_| format!("{s}: integer expression expected"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(builtin: &dyn Builtin, args: &[String]) -> (i32, String) {
        let mut shell = Shell::new();
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = builtin.run(&mut shell, args, &mut stdin, &mut stdout, &mut stderr);
        (status, String::from_utf8(stderr).unwrap())
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn zero_args_is_false() {
        assert_eq!(run(&Test, &s(&[])).0, 1);
    }

    #[test]
    fn one_arg_nonempty_is_true() {
        assert_eq!(run(&Test, &s(&["hi"])).0, 0);
    }

    #[test]
    fn one_arg_empty_is_false() {
        assert_eq!(run(&Test, &s(&[""])).0, 1);
    }

    #[test]
    fn two_arg_bang_negates_null_test() {
        assert_eq!(run(&Test, &s(&["!", ""])).0, 0);
        assert_eq!(run(&Test, &s(&["!", "x"])).0, 1);
    }

    #[test]
    fn two_arg_unary_string_ops() {
        assert_eq!(run(&Test, &s(&["-z", ""])).0, 0);
        assert_eq!(run(&Test, &s(&["-n", "x"])).0, 0);
    }

    #[test]
    fn two_arg_unrecognized_first_operand_is_a_syntax_error() {
        let (status, stderr) = run(&Test, &s(&["foo", "bar"]));
        assert_eq!(status, 2);
        assert!(stderr.contains("unary operator expected"));
    }

    #[test]
    fn three_arg_string_equality() {
        assert_eq!(run(&Test, &s(&["a", "=", "a"])).0, 0);
        assert_eq!(run(&Test, &s(&["a", "=", "b"])).0, 1);
        assert_eq!(run(&Test, &s(&["a", "!=", "b"])).0, 0);
    }

    #[test]
    fn three_arg_binary_check_wins_over_bang_when_dollar1_is_literally_bang() {
        // The load-bearing disambiguation this module's own docs call
        // out: `[ "$x" = foo ]` with x='!' must evaluate the *string*
        // comparison, not misread "!" as negation.
        assert_eq!(run(&Test, &s(&["!", "=", "!"])).0, 0);
        assert_eq!(run(&Test, &s(&["!", "=", "foo"])).0, 1);
    }

    #[test]
    fn three_arg_bang_negates_two_arg_eval() {
        assert_eq!(run(&Test, &s(&["!", "-z", "x"])).0, 0);
        assert_eq!(run(&Test, &s(&["!", "-n", "x"])).0, 1);
    }

    #[test]
    fn three_arg_paren_grouping_reduces_to_one_arg() {
        assert_eq!(run(&Test, &s(&["(", "x", ")"])).0, 0);
        assert_eq!(run(&Test, &s(&["(", "", ")"])).0, 1);
    }

    #[test]
    fn four_arg_bang_negates_three_arg_eval() {
        assert_eq!(run(&Test, &s(&["!", "a", "=", "b"])).0, 0);
        assert_eq!(run(&Test, &s(&["!", "a", "=", "a"])).0, 1);
    }

    #[test]
    fn four_arg_paren_wrapped_two_arg_eval() {
        assert_eq!(run(&Test, &s(&["(", "-n", "x", ")"])).0, 0);
    }

    #[test]
    fn four_arg_dash_a_combination_without_bang_or_parens() {
        assert_eq!(run(&Test, &s(&["x", "-a", "-n", "y"])).0, 0);
        assert_eq!(run(&Test, &s(&["", "-a", "-n", "y"])).0, 1);
    }

    #[test]
    fn general_dash_a_dash_o_precedence_and_left_associativity() {
        // `-a` binds tighter than `-o`: `false -a false -o true` is
        // `(false -a false) -o true` = true.
        assert_eq!(
            run(&Test, &s(&["", "-a", "", "-o", "x"])).0,
            0,
            "-a must bind tighter than -o"
        );
    }

    #[test]
    fn general_parens_override_precedence() {
        // `! ( x -o "" )` -- true -o inside parens, negated -> false.
        assert_eq!(run(&Test, &s(&["!", "(", "x", "-o", "", ")"])).0, 1);
    }

    #[test]
    fn integer_comparisons() {
        assert_eq!(run(&Test, &s(&["3", "-lt", "5"])).0, 0);
        assert_eq!(run(&Test, &s(&["5", "-le", "5"])).0, 0);
        assert_eq!(run(&Test, &s(&["5", "-eq", "5"])).0, 0);
        assert_eq!(run(&Test, &s(&["5", "-ne", "6"])).0, 0);
        assert_eq!(run(&Test, &s(&["7", "-gt", "5"])).0, 0);
        assert_eq!(run(&Test, &s(&["-1", "-ge", "-1"])).0, 0);
    }

    #[test]
    fn non_integer_operand_is_a_syntax_error() {
        let (status, stderr) = run(&Test, &s(&["x", "-eq", "1"]));
        assert_eq!(status, 2);
        assert!(stderr.contains("integer expression expected"));
    }

    #[test]
    fn string_ordering() {
        assert_eq!(run(&Test, &s(&["a", "<", "b"])).0, 0);
        assert_eq!(run(&Test, &s(&["b", ">", "a"])).0, 0);
    }

    #[test]
    fn file_tests_against_a_real_temp_dir_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "hi").unwrap();
        let dir_str = dir.path().display().to_string();
        let file_str = file.display().to_string();

        assert_eq!(run(&Test, &s(&["-d", &dir_str])).0, 0);
        assert_eq!(run(&Test, &s(&["-f", &file_str])).0, 0);
        assert_eq!(run(&Test, &s(&["-e", &file_str])).0, 0);
        assert_eq!(run(&Test, &s(&["-s", &file_str])).0, 0);
        assert_eq!(run(&Test, &s(&["-r", &file_str])).0, 0);
        assert_eq!(run(&Test, &s(&["-e", "/no/such/path/hopefully"])).0, 1);
    }

    #[test]
    fn var_set_operator() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("X".to_string(), "1".to_string());
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = Test.run(
            &mut shell,
            &s(&["-v", "X"]),
            &mut stdin,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 0);
        let status = Test.run(
            &mut shell,
            &s(&["-v", "NEVER_SET"]),
            &mut stdin,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 1);
    }

    // ---- [ ] specifically ------------------------------------------------

    #[test]
    fn bracket_requires_a_trailing_close_bracket() {
        let (status, stderr) = run(&Bracket, &s(&["x"]));
        assert_eq!(status, 2);
        assert!(stderr.contains("missing"));
    }

    #[test]
    fn bracket_strips_the_trailing_bracket_before_evaluating() {
        assert_eq!(run(&Bracket, &s(&["x", "]"])).0, 0);
        assert_eq!(run(&Bracket, &s(&["a", "=", "a", "]"])).0, 0);
    }
}
