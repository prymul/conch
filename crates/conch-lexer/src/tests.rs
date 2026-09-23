use super::*;

/// Lexes `input`, panicking (with the input included) on a `LexError` —
/// convenience for tests that expect success.
fn lex_ok(input: &str) -> Vec<Token> {
    lex(input).unwrap_or_else(|e| panic!("expected {input:?} to lex, got error: {e}"))
}

fn word_kinds(tokens: &[Token]) -> Vec<&TokenKind> {
    tokens.iter().map(|t| &t.kind).collect()
}

/// Extracts the single `Word`'s segments from a one-token lex result.
fn only_word_segments(input: &str) -> Vec<WordSegment> {
    let tokens = lex_ok(input);
    match tokens.as_slice() {
        [
            Token {
                kind: TokenKind::Word(word),
                ..
            },
        ] => word.segments.clone(),
        other => panic!("expected exactly one Word token for {input:?}, got {other:?}"),
    }
}

// ---- words / blanks --------------------------------------------------

#[test]
fn words_split_on_blanks() {
    let tokens = lex_ok("echo  hello\tworld");
    let words: Vec<&str> = tokens
        .iter()
        .map(|t| match &t.kind {
            TokenKind::Word(w) => w.as_plain_literal().unwrap(),
            other => panic!("expected Word, got {other:?}"),
        })
        .collect();
    assert_eq!(words, vec!["echo", "hello", "world"]);
}

#[test]
fn newline_is_a_significant_token() {
    let tokens = lex_ok("a\nb");
    assert_eq!(
        word_kinds(&tokens),
        vec![
            &TokenKind::Word(Word::new(vec![WordSegment::Literal("a".into())])),
            &TokenKind::Newline,
            &TokenKind::Word(Word::new(vec![WordSegment::Literal("b".into())])),
        ]
    );
}

// ---- quoting -----------------------------------------------------------

#[test]
fn single_quotes_are_fully_literal() {
    let segments = only_word_segments("'$foo * bar; | not special'");
    assert_eq!(
        segments,
        vec![WordSegment::SingleQuoted(
            "$foo * bar; | not special".into()
        )]
    );
}

#[test]
fn single_quote_does_not_admit_backslash_escapes() {
    // Inside '...', a backslash has no special meaning at all — the
    // quote only ends at the very next `'`.
    let segments = only_word_segments(r"'a\'");
    assert_eq!(segments, vec![WordSegment::SingleQuoted(r"a\".into())]);
}

#[test]
fn quoted_operator_characters_do_not_tokenize_as_operators() {
    // A ';' inside single quotes must not become an Operator token.
    let tokens = lex_ok("'a;b'");
    assert_eq!(tokens.len(), 1);
    assert!(matches!(tokens[0].kind, TokenKind::Word(_)));
}

#[test]
fn double_quotes_allow_expansion_sites() {
    let segments = only_word_segments(r#""hello $name and $(cmd) and $((1+1))""#);
    assert_eq!(
        segments,
        vec![WordSegment::DoubleQuoted(vec![
            WordSegment::Literal("hello ".into()),
            WordSegment::Parameter(Parameter::Name("name".into())),
            WordSegment::Literal(" and ".into()),
            WordSegment::CommandSubstitution(CommandSubstitution {
                style: SubstitutionStyle::DollarParen,
                body: "cmd".into(),
            }),
            WordSegment::Literal(" and ".into()),
            WordSegment::ArithmeticExpansion("1+1".into()),
        ])]
    );
}

#[test]
fn double_quote_backslash_only_escapes_the_special_set() {
    // Confirmed against real bash: echo "a\qb" -> a\qb (backslash kept).
    let segments = only_word_segments(r#""a\qb""#);
    assert_eq!(
        segments,
        vec![WordSegment::DoubleQuoted(vec![WordSegment::Literal(
            r"a\qb".into()
        )])]
    );
}

#[test]
fn double_quote_backslash_n_stays_two_literal_characters() {
    // Confirmed against real bash: x=hi; echo "\n$x" -> \nhi
    let segments = only_word_segments(r#""\n$x""#);
    assert_eq!(
        segments,
        vec![WordSegment::DoubleQuoted(vec![
            WordSegment::Literal(r"\n".into()),
            WordSegment::Parameter(Parameter::Name("x".into())),
        ])]
    );
}

#[test]
fn double_quote_backslash_dollar_is_escaped_to_literal_dollar() {
    let segments = only_word_segments(r#""\$foo""#);
    assert_eq!(
        segments,
        vec![WordSegment::DoubleQuoted(vec![WordSegment::Literal(
            "$foo".into()
        )])]
    );
}

#[test]
fn double_quote_single_quote_has_no_special_meaning() {
    let segments = only_word_segments(r#""it's fine""#);
    assert_eq!(
        segments,
        vec![WordSegment::DoubleQuoted(vec![WordSegment::Literal(
            "it's fine".into()
        )])]
    );
}

#[test]
fn unquoted_backslash_escapes_exactly_the_next_character() {
    let segments = only_word_segments(r"\$foo");
    assert_eq!(segments, vec![WordSegment::Literal("$foo".into())]);
}

#[test]
fn unquoted_backslash_before_operator_char_makes_it_literal() {
    let segments = only_word_segments(r"a\;b");
    assert_eq!(segments, vec![WordSegment::Literal("a;b".into())]);
    // And the whole thing must lex as a single Word, not a Word + Semi.
    let tokens = lex_ok(r"a\;b");
    assert_eq!(tokens.len(), 1);
}

#[test]
fn backslash_newline_line_continuation_does_not_split_the_word() {
    let segments = only_word_segments("ab\\\ncd");
    assert_eq!(segments, vec![WordSegment::Literal("abcd".into())]);
}

#[test]
fn double_quoted_backslash_newline_line_continuation_is_removed() {
    let segments = only_word_segments("\"ab\\\ncd\"");
    assert_eq!(
        segments,
        vec![WordSegment::DoubleQuoted(vec![WordSegment::Literal(
            "abcd".into()
        )])]
    );
}

// ---- comments ------------------------------------------------------------

#[test]
fn hash_mid_word_is_literal() {
    let segments = only_word_segments("a#b");
    assert_eq!(segments, vec![WordSegment::Literal("a#b".into())]);
}

#[test]
fn hash_after_blank_starts_a_comment_to_end_of_line() {
    let tokens = lex_ok("echo a #comment here\nnext");
    let words: Vec<&str> = tokens
        .iter()
        .filter_map(|t| match &t.kind {
            TokenKind::Word(w) => w.as_plain_literal(),
            _ => None,
        })
        .collect();
    assert_eq!(words, vec!["echo", "a", "next"]);
    assert!(tokens.iter().any(|t| matches!(t.kind, TokenKind::Newline)));
}

// ---- operators -------------------------------------------------------------

#[test]
fn operators_are_recognized_by_longest_match() {
    let tokens = lex_ok("a&&b||c|d;e&f");
    let ops: Vec<Operator> = tokens
        .iter()
        .filter_map(|t| match t.kind {
            TokenKind::Operator(op) => Some(op),
            _ => None,
        })
        .collect();
    assert_eq!(
        ops,
        vec![
            Operator::AndIf,
            Operator::OrIf,
            Operator::Pipe,
            Operator::Semi,
            Operator::Amp,
        ]
    );
}

#[test]
fn full_posix_operator_set_is_recognized() {
    // Every operator this crate documents supporting lexically, even
    // though Phase 1's parser only accepts a subset — see crate docs.
    let cases: &[(&str, Operator)] = &[
        ("|", Operator::Pipe),
        ("||", Operator::OrIf),
        ("&", Operator::Amp),
        ("&&", Operator::AndIf),
        (";", Operator::Semi),
        (";;", Operator::DSemi),
        ("<", Operator::Less),
        ("<<", Operator::DLess),
        ("<<-", Operator::DLessDash),
        ("<&", Operator::LessAnd),
        ("<>", Operator::LessGreat),
        (">", Operator::Great),
        (">>", Operator::DGreat),
        (">&", Operator::GreatAnd),
        (">|", Operator::Clobber),
        ("(", Operator::LParen),
        (")", Operator::RParen),
    ];
    for (text, expected) in cases {
        let tokens = lex_ok(text);
        assert_eq!(
            tokens,
            vec![Token::new(
                TokenKind::Operator(*expected),
                Span::new(0, text.len())
            )],
            "lexing {text:?}",
        );
    }
}

// ---- IO_NUMBER -------------------------------------------------------------

#[test]
fn digit_run_immediately_before_redirect_is_io_number() {
    let tokens = lex_ok("2>file");
    assert_eq!(
        word_kinds(&tokens),
        vec![
            &TokenKind::IoNumber(2),
            &TokenKind::Operator(Operator::Great),
            &TokenKind::Word(Word::new(vec![WordSegment::Literal("file".into())])),
        ]
    );
}

#[test]
fn digit_run_not_at_word_start_is_not_io_number() {
    let tokens = lex_ok("a2>file");
    assert_eq!(
        word_kinds(&tokens),
        vec![
            &TokenKind::Word(Word::new(vec![WordSegment::Literal("a2".into())])),
            &TokenKind::Operator(Operator::Great),
            &TokenKind::Word(Word::new(vec![WordSegment::Literal("file".into())])),
        ]
    );
}

#[test]
fn digit_run_not_followed_by_redirect_is_a_plain_word() {
    let segments = only_word_segments("123");
    assert_eq!(segments, vec![WordSegment::Literal("123".into())]);
}

// ---- parameter expansion --------------------------------------------------

#[test]
fn simple_name_parameter_unbraced_and_braced() {
    assert_eq!(
        only_word_segments("$foo"),
        vec![WordSegment::Parameter(Parameter::Name("foo".into()))]
    );
    assert_eq!(
        only_word_segments("${foo}"),
        vec![WordSegment::Parameter(Parameter::Name("foo".into()))]
    );
}

#[test]
fn unbraced_positional_parameter_consumes_exactly_one_digit() {
    assert_eq!(
        only_word_segments("$12"),
        vec![
            WordSegment::Parameter(Parameter::Positional(1)),
            WordSegment::Literal("2".into()),
        ]
    );
}

#[test]
fn braced_positional_parameter_consumes_the_whole_digit_run() {
    assert_eq!(
        only_word_segments("${12}"),
        vec![WordSegment::Parameter(Parameter::Positional(12))]
    );
}

#[test]
fn all_special_parameters() {
    let cases: &[(&str, SpecialParameter)] = &[
        ("$@", SpecialParameter::At),
        ("$*", SpecialParameter::Star),
        ("$#", SpecialParameter::Hash),
        ("$?", SpecialParameter::Question),
        ("$-", SpecialParameter::Dash),
        ("$$", SpecialParameter::Dollar),
        ("$!", SpecialParameter::Bang),
        ("$0", SpecialParameter::Zero),
    ];
    for (text, expected) in cases {
        assert_eq!(
            only_word_segments(text),
            vec![WordSegment::Parameter(Parameter::Special(*expected))],
            "lexing {text:?}",
        );
    }
}

#[test]
fn braced_hash_alone_is_the_special_parameter_not_length_operator() {
    // ${#} means "$#" (count of positional params); ${#VAR} (length-of)
    // is the *different*, more-than-a-bare-name case handled below.
    assert_eq!(
        only_word_segments("${#}"),
        vec![WordSegment::Parameter(Parameter::Special(
            SpecialParameter::Hash
        ))]
    );
}

#[test]
fn dollar_not_followed_by_valid_parameter_syntax_is_a_literal_dollar() {
    assert_eq!(
        only_word_segments("$"),
        vec![WordSegment::Literal("$".into())]
    );
    assert_eq!(
        only_word_segments("$.foo"),
        vec![WordSegment::Literal("$.foo".into())]
    );
}

#[test]
fn complex_parameter_expansion_is_captured_raw_and_not_decoded() {
    assert_eq!(
        only_word_segments("${foo:-default}"),
        vec![WordSegment::ComplexParameterExpansion(
            "foo:-default".into()
        )]
    );
    assert_eq!(
        only_word_segments("${#foo}"),
        vec![WordSegment::ComplexParameterExpansion("#foo".into())]
    );
    assert_eq!(
        only_word_segments("${foo#pattern}"),
        vec![WordSegment::ComplexParameterExpansion("foo#pattern".into())]
    );
}

#[test]
fn complex_parameter_expansion_body_can_contain_nested_expansions() {
    assert_eq!(
        only_word_segments("${foo:-$(bar)}"),
        vec![WordSegment::ComplexParameterExpansion("foo:-$(bar)".into())]
    );
}

// ---- command substitution --------------------------------------------------

#[test]
fn dollar_paren_command_substitution_body_is_raw() {
    assert_eq!(
        only_word_segments("$(echo hi)"),
        vec![WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::DollarParen,
            body: "echo hi".into(),
        })]
    );
}

#[test]
fn empty_command_substitution() {
    assert_eq!(
        only_word_segments("$()"),
        vec![WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::DollarParen,
            body: String::new(),
        })]
    );
}

#[test]
fn backtick_command_substitution_body_is_raw() {
    assert_eq!(
        only_word_segments("`echo hi`"),
        vec![WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::Backtick,
            body: "echo hi".into(),
        })]
    );
}

#[test]
fn nested_bare_parens_in_command_substitution_are_depth_balanced() {
    // Confirmed against real bash: a genuine subshell nested inside
    // $(...) uses bare parens that must be depth-counted, not just
    // matched against the first ')'.
    assert_eq!(
        only_word_segments("$(echo a; (echo b); echo c)"),
        vec![WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::DollarParen,
            body: "echo a; (echo b); echo c".into(),
        })]
    );
}

#[test]
fn nested_double_quotes_in_command_substitution_are_not_mistaken_for_the_outer_close() {
    // Confirmed against real bash: echo "outer $(echo "a)b") end"
    // prints `outer a)b end` — the inner ')' and the inner '"' pair must
    // not affect the outer double-quote or command-substitution boundary.
    let segments = only_word_segments(r#""outer $(echo "a)b") end""#);
    assert_eq!(
        segments,
        vec![WordSegment::DoubleQuoted(vec![
            WordSegment::Literal("outer ".into()),
            WordSegment::CommandSubstitution(CommandSubstitution {
                style: SubstitutionStyle::DollarParen,
                body: r#"echo "a)b""#.into(),
            }),
            WordSegment::Literal(" end".into()),
        ])]
    );
}

#[test]
fn close_paren_inside_nested_parameter_expansion_does_not_end_command_substitution() {
    // Confirmed against real bash: echo $(echo ${x:-)} end) with x unset
    // treats the whole `${x:-)}` as one atomic nested construct, so the
    // command substitution's own ')' is the *last* one.
    assert_eq!(
        only_word_segments("$(echo ${x:-)} end)"),
        vec![WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::DollarParen,
            body: "echo ${x:-)} end".into(),
        })]
    );
}

// ---- arithmetic expansion --------------------------------------------------

#[test]
fn arithmetic_expansion_body_is_raw() {
    assert_eq!(
        only_word_segments("$((1+2))"),
        vec![WordSegment::ArithmeticExpansion("1+2".into())]
    );
}

#[test]
fn arithmetic_expansion_has_precedence_over_nested_subshell_reading() {
    // POSIX 2.6.4: "Arithmetic expansion has precedence" — $((foo)) is
    // always read as arithmetic (grouped `(foo)`), never as a command
    // substitution containing a subshell running `(foo)`. An explicit
    // space, `$( (foo) )`, is how POSIX says to write the latter.
    assert_eq!(
        only_word_segments("$((foo))"),
        vec![WordSegment::ArithmeticExpansion("foo".into())]
    );
    assert_eq!(
        only_word_segments("$( (foo) )"),
        vec![WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::DollarParen,
            body: " (foo) ".into(),
        })]
    );
}

#[test]
fn nested_parens_in_arithmetic_expansion_are_depth_balanced() {
    assert_eq!(
        only_word_segments("$(( (1+2) * 3 ))"),
        vec![WordSegment::ArithmeticExpansion(" (1+2) * 3 ".into())]
    );
}

// ---- assignment-shaped words stay plain words at the lexer level ---------

#[test]
fn assignment_shaped_word_is_not_specially_tokenized_by_the_lexer() {
    // Whether `FOO=bar` is an assignment is a *parser*-level, positional
    // concern (POSIX only allows ASSIGNMENT_WORD in cmd_prefix); the
    // lexer just produces an ordinary Word either way.
    assert_eq!(
        only_word_segments("FOO=bar"),
        vec![WordSegment::Literal("FOO=bar".into())]
    );
}

// ---- bare-brace-not-depth-tracked -----------------------------------------

#[test]
fn bare_brace_is_not_depth_tracked_inside_parameter_expansion() {
    // Confirmed against real bash: y=Z; echo "${y:-}}" prints `Z}` — the
    // expansion closes at the *first* unescaped '}', and the second '}'
    // is just a literal character after it.
    assert_eq!(
        only_word_segments("${y:-}}"),
        vec![
            WordSegment::ComplexParameterExpansion("y:-".into()),
            WordSegment::Literal("}".into()),
        ]
    );
}

// ---- Phase 2 re-lexing entry points ----------------------------------------

#[test]
fn lex_word_body_runs_to_eof_ignoring_blanks_and_operators() {
    // Unlike scan_word (and thus `lex`), lex_word_body must NOT stop at a
    // blank or an operator character — those are ordinary content in a
    // `${var:-word}` operand.
    assert_eq!(
        lex_word_body("a b|c").unwrap(),
        Word::new(vec![WordSegment::Literal("a b|c".into())])
    );
}

#[test]
fn lex_word_body_recognizes_quotes_and_expansions() {
    assert_eq!(
        lex_word_body("'a b' \"$x\" $y").unwrap(),
        Word::new(vec![
            WordSegment::SingleQuoted("a b".into()),
            WordSegment::Literal(" ".into()),
            WordSegment::DoubleQuoted(vec![WordSegment::Parameter(Parameter::Name("x".into()))]),
            WordSegment::Literal(" ".into()),
            WordSegment::Parameter(Parameter::Name("y".into())),
        ])
    );
}

#[test]
fn lex_word_body_matches_unquoted_ctxt_param_expansion_operand_bash_behavior() {
    // Confirmed against real bash: `${x:-'a b' c}` unquoted, x unset,
    // keeps 'a b' as real single-quoting (later split from `c` only on
    // the unquoted space between them).
    assert_eq!(
        lex_word_body("'a b' c").unwrap(),
        Word::new(vec![
            WordSegment::SingleQuoted("a b".into()),
            WordSegment::Literal(" c".into()),
        ])
    );
}

#[test]
fn lex_double_quoted_body_treats_single_quote_as_literal() {
    // Confirmed against real bash: echo "${x:-'a b'}" (x unset) prints
    // `'a b'` — the quote characters survive literally, unlike the
    // unquoted-context case (lex_word_body, above).
    assert_eq!(
        lex_double_quoted_body("'a b'").unwrap(),
        vec![WordSegment::Literal("'a b'".into())]
    );
}

#[test]
fn lex_double_quoted_body_still_allows_nested_double_quotes() {
    // Confirmed against real bash: echo "${x:-"y"}" (x unset) prints
    // `y`, not `"y"` — a nested "..." still really quotes here (POSIX
    // 2.6.2/2.6.3's recursive tokenizing rules), unlike a nested '...'.
    assert_eq!(
        lex_double_quoted_body("\"y\"").unwrap(),
        vec![WordSegment::DoubleQuoted(vec![WordSegment::Literal(
            "y".into()
        )])]
    );
}

#[test]
fn lex_double_quoted_body_uses_the_double_quote_escape_set() {
    // Confirmed against real bash: echo "${x:-a\ b}" (x unset) prints
    // `a\ b` — backslash-space isn't in the double-quote escape set, so
    // both characters stay literal (unlike lex_word_body, where a
    // backslash escapes any next character unconditionally).
    assert_eq!(
        lex_double_quoted_body(r"a\ b").unwrap(),
        vec![WordSegment::Literal(r"a\ b".into())]
    );
}

#[test]
fn lex_dollar_expansion_recognizes_command_substitution_and_reports_len() {
    let (segment, len) = lex_dollar_expansion("$(cmd) rest").unwrap();
    assert_eq!(
        segment,
        WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::DollarParen,
            body: "cmd".into(),
        })
    );
    assert_eq!(len, "$(cmd)".len());
}

#[test]
fn lex_dollar_expansion_falls_back_to_literal_dollar() {
    let (segment, len) = lex_dollar_expansion("$.rest").unwrap();
    assert_eq!(segment, WordSegment::Literal("$".into()));
    assert_eq!(len, 1);
}

#[test]
#[should_panic(expected = "requires input starting with '$'")]
fn lex_dollar_expansion_panics_without_leading_dollar() {
    let _ = lex_dollar_expansion("no dollar here");
}

#[test]
fn lex_backquote_expansion_recognizes_backtick_substitution_and_reports_len() {
    let (segment, len) = lex_backquote_expansion("`cmd` rest").unwrap();
    assert_eq!(
        segment,
        WordSegment::CommandSubstitution(CommandSubstitution {
            style: SubstitutionStyle::Backtick,
            body: "cmd".into(),
        })
    );
    assert_eq!(len, "`cmd`".len());
}

#[test]
#[should_panic(expected = "requires input starting with '`'")]
fn lex_backquote_expansion_panics_without_leading_backquote() {
    let _ = lex_backquote_expansion("no backquote here");
}

#[test]
fn match_bare_parameter_reports_bytes_consumed_for_length_form_disambiguation() {
    // conch-shell-parser's `${#parameter}` handling relies on this: the
    // length form only applies when the match consumes the *entire*
    // remaining string.
    let (param, len) = match_bare_parameter("x", true).unwrap();
    assert_eq!(param, Parameter::Name("x".into()));
    assert_eq!(len, 1); // consumes all of "x" -> ${#x} is valid length form
    let (param, len) = match_bare_parameter("x#l", true).unwrap();
    assert_eq!(param, Parameter::Name("x".into()));
    assert_eq!(len, 1); // stops before "#l" -> leftover means NOT length form
}

// ---- error cases -----------------------------------------------------------

#[test]
fn unterminated_single_quote_errors() {
    assert_eq!(
        lex("'abc"),
        Err(LexError::UnterminatedSingleQuote { start: 0 })
    );
}

#[test]
fn unterminated_double_quote_errors() {
    assert_eq!(
        lex("\"abc"),
        Err(LexError::UnterminatedDoubleQuote { start: 0 })
    );
}

#[test]
fn unterminated_parameter_expansion_errors() {
    assert_eq!(
        lex("${abc"),
        Err(LexError::UnterminatedParameterExpansion { start: 0 })
    );
}

#[test]
fn unterminated_command_substitution_errors() {
    assert_eq!(
        lex("$(abc"),
        Err(LexError::UnterminatedCommandSubstitution { start: 0 })
    );
}

#[test]
fn unterminated_backquote_errors() {
    assert_eq!(
        lex("`abc"),
        Err(LexError::UnterminatedBackquote { start: 0 })
    );
}

#[test]
fn unterminated_arithmetic_expansion_errors() {
    assert_eq!(
        lex("$((abc"),
        Err(LexError::UnterminatedArithmeticExpansion { start: 0 })
    );
}

#[test]
fn trailing_backslash_errors() {
    assert_eq!(lex("abc\\"), Err(LexError::TrailingBackslash { pos: 3 }));
}

#[test]
fn empty_input_lexes_to_no_tokens() {
    assert_eq!(lex(""), Ok(Vec::new()));
}

#[test]
fn whitespace_only_input_lexes_to_no_tokens() {
    assert_eq!(lex("   \t  "), Ok(Vec::new()));
}

#[test]
fn backslash_escaped_space_stays_one_word() {
    // Confirmed against real bash: `printf '[%s]\n' foo\ bar` prints
    // `[foo bar]` (one argument) — the escaped space must never cause a
    // second Word token, and by the time it's a Literal segment it's
    // indistinguishable from an always-literal space (this crate's own
    // docs: "an unquoted \X has already become literal X"), which is
    // exactly why word-splitting during *expansion* must never re-split
    // plain Literal-segment text, only genuine expansion results.
    let tokens = lex_ok(r"foo\ bar");
    assert_eq!(
        tokens,
        vec![Token::new(
            TokenKind::Word(Word::new(vec![WordSegment::Literal("foo bar".into())])),
            Span::new(0, 8),
        )]
    );
}
