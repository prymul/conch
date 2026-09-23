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
    let Command::Simple(cmd) = &item.and_or.first.commands[0];
    cmd.clone()
}

fn plain_word(text: &str) -> Word {
    Word::new(vec![WordSegment::Literal(text.to_string())])
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
        let Command::Simple(cmd) = cmd;
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
    let Command::Simple(first) = &and_or.first.commands[0];
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
fn subshell_is_a_clear_unsupported_construct_error() {
    let err = parse("(echo hi)").unwrap_err();
    match err {
        ParseError::UnsupportedConstruct { message, .. } => {
            assert!(message.contains("subshell"), "message was: {message}");
            assert!(message.contains("Phase 3"), "message was: {message}");
        }
        other => panic!("expected UnsupportedConstruct, got {other:?}"),
    }
}

#[test]
fn heredoc_is_a_clear_unsupported_construct_error() {
    let err = parse("cat <<EOF").unwrap_err();
    match err {
        ParseError::UnsupportedConstruct { message, .. } => {
            assert!(message.contains("here-document"), "message was: {message}");
            assert!(message.contains("Phase 2"), "message was: {message}");
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
fn trailing_subshell_after_a_valid_command_is_still_reported() {
    let err = parse("a b )").unwrap_err();
    assert!(matches!(err, ParseError::UnsupportedConstruct { .. }));
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
    let Command::Simple(cmd1) = &first.and_or.first.commands[0];
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
    let Command::Simple(cmd5) = &second.and_or.first.commands[0];
    assert_eq!(cmd5.name, Some(plain_word("cmd5")));
}
