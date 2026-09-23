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
