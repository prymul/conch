//! Phase 1 word expansion: resolves a [`Word`] to a single string.
//!
//! Per the plan's Phase 1 scope, this implements only simple parameter
//! expansion (`$VAR`/`${VAR}`) — no field splitting, no pathname expansion
//! (globbing), no command/arithmetic substitution, no `${VAR:-default}`
//! style operators. A [`Word`] always expands to exactly one string; the
//! POSIX-specified splitting/globbing pipeline (2.6.5/2.6.6) is Phase 2.

use conch_shell_parser::{Parameter, SpecialParameter, Word, WordSegment};

use crate::Shell;

/// An error encountered while expanding a word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// The word contains a construct Phase 1's expansion engine doesn't
    /// evaluate yet (full parameter-expansion operators, command
    /// substitution, arithmetic expansion) — real POSIX/bash syntax that
    /// `conch-shell-parser` accepts but that this phase can't evaluate.
    #[error("{construct} is not yet supported (planned for Phase 2)")]
    UnsupportedConstruct { construct: &'static str },
}

/// Expands `word` against `shell`'s variables, per the Phase 1 scope
/// documented on this module.
pub fn expand_word(word: &Word, shell: &Shell) -> Result<String, ExpandError> {
    let mut out = String::new();
    for segment in &word.segments {
        expand_segment(segment, shell, &mut out)?;
    }
    Ok(out)
}

fn expand_segment(
    segment: &WordSegment,
    shell: &Shell,
    out: &mut String,
) -> Result<(), ExpandError> {
    match segment {
        WordSegment::Literal(text) | WordSegment::SingleQuoted(text) => {
            out.push_str(text);
            Ok(())
        }
        WordSegment::DoubleQuoted(segments) => {
            for inner in segments {
                expand_segment(inner, shell, out)?;
            }
            Ok(())
        }
        WordSegment::Parameter(param) => {
            out.push_str(&expand_parameter(param, shell));
            Ok(())
        }
        WordSegment::ComplexParameterExpansion(_) => Err(ExpandError::UnsupportedConstruct {
            construct: "parameter expansion operators (${var:-word}, ${var#pattern}, ...)",
        }),
        WordSegment::CommandSubstitution(_) => Err(ExpandError::UnsupportedConstruct {
            construct: "command substitution ($(...) or `...`)",
        }),
        WordSegment::ArithmeticExpansion(_) => Err(ExpandError::UnsupportedConstruct {
            construct: "arithmetic expansion ($((...)))",
        }),
    }
}

fn expand_parameter(param: &Parameter, shell: &Shell) -> String {
    match param {
        Parameter::Name(name) => shell.get_var(name).unwrap_or("").to_string(),
        // Positional parameters and most special parameters need a real
        // argv/PID/last-background-PID model this phase doesn't have yet;
        // an unset-and-empty reading is POSIX-correct for a parameter
        // that's never been given a value (2.5.1/2.5.2) and keeps
        // expansion total rather than partial for Phase 1.
        Parameter::Positional(_) => String::new(),
        Parameter::Special(special) => match special {
            SpecialParameter::Question => shell.last_status.to_string(),
            _ => String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conch_shell_parser::parse;

    fn expand_str(input: &str, shell: &Shell) -> Result<String, ExpandError> {
        let list = parse(input).unwrap();
        let conch_shell_parser::Command::Simple(cmd) = &list.items[0].and_or.first.commands[0]
        else {
            unreachable!()
        };
        expand_word(cmd.name.as_ref().unwrap(), shell)
    }

    #[test]
    fn literal_word_expands_unchanged() {
        let shell = Shell::new();
        assert_eq!(expand_str("hello", &shell).unwrap(), "hello");
    }

    #[test]
    fn single_quoted_expands_unchanged() {
        let shell = Shell::new();
        assert_eq!(expand_str("'$HOME'", &shell).unwrap(), "$HOME");
    }

    #[test]
    fn unset_variable_expands_to_empty() {
        let shell = Shell::new();
        assert_eq!(expand_str("$NO_SUCH_VAR", &shell).unwrap(), "");
    }

    #[test]
    fn set_variable_expands_to_its_value() {
        let mut shell = Shell::new();
        shell
            .env_vars
            .insert("GREETING".to_string(), "hi".to_string());
        assert_eq!(expand_str("$GREETING", &shell).unwrap(), "hi");
    }

    #[test]
    fn double_quoted_expands_nested_parameter() {
        let mut shell = Shell::new();
        shell.env_vars.insert("X".to_string(), "5".to_string());
        assert_eq!(expand_str("\"val=$X\"", &shell).unwrap(), "val=5");
    }

    #[test]
    fn question_mark_expands_to_last_status() {
        let mut shell = Shell::new();
        shell.last_status = 7;
        assert_eq!(expand_str("$?", &shell).unwrap(), "7");
    }

    #[test]
    fn complex_parameter_expansion_is_unsupported() {
        let shell = Shell::new();
        assert!(matches!(
            expand_str("${FOO:-bar}", &shell),
            Err(ExpandError::UnsupportedConstruct { .. })
        ));
    }
}
