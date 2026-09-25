use super::*;

fn parse_ok(input: &str) -> CommandList {
    parse(input).unwrap_or_else(|e| panic!("expected {input:?} to parse, got error: {e}"))
}

/// Parses `input`, panicking unless it produces exactly one
/// [`CommandListItem`] whose `and_or` has no `&&`/`||` and whose single
/// pipeline has no `|` — i.e. exactly one [`SimpleCommand`] — returning
/// it.
fn parse_one_simple_command(input: &str) -> SimpleCommand {
    let list = parse_ok(input);
    assert_eq!(
        list.items.len(),
        1,
        "expected exactly one item for {input:?}"
    );
    let item = &list.items[0];
    assert!(
        item.and_or.rest.is_empty(),
        "expected no &&/|| for {input:?}"
    );
    assert_eq!(
        item.and_or.first.commands.len(),
        1,
        "expected no pipeline for {input:?}"
    );
    let Command::Simple(cmd) = &item.and_or.first.commands[0] else {
        unreachable!("expected a Command::Simple")
    };
    cmd.clone()
}

fn plain_word(text: &str) -> Word {
    Word::new(vec![WordSegment::Literal(text.to_string())])
}

fn plain_words(texts: &[&str]) -> Vec<Word> {
    texts.iter().map(|t| plain_word(t)).collect()
}

/// Parses `input`, panicking unless it produces exactly one
/// [`CommandListItem`] whose `and_or` has no `&&`/`||` and whose single
/// pipeline has no `|` — i.e. exactly one [`CompoundCommand`] — returning
/// it.
fn parse_one_compound_command(input: &str) -> CompoundCommand {
    let list = parse_ok(input);
    assert_eq!(
        list.items.len(),
        1,
        "expected exactly one item for {input:?}"
    );
    let item = &list.items[0];
    assert!(
        item.and_or.rest.is_empty(),
        "expected no &&/|| for {input:?}"
    );
    assert_eq!(
        item.and_or.first.commands.len(),
        1,
        "expected no pipeline for {input:?}"
    );
    let Command::Compound(cmd) = &item.and_or.first.commands[0] else {
        unreachable!("expected a Command::Compound for {input:?}")
    };
    cmd.clone()
}

/// Parses `input`, panicking unless it produces exactly one
/// [`CommandListItem`] whose `and_or` has no `&&`/`||` and whose single
/// pipeline has no `|` — i.e. exactly one [`FunctionDefinition`] —
/// returning it.
fn parse_one_function_definition(input: &str) -> FunctionDefinition {
    let list = parse_ok(input);
    assert_eq!(
        list.items.len(),
        1,
        "expected exactly one item for {input:?}"
    );
    let item = &list.items[0];
    assert!(
        item.and_or.rest.is_empty(),
        "expected no &&/|| for {input:?}"
    );
    assert_eq!(
        item.and_or.first.commands.len(),
        1,
        "expected no pipeline for {input:?}"
    );
    let Command::Function(func) = &item.and_or.first.commands[0] else {
        unreachable!("expected a Command::Function for {input:?}")
    };
    func.clone()
}

/// Extracts the single command name of `list`'s one-and-only
/// [`SimpleCommand`] item, for terse assertions on a compound command's
/// body contents.
fn only_command_name(list: &CommandList) -> &str {
    assert_eq!(list.items.len(), 1, "expected exactly one body item");
    let Command::Simple(cmd) = &list.items[0].and_or.first.commands[0] else {
        unreachable!("expected a Command::Simple")
    };
    cmd.name.as_ref().unwrap().as_plain_literal().unwrap()
}

// ---- simple commands -----------------------------------------------------

#[test]
fn bare_command_name_only() {
    let cmd = parse_one_simple_command("echo");
    assert_eq!(cmd.name, Some(plain_word("echo")));
    assert!(cmd.args.is_empty());
    assert!(cmd.assignments.is_empty());
    assert!(cmd.redirects.is_empty());
}

#[test]
fn command_with_arguments() {
    let cmd = parse_one_simple_command("echo hello world");
    assert_eq!(cmd.name, Some(plain_word("echo")));
    assert_eq!(cmd.args, vec![plain_word("hello"), plain_word("world")]);
}

#[test]
fn leading_assignment_is_recognized_and_split() {
    let cmd = parse_one_simple_command("FOO=bar echo hi");
    assert_eq!(
        cmd.assignments,
        vec![Assignment {
            name: "FOO".to_string(),
            value: plain_word("bar"),
        }]
    );
    assert_eq!(cmd.name, Some(plain_word("echo")));
    assert_eq!(cmd.args, vec![plain_word("hi")]);
}

#[test]
fn multiple_leading_assignments_keep_original_order() {
    let cmd = parse_one_simple_command("A=1 B=2 env");
    assert_eq!(
        cmd.assignments,
        vec![
            Assignment {
                name: "A".to_string(),
                value: plain_word("1"),
            },
            Assignment {
                name: "B".to_string(),
                value: plain_word("2"),
            },
        ]
    );
}

#[test]
fn bare_assignment_with_no_command_name_is_valid() {
    let cmd = parse_one_simple_command("FOO=bar");
    assert_eq!(
        cmd.assignments,
        vec![Assignment {
            name: "FOO".to_string(),
            value: plain_word("bar"),
        }]
    );
    assert_eq!(cmd.name, None);
    assert!(cmd.args.is_empty());
}

#[test]
fn assignment_value_can_contain_expansion_sites() {
    let cmd = parse_one_simple_command("FOO=$bar cmd");
    assert_eq!(
        cmd.assignments,
        vec![Assignment {
            name: "FOO".to_string(),
            value: Word::new(vec![WordSegment::Parameter(Parameter::Name(
                "bar".to_string()
            ))]),
        }]
    );
}

#[test]
fn assignment_shaped_word_after_command_name_is_a_plain_argument() {
    // Confirmed against real bash: `echo FOO=bar` prints `FOO=bar`.
    let cmd = parse_one_simple_command("echo FOO=bar");
    assert!(cmd.assignments.is_empty());
    assert_eq!(cmd.name, Some(plain_word("echo")));
    assert_eq!(cmd.args, vec![plain_word("FOO=bar")]);
}

#[test]
fn quoted_assignment_name_is_not_recognized_as_an_assignment() {
    // Confirmed against real bash: `'FOO'=bar` runs a command literally
    // named `FOO=bar` (not found), rather than assigning FOO.
    let cmd = parse_one_simple_command("'FOO'=bar");
    assert!(cmd.assignments.is_empty());
    assert_eq!(
        cmd.name,
        Some(Word::new(vec![
            WordSegment::SingleQuoted("FOO".to_string()),
            WordSegment::Literal("=bar".to_string()),
        ]))
    );
}

#[test]
fn assignment_name_must_be_a_valid_name() {
    // A leading digit isn't a valid `Name` start, so `2=x` is a command
    // named "2=x", not an assignment.
    let cmd = parse_one_simple_command("2=x");
    assert!(cmd.assignments.is_empty());
    assert_eq!(cmd.name, Some(plain_word("2=x")));
}

// ---- redirects -------------------------------------------------------------

#[test]
fn redirects_after_command_name_in_order() {
    let cmd = parse_one_simple_command("cmd < in > out >> app");
    assert_eq!(cmd.name, Some(plain_word("cmd")));
    assert!(cmd.args.is_empty());
    assert_eq!(
        cmd.redirects,
        vec![
            Redirect {
                fd: None,
                operator: RedirectOperator::Input,
                target: plain_word("in"),
            },
            Redirect {
                fd: None,
                operator: RedirectOperator::Output,
                target: plain_word("out"),
            },
            Redirect {
                fd: None,
                operator: RedirectOperator::Append,
                target: plain_word("app"),
            },
        ]
    );
}

#[test]
fn redirect_with_io_number_prefix() {
    let cmd = parse_one_simple_command("cmd 2>err.log");
    assert_eq!(
        cmd.redirects,
        vec![Redirect {
            fd: Some(2),
            operator: RedirectOperator::Output,
            target: plain_word("err.log"),
        }]
    );
}

#[test]
fn redirect_before_command_name_is_a_prefix_redirect() {
    let cmd = parse_one_simple_command("> out echo hi");
    assert_eq!(cmd.name, Some(plain_word("echo")));
    assert_eq!(cmd.args, vec![plain_word("hi")]);
    assert_eq!(
        cmd.redirects,
        vec![Redirect {
            fd: None,
            operator: RedirectOperator::Output,
            target: plain_word("out"),
        }]
    );
}

#[test]
fn redirect_only_with_no_command_name_is_valid() {
    let cmd = parse_one_simple_command("> out");
    assert_eq!(cmd.name, None);
    assert_eq!(
        cmd.redirects,
        vec![Redirect {
            fd: None,
            operator: RedirectOperator::Output,
            target: plain_word("out"),
        }]
    );
}

#[test]
fn redirect_missing_filename_is_an_error() {
    let err = parse("cmd >").unwrap_err();
    assert_eq!(
        err,
        ParseError::UnexpectedEof {
            expected: "a filename".to_string(),
        }
    );
}

// ---- pipelines -------------------------------------------------------------

#[test]
fn pipeline_of_three_commands() {
    let list = parse_ok("a | b | c");
    assert_eq!(list.items.len(), 1);
    let pipeline = &list.items[0].and_or.first;
    assert_eq!(pipeline.commands.len(), 3);
    for (cmd, expected) in pipeline.commands.iter().zip(["a", "b", "c"]) {
        let Command::Simple(cmd) = cmd else {
            unreachable!("expected a Command::Simple")
        };
        assert_eq!(cmd.name, Some(plain_word(expected)));
    }
}

#[test]
fn pipeline_allows_newline_after_pipe() {
    let list = parse_ok("a |\nb");
    let pipeline = &list.items[0].and_or.first;
    assert_eq!(pipeline.commands.len(), 2);
}

// ---- and/or lists -----------------------------------------------------------

#[test]
fn and_or_preserves_operator_between_each_pair() {
    let list = parse_ok("a && b || c");
    assert_eq!(list.items.len(), 1);
    let and_or = &list.items[0].and_or;
    let Command::Simple(first) = &and_or.first.commands[0] else {
        unreachable!("expected a Command::Simple")
    };
    assert_eq!(first.name, Some(plain_word("a")));
    assert_eq!(and_or.rest.len(), 2);
    assert_eq!(and_or.rest[0].0, LogicalOp::And);
    assert_eq!(and_or.rest[1].0, LogicalOp::Or);
}

#[test]
fn and_or_allows_newline_after_operator() {
    let list = parse_ok("a &&\nb");
    let and_or = &list.items[0].and_or;
    assert_eq!(and_or.rest.len(), 1);
    assert_eq!(and_or.rest[0].0, LogicalOp::And);
}

// ---- lists / separators -----------------------------------------------------

#[test]
fn semicolon_separated_list_without_trailing_separator() {
    let list = parse_ok("a;b;c");
    assert_eq!(list.items.len(), 3);
    assert_eq!(list.items[0].separator, Separator::Sequential);
    assert_eq!(list.items[1].separator, Separator::Sequential);
    assert_eq!(list.items[2].separator, Separator::None);
}

#[test]
fn trailing_semicolon_does_not_add_an_empty_item() {
    let list = parse_ok("a;b;");
    assert_eq!(list.items.len(), 2);
    assert_eq!(list.items[1].separator, Separator::Sequential);
}

#[test]
fn trailing_ampersand_marks_async() {
    let list = parse_ok("sleep 1 &");
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.items[0].separator, Separator::Async);
}

#[test]
fn newline_separates_list_items() {
    let list = parse_ok("a\nb");
    assert_eq!(list.items.len(), 2);
    assert_eq!(list.items[0].separator, Separator::Sequential);
    assert_eq!(list.items[1].separator, Separator::None);
}

#[test]
fn blank_lines_between_items_are_skipped() {
    let list = parse_ok("a\n\n\nb");
    assert_eq!(list.items.len(), 2);
}

#[test]
fn leading_and_trailing_blank_lines_are_skipped() {
    let list = parse_ok("\n\na\n\n");
    assert_eq!(list.items.len(), 1);
}

#[test]
fn comment_does_not_produce_an_extra_item() {
    let list = parse_ok("a # comment\nb");
    assert_eq!(list.items.len(), 2);
}

// ---- empty input -------------------------------------------------------------

#[test]
fn empty_input_parses_to_an_empty_list() {
    assert_eq!(parse_ok(""), CommandList::default());
}

#[test]
fn whitespace_and_comment_only_input_parses_to_an_empty_list() {
    assert_eq!(
        parse_ok("   \n  # just a comment\n\n"),
        CommandList::default()
    );
}

// ---- deferred-construct errors -----------------------------------------------

#[test]
fn heredoc_is_a_clear_unsupported_construct_error() {
    let err = parse("cat <<EOF").unwrap_err();
    match err {
        ParseError::UnsupportedConstruct { message, .. } => {
            assert!(message.contains("here-document"), "message was: {message}");
        }
        other => panic!("expected UnsupportedConstruct, got {other:?}"),
    }
}

#[test]
fn fd_duplicating_redirect_is_a_clear_unsupported_construct_error() {
    let err = parse("cmd 2>&1").unwrap_err();
    assert!(matches!(err, ParseError::UnsupportedConstruct { .. }));
}

#[test]
fn trailing_unmatched_rparen_after_a_valid_command_is_a_syntax_error() {
    // '(' / ')' are now meaningful (subshells), so a stray trailing ')'
    // with nothing to match is an ordinary syntax error, not a
    // "not yet supported" one.
    let err = parse("a b )").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

// ---- genuine syntax errors ----------------------------------------------------

#[test]
fn double_pipe_with_nothing_between_is_a_syntax_error() {
    let err = parse("a || | b").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn leading_semicolon_is_a_syntax_error() {
    let err = parse(";foo").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn lex_errors_propagate_through_parse() {
    let err = parse("'unterminated").unwrap_err();
    assert!(matches!(err, ParseError::Lex(_)));
}

// ---- integration-style: everything at once ------------------------------------

#[test]
fn realistic_combined_input() {
    let list = parse_ok("FOO=bar cmd1 arg1 2>err.log | cmd2 && cmd3 || cmd4; cmd5 &");
    assert_eq!(list.items.len(), 2);

    let first = &list.items[0];
    assert_eq!(first.separator, Separator::Sequential);
    assert_eq!(first.and_or.rest.len(), 2);
    assert_eq!(first.and_or.rest[0].0, LogicalOp::And);
    assert_eq!(first.and_or.rest[1].0, LogicalOp::Or);
    assert_eq!(first.and_or.first.commands.len(), 2);
    let Command::Simple(cmd1) = &first.and_or.first.commands[0] else {
        unreachable!("expected a Command::Simple")
    };
    assert_eq!(
        cmd1.assignments,
        vec![Assignment {
            name: "FOO".to_string(),
            value: plain_word("bar"),
        }]
    );
    assert_eq!(cmd1.name, Some(plain_word("cmd1")));
    assert_eq!(cmd1.args, vec![plain_word("arg1")]);
    assert_eq!(
        cmd1.redirects,
        vec![Redirect {
            fd: Some(2),
            operator: RedirectOperator::Output,
            target: plain_word("err.log"),
        }]
    );

    let second = &list.items[1];
    assert_eq!(second.separator, Separator::Async);
    let Command::Simple(cmd5) = &second.and_or.first.commands[0] else {
        unreachable!("expected a Command::Simple")
    };
    assert_eq!(cmd5.name, Some(plain_word("cmd5")));
}

// ---- compound commands: if/elif/else ----------------------------------------

#[test]
fn if_then_fi_no_else() {
    let cmd = parse_one_compound_command("if true; then echo yes; fi");
    let CompoundCommandKind::If(clause) = cmd.kind else {
        unreachable!("expected If")
    };
    assert_eq!(clause.branches.len(), 1);
    assert_eq!(only_command_name(&clause.branches[0].0), "true");
    assert_eq!(only_command_name(&clause.branches[0].1), "echo");
    assert_eq!(clause.else_branch, None);
}

#[test]
fn if_then_else_fi() {
    let cmd = parse_one_compound_command("if false; then echo a; else echo b; fi");
    let CompoundCommandKind::If(clause) = cmd.kind else {
        unreachable!("expected If")
    };
    assert_eq!(clause.branches.len(), 1);
    assert!(clause.else_branch.is_some());
    assert_eq!(
        only_command_name(clause.else_branch.as_ref().unwrap()),
        "echo"
    );
}

#[test]
fn if_elif_elif_else_fi_flattens_elif_chain() {
    let cmd =
        parse_one_compound_command("if a; then b; elif c; then d; elif e; then f; else g; fi");
    let CompoundCommandKind::If(clause) = cmd.kind else {
        unreachable!("expected If")
    };
    // 1 leading `if` + 2 `elif`s = 3 branches, flattened into one Vec
    // rather than nested else_parts.
    assert_eq!(clause.branches.len(), 3);
    assert_eq!(only_command_name(&clause.branches[0].0), "a");
    assert_eq!(only_command_name(&clause.branches[1].0), "c");
    assert_eq!(only_command_name(&clause.branches[2].0), "e");
    assert!(clause.else_branch.is_some());
}

#[test]
fn if_clause_accepts_trailing_redirect() {
    let cmd = parse_one_compound_command("if true; then echo hi; fi > out.log");
    assert_eq!(
        cmd.redirects,
        vec![Redirect {
            fd: None,
            operator: RedirectOperator::Output,
            target: plain_word("out.log"),
        }]
    );
}

#[test]
fn unclosed_if_is_a_syntax_error() {
    let err = parse("if true; then echo hi").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedEof { .. }));
}

#[test]
fn bare_fi_at_command_start_is_a_syntax_error() {
    let err = parse("fi").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

// ---- compound commands: reserved-word position sensitivity ------------------

#[test]
fn reserved_word_as_plain_argument_is_not_reinterpreted() {
    // `if` here is just an ordinary argument to `echo` -- it's not in
    // command-start position, so it must never be treated as the
    // reserved word. Confirmed against real bash: `echo if` prints `if`.
    let cmd = parse_one_simple_command("echo if");
    assert_eq!(cmd.name, Some(plain_word("echo")));
    assert_eq!(cmd.args, vec![plain_word("if")]);
}

#[test]
fn reserved_word_glued_to_a_closing_command_without_separator_stays_an_argument() {
    // Confirmed against real bash: `if true; then echo hi fi; fi` prints
    // `hi fi` -- the un-separated `fi` right after `hi` is a second
    // argument to `echo`, not the clause's closing `fi` (only the
    // *second*, properly separated `fi` closes it).
    let cmd = parse_one_compound_command("if true; then echo hi fi; fi");
    let CompoundCommandKind::If(clause) = cmd.kind else {
        unreachable!("expected If")
    };
    let body = &clause.branches[0].1;
    assert_eq!(body.items.len(), 1);
    let Command::Simple(echo) = &body.items[0].and_or.first.commands[0] else {
        unreachable!("expected Command::Simple")
    };
    assert_eq!(echo.args, plain_words(&["hi", "fi"]));
}

#[test]
fn quoted_reserved_word_is_never_recognized() {
    // POSIX 2.10.2 rule 1: quoting removes a reserved word's special
    // meaning. `'if'` as a command name should just try (and fail) to
    // run a program literally named "if", not be parsed as `if_clause`.
    let cmd = parse_one_simple_command("'if' true");
    assert_eq!(
        cmd.name,
        Some(Word::new(vec![WordSegment::SingleQuoted("if".into())]))
    );
}

// ---- compound commands: while/until ------------------------------------------

#[test]
fn while_do_done() {
    let cmd = parse_one_compound_command("while true; do echo hi; done");
    let CompoundCommandKind::While(clause) = cmd.kind else {
        unreachable!("expected While")
    };
    assert_eq!(only_command_name(&clause.condition), "true");
    assert_eq!(only_command_name(&clause.body), "echo");
}

#[test]
fn until_do_done() {
    let cmd = parse_one_compound_command("until false; do echo hi; done");
    let CompoundCommandKind::Until(clause) = cmd.kind else {
        unreachable!("expected Until")
    };
    assert_eq!(only_command_name(&clause.condition), "false");
    assert_eq!(only_command_name(&clause.body), "echo");
}

#[test]
fn while_condition_without_separator_before_do_is_a_syntax_error() {
    // Confirmed against real bash: `while true do ... done` (no `;` or
    // newline before `do`) is a syntax error -- `do` right after `true`
    // is swallowed as part of the *condition* command, not recognized
    // as the reserved word, leaving no `do` left to find.
    let err = parse("while true do echo hi; done").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedEof { .. } | ParseError::UnexpectedToken { .. }
    ));
}

// ---- compound commands: for ---------------------------------------------------

#[test]
fn for_in_wordlist() {
    let cmd = parse_one_compound_command("for x in a b c; do echo $x; done");
    let CompoundCommandKind::For(clause) = cmd.kind else {
        unreachable!("expected For")
    };
    assert_eq!(clause.name, "x");
    assert_eq!(clause.words, Some(plain_words(&["a", "b", "c"])));
    assert_eq!(only_command_name(&clause.body), "echo");
}

#[test]
fn for_in_with_empty_wordlist() {
    let cmd = parse_one_compound_command("for x in; do echo $x; done");
    let CompoundCommandKind::For(clause) = cmd.kind else {
        unreachable!("expected For")
    };
    assert_eq!(clause.words, Some(vec![]));
}

#[test]
fn for_without_in_clause_is_words_none() {
    let cmd = parse_one_compound_command("for x; do echo $x; done");
    let CompoundCommandKind::For(clause) = cmd.kind else {
        unreachable!("expected For")
    };
    assert_eq!(clause.name, "x");
    assert_eq!(clause.words, None);
}

#[test]
fn for_without_in_clause_and_without_separator_before_do() {
    // Confirmed against real bash: `for x do echo $x; done` (no `in`
    // clause *and* no `;`/newline before `do`) is valid -- right after
    // `name`, only a reserved word (`in` or `do`) can be next, so `do`
    // is recognized immediately either way.
    let cmd = parse_one_compound_command("for x do echo $x; done");
    let CompoundCommandKind::For(clause) = cmd.kind else {
        unreachable!("expected For")
    };
    assert_eq!(clause.words, None);
}

#[test]
fn for_in_wordlist_without_separator_before_do_is_a_syntax_error() {
    // Confirmed against real bash: `for x in a b do ...; done` (an `in`
    // wordlist, but no `;`/newline before `do`) is a syntax error -- the
    // un-separated `do` is swallowed as a fourth wordlist item.
    let err = parse("for x in a b do echo $x; done").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedEof { .. } | ParseError::UnexpectedToken { .. }
    ));
}

#[test]
fn for_invalid_name_is_a_syntax_error() {
    let err = parse("for 1x in a; do echo $1x; done").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

// ---- compound commands: case/esac --------------------------------------------

#[test]
fn case_multiple_arms_and_patterns_per_arm() {
    let cmd =
        parse_one_compound_command("case $x in a|b) echo ab ;; c) echo c ;; *) echo other ;; esac");
    let CompoundCommandKind::Case(clause) = cmd.kind else {
        unreachable!("expected Case")
    };
    assert_eq!(clause.arms.len(), 3);
    assert_eq!(clause.arms[0].patterns, plain_words(&["a", "b"]));
    assert_eq!(only_command_name(&clause.arms[0].body), "echo");
    assert_eq!(clause.arms[0].terminator, CaseTerminator::Break);
    assert_eq!(clause.arms[1].patterns, plain_words(&["c"]));
    assert_eq!(clause.arms[2].patterns, plain_words(&["*"]));
}

#[test]
fn case_last_arm_may_omit_double_semicolon() {
    // Confirmed against real bash: a `;` (or newline) is still required
    // before `esac` even when the terminator itself is omitted --
    // `... b) echo b esac` (no separator at all) is a syntax error
    // (`esac` gets swallowed as a second argument to `echo`, same
    // reserved-word-position rule as `fi`/`done`); `... b) echo b; esac`
    // is what's actually valid.
    let cmd = parse_one_compound_command("case $x in a) echo a ;; b) echo b; esac");
    let CompoundCommandKind::Case(clause) = cmd.kind else {
        unreachable!("expected Case")
    };
    assert_eq!(clause.arms.len(), 2);
    assert_eq!(clause.arms[0].terminator, CaseTerminator::Break);
    assert_eq!(clause.arms[1].terminator, CaseTerminator::None);
}

#[test]
fn case_last_arm_without_separator_before_esac_is_a_syntax_error() {
    let err = parse("case $x in a) echo a ;; b) echo b esac").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedEof { .. } | ParseError::UnexpectedToken { .. }
    ));
}

#[test]
fn case_arm_with_empty_body() {
    let cmd = parse_one_compound_command("case $x in a) ;; esac");
    let CompoundCommandKind::Case(clause) = cmd.kind else {
        unreachable!("expected Case")
    };
    assert_eq!(clause.arms.len(), 1);
    assert_eq!(clause.arms[0].body.items.len(), 0);
}

#[test]
fn case_with_no_arms_at_all() {
    let cmd = parse_one_compound_command("case $x in esac");
    let CompoundCommandKind::Case(clause) = cmd.kind else {
        unreachable!("expected Case")
    };
    assert_eq!(clause.arms.len(), 0);
}

#[test]
fn case_arm_pattern_accepts_optional_leading_paren() {
    let cmd = parse_one_compound_command("case $x in (a) echo a ;; esac");
    let CompoundCommandKind::Case(clause) = cmd.kind else {
        unreachable!("expected Case")
    };
    assert_eq!(clause.arms[0].patterns, plain_words(&["a"]));
}

#[test]
fn case_fallthrough_extension_is_a_syntax_error_not_a_silent_misparse() {
    // `;&` is a bash extension this parser deliberately doesn't decode
    // (see CaseTerminator's docs) -- it must be rejected, not silently
    // treated as `;;` or swallowed into the next arm's body.
    let err = parse("case $x in a) echo a ;& b) echo b ;; esac").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedToken { .. } | ParseError::UnexpectedEof { .. }
    ));
}

// ---- compound commands: subshell / brace group -------------------------------

#[test]
fn subshell_parses_as_a_compound_command() {
    let cmd = parse_one_compound_command("(echo hi)");
    let CompoundCommandKind::Subshell(subshell) = cmd.kind else {
        unreachable!("expected Subshell")
    };
    assert_eq!(only_command_name(&subshell.body), "echo");
    // The raw source text is what conch-shell-core::exec actually runs
    // (real fork semantics) -- see SubshellBody's docs.
    assert_eq!(subshell.source, "echo hi");
}

#[test]
fn subshell_with_pipeline_and_redirect() {
    let cmd = parse_one_compound_command("(echo hi; echo bye) > out.log");
    let CompoundCommandKind::Subshell(subshell) = cmd.kind else {
        unreachable!("expected Subshell")
    };
    assert_eq!(subshell.body.items.len(), 2);
    assert_eq!(subshell.source, "echo hi; echo bye");
    assert_eq!(
        cmd.redirects,
        vec![Redirect {
            fd: None,
            operator: RedirectOperator::Output,
            target: plain_word("out.log"),
        }]
    );
}

#[test]
fn subshell_source_excludes_the_parens_and_preserves_nested_ones() {
    let cmd = parse_one_compound_command("( (echo hi) )");
    let CompoundCommandKind::Subshell(subshell) = cmd.kind else {
        unreachable!("expected Subshell")
    };
    // Byte-exact slice of the original source between the outer parens
    // -- includes the surrounding whitespace verbatim (harmless once
    // handed to `conch -c`, which ignores leading/trailing blanks) and
    // preserves the inner subshell's own parens untouched.
    assert_eq!(subshell.source, " (echo hi) ");
}

#[test]
fn brace_group_parses_as_a_compound_command() {
    let cmd = parse_one_compound_command("{ echo hi; }");
    let CompoundCommandKind::BraceGroup(body) = cmd.kind else {
        unreachable!("expected BraceGroup")
    };
    assert_eq!(only_command_name(&body), "echo");
}

#[test]
fn brace_group_closing_brace_needs_a_preceding_separator() {
    // Confirmed against real bash: `{ echo hi }` (no `;` before `}`) is
    // a syntax error -- `}` glued onto the argument list is just
    // another argument, not the closing brace.
    let err = parse("{ echo hi }").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedEof { .. } | ParseError::UnexpectedToken { .. }
    ));
}

#[test]
fn brace_group_closing_brace_without_a_space_after_semicolon_is_fine() {
    let cmd = parse_one_compound_command("{ echo hi;}");
    let CompoundCommandKind::BraceGroup(body) = cmd.kind else {
        unreachable!("expected BraceGroup")
    };
    assert_eq!(only_command_name(&body), "echo");
}

// ---- compound commands: nesting and pipelines ---------------------------------

#[test]
fn compound_command_can_be_a_pipeline_stage() {
    let list = parse_ok("if true; then echo hi; fi | cat");
    assert_eq!(list.items[0].and_or.first.commands.len(), 2);
    assert!(matches!(
        list.items[0].and_or.first.commands[0],
        Command::Compound(_)
    ));
}

#[test]
fn nested_compound_commands() {
    let cmd = parse_one_compound_command("while true; do if true; then echo hi; fi; done");
    let CompoundCommandKind::While(clause) = cmd.kind else {
        unreachable!("expected While")
    };
    assert_eq!(clause.body.items.len(), 1);
    assert!(matches!(
        clause.body.items[0].and_or.first.commands[0],
        Command::Compound(_)
    ));
}

#[test]
fn for_loop_with_no_in_clause_and_nested_if() {
    let cmd = parse_one_compound_command("for x; do if true; then echo $x; fi; done");
    let CompoundCommandKind::For(clause) = cmd.kind else {
        unreachable!("expected For")
    };
    assert_eq!(clause.words, None);
    assert_eq!(clause.body.items.len(), 1);
}

// ---- function definitions ------------------------------------------------

#[test]
fn posix_function_definition_with_brace_group_body() {
    let func = parse_one_function_definition("foo() { echo hi; }");
    assert_eq!(func.name, "foo");
    let CompoundCommandKind::BraceGroup(body) = func.body.kind else {
        unreachable!("expected BraceGroup")
    };
    assert_eq!(only_command_name(&body), "echo");
}

#[test]
fn posix_function_definition_tolerates_blanks_around_parens() {
    // Confirmed against real bash: blanks anywhere around `fname()`'s
    // parens are fine -- and fall out for free here since whitespace was
    // never tokenized to begin with.
    let func = parse_one_function_definition("foo ( ) { echo hi; }");
    assert_eq!(func.name, "foo");
    assert!(matches!(func.body.kind, CompoundCommandKind::BraceGroup(_)));
}

#[test]
fn posix_function_definition_allows_a_newline_before_the_body() {
    let func = parse_one_function_definition("foo()\n{ echo hi; }");
    assert_eq!(func.name, "foo");
    assert!(matches!(func.body.kind, CompoundCommandKind::BraceGroup(_)));
}

#[test]
fn posix_function_definition_body_can_be_any_compound_command() {
    // POSIX `function_body` permits any compound command, not just a
    // brace group -- confirmed against real bash: `foo() (echo hi)` runs
    // the body in a subshell every time `foo` is called.
    let func = parse_one_function_definition("foo() (echo hi)");
    let CompoundCommandKind::Subshell(subshell) = func.body.kind else {
        unreachable!("expected Subshell")
    };
    assert_eq!(subshell.source, "echo hi");
}

#[test]
fn posix_function_definition_invalid_name_is_a_syntax_error() {
    let err = parse("1foo() { echo hi; }").unwrap_err();
    assert!(matches!(err, ParseError::UnexpectedToken { .. }));
}

#[test]
fn function_keyword_without_parens() {
    // bash extension: the `function` keyword makes the `()` optional.
    let func = parse_one_function_definition("function foo { echo hi; }");
    assert_eq!(func.name, "foo");
    assert!(matches!(func.body.kind, CompoundCommandKind::BraceGroup(_)));
}

#[test]
fn function_keyword_with_parens() {
    let func = parse_one_function_definition("function foo() { echo hi; }");
    assert_eq!(func.name, "foo");
    assert!(matches!(func.body.kind, CompoundCommandKind::BraceGroup(_)));
}

#[test]
fn function_keyword_body_can_be_any_compound_command() {
    let func = parse_one_function_definition("function foo while true; do break; done");
    assert_eq!(func.name, "foo");
    assert!(matches!(func.body.kind, CompoundCommandKind::While(_)));
}

#[test]
fn function_definition_can_be_followed_by_a_redirect() {
    let func = parse_one_function_definition("foo() { echo hi; } > out.log");
    assert_eq!(
        func.body.redirects,
        vec![Redirect {
            fd: None,
            operator: RedirectOperator::Output,
            target: plain_word("out.log"),
        }]
    );
}

#[test]
fn ordinary_command_name_glued_to_parens_without_an_immediate_close_paren_is_not_a_function() {
    // `at_posix_function_definition` requires `Word '(' ')'` with nothing
    // between the parens -- `foo(bar)` isn't that shape (there's a `bar`
    // between them), so this must NOT be parsed as a function
    // definition. It also isn't valid as an ordinary simple command
    // (bash agrees: this is a syntax error), so the only assertion that
    // matters here is that the parser doesn't misinterpret `(bar)` as
    // part of a function definition it silently accepts.
    let err = parse("foo(bar)").unwrap_err();
    assert!(matches!(
        err,
        ParseError::UnexpectedToken { .. } | ParseError::UnexpectedEof { .. }
    ));
}

#[test]
fn ordinary_command_is_not_misparsed_as_a_function_definition() {
    let cmd = parse_one_simple_command("echo hi");
    assert_eq!(cmd.name, Some(plain_word("echo")));
}

#[test]
fn function_definition_can_be_a_pipeline_stage_target() {
    // A function *definition* itself isn't piped into anything
    // meaningful in real shells either, but it must still be parseable
    // as one pipeline stage without the parser choking -- confirming
    // `parse_command`'s function-definition dispatch composes with the
    // rest of the grammar around it.
    let list = parse_ok("foo() { echo hi; }; foo");
    assert_eq!(list.items.len(), 2);
    assert!(matches!(
        list.items[0].and_or.first.commands[0],
        Command::Function(_)
    ));
    assert!(matches!(
        list.items[1].and_or.first.commands[0],
        Command::Simple(_)
    ));
}
