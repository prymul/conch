//! `alias`/`unalias` (bash extension, not POSIX) — pure storage
//! (`Shell::aliases`) manipulation; the actual expansion mechanism lives
//! in `conch-shell-parser` (`parse_with_aliases`) — see that function's
//! own module docs for the full algorithm, its quote-awareness, and its
//! one documented gap (a same-line `alias foo=bar; foo` doesn't see its
//! own just-defined alias, since this crate parses a whole input string
//! in one pass with no interleaved execution).
//!
//! Policy (not this module's own decision — see `conch`'s binary crate):
//! aliases only ever expand in an interactive shell, matching real
//! bash's own default (`-c`/script-file/`eval`/`.`-sourced text all use
//! plain, non-alias-aware `parse` instead of `parse_with_aliases`) —
//! these two builtins still work identically everywhere regardless
//! (defining an alias never fails just because it wouldn't currently be
//! consulted), only *expansion* is gated.

use std::io::{Read, Write};

use conch_shell_core::{Builtin, Shell};

/// `alias [-p] [name[=value]...]`.
pub struct Alias;
impl Builtin for Alias {
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
            let mut names: Vec<&String> = shell.aliases.keys().collect();
            names.sort();
            for name in names {
                print_one(name, &shell.aliases[name], stdout);
            }
            return 0;
        }

        let mut status = 0;
        for arg in args {
            match arg.split_once('=') {
                Some((name, value)) => {
                    if name.is_empty() || name.contains('/') {
                        let _ = writeln!(stderr, "alias: `{arg}': invalid alias name");
                        status = 1;
                        continue;
                    }
                    shell.aliases.insert(name.to_string(), value.to_string());
                }
                None => match shell.aliases.get(arg.as_str()) {
                    Some(value) => print_one(arg, value, stdout),
                    None => {
                        let _ = writeln!(stderr, "alias: {arg}: not found");
                        status = 1;
                    }
                },
            }
        }
        status
    }
}

fn print_one(name: &str, value: &str, stdout: &mut dyn Write) {
    let _ = writeln!(stdout, "alias {name}='{value}'");
}

/// `unalias [-a] name...`.
pub struct Unalias;
impl Builtin for Unalias {
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        _stdin: &mut dyn Read,
        _stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32 {
        if args.first().map(String::as_str) == Some("-a") {
            shell.aliases.clear();
            return 0;
        }
        if args.is_empty() {
            let _ = writeln!(stderr, "unalias: usage: unalias [-a] name [name ...]");
            return 2;
        }
        let mut status = 0;
        for name in args {
            if shell.aliases.remove(name).is_none() {
                let _ = writeln!(stderr, "unalias: {name}: not found");
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
        let mut stdin = std::io::empty();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = builtin.run(shell, args, &mut stdin, &mut stdout, &mut stderr);
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
    fn defines_and_lists_an_alias() {
        let mut shell = Shell::new();
        run(&Alias, &mut shell, &s(&["ll=ls -la"]));
        assert_eq!(shell.aliases.get("ll"), Some(&"ls -la".to_string()));
        let (status, stdout, _) = run(&Alias, &mut shell, &s(&[]));
        assert_eq!(status, 0);
        assert_eq!(stdout, "alias ll='ls -la'\n");
    }

    #[test]
    fn prints_one_named_alias() {
        let mut shell = Shell::new();
        shell.aliases.insert("ll".to_string(), "ls -la".to_string());
        let (status, stdout, _) = run(&Alias, &mut shell, &s(&["ll"]));
        assert_eq!(status, 0);
        assert_eq!(stdout, "alias ll='ls -la'\n");
    }

    #[test]
    fn unknown_name_is_an_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Alias, &mut shell, &s(&["nope"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("not found"));
    }

    #[test]
    fn unalias_removes_a_definition() {
        let mut shell = Shell::new();
        shell.aliases.insert("ll".to_string(), "ls -la".to_string());
        let (status, _, _) = run(&Unalias, &mut shell, &s(&["ll"]));
        assert_eq!(status, 0);
        assert!(!shell.aliases.contains_key("ll"));
    }

    #[test]
    fn unalias_dash_a_clears_everything() {
        let mut shell = Shell::new();
        shell.aliases.insert("ll".to_string(), "ls -la".to_string());
        shell.aliases.insert("la".to_string(), "ls -a".to_string());
        run(&Unalias, &mut shell, &s(&["-a"]));
        assert!(shell.aliases.is_empty());
    }

    #[test]
    fn unalias_unknown_name_is_an_error() {
        let mut shell = Shell::new();
        let (status, _, stderr) = run(&Unalias, &mut shell, &s(&["nope"]));
        assert_eq!(status, 1);
        assert!(stderr.contains("not found"));
    }
}
