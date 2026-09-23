use super::*;
use conch_shell_lexer::{CommandSubstitution, Parameter, SubstitutionStyle};

fn num(n: i64) -> ArithExpr {
    ArithExpr::Number(n)
}

fn var(name: &str) -> ArithExpr {
    ArithExpr::Variable(name.to_string())
}

fn bin(op: ArithBinaryOp, lhs: ArithExpr, rhs: ArithExpr) -> ArithExpr {
    ArithExpr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn parse(s: &str) -> ArithExpr {
    parse_arithmetic_expr(s).unwrap_or_else(|e| panic!("expected {s:?} to parse, got: {e}"))
}

// ---- numeric literals ---------------------------------------------------

#[test]
fn decimal_literal() {
    assert_eq!(parse("42"), num(42));
}

#[test]
fn octal_literal() {
    // Confirmed against real bash: $((010)) == 8.
    assert_eq!(parse("010"), num(8));
}

#[test]
fn zero_is_not_octal() {
    assert_eq!(parse("0"), num(0));
}

#[test]
fn hex_literal() {
    assert_eq!(parse("0x1F"), num(31));
    assert_eq!(parse("0X1f"), num(31));
}

#[test]
fn hex_literal_with_no_digits_is_zero() {
    // Confirmed against real bash: echo $((0x)) -> 0.
    assert_eq!(parse("0x"), num(0));
}

#[test]
fn arbitrary_base_literal() {
    // Confirmed against real bash: echo $((2#101)) -> 5.
    assert_eq!(parse("2#101"), num(5));
    assert_eq!(parse("16#FF"), num(255));
}

#[test]
fn arbitrary_base_letters_interchangeable_at_or_below_36() {
    // Confirmed against real bash: ${36#z} == ${36#Z} == 35.
    assert_eq!(parse("36#z"), num(35));
    assert_eq!(parse("36#Z"), num(35));
}

#[test]
fn arbitrary_base_letters_distinct_above_36() {
    // Confirmed against real bash: 37#Z errors ("value too great for
    // base") since Z=61 needs base >= 62 once upper/lower diverge.
    assert!(parse_arithmetic_expr("37#Z").is_err());
    // '@' is digit-value 62 (needs base >= 63); '_' is digit-value 63
    // (needs base >= 64) -- confirmed against real bash: 62#@ and 63#_
    // both error ("value too great for base"), while 63#@ == 62 and
    // 64#_ == 63 succeed.
    assert!(parse_arithmetic_expr("62#@").is_err());
    assert_eq!(parse("63#@"), num(62));
    assert!(parse_arithmetic_expr("63#_").is_err());
    assert_eq!(parse("64#_"), num(63));
}

#[test]
fn base_out_of_range_errors() {
    assert!(matches!(
        parse_arithmetic_expr("1#1"),
        Err(ArithError::InvalidBase { .. })
    ));
    assert!(matches!(
        parse_arithmetic_expr("65#1"),
        Err(ArithError::InvalidBase { .. })
    ));
}

#[test]
fn base_marker_with_no_digits_errors() {
    // Confirmed against real bash: echo $((2#)) errors, unlike 0x.
    assert!(parse_arithmetic_expr("2#").is_err());
}

#[test]
fn digit_too_large_for_base_errors() {
    assert!(matches!(
        parse_arithmetic_expr("2#12"),
        Err(ArithError::DigitOutOfRange { .. })
    ));
}

#[test]
fn overflow_wraps_like_unsigned_64_bit_then_reinterprets_as_signed() {
    // Confirmed against real bash: a 20-digit decimal literal wraps.
    assert_eq!(parse("99999999999999999999"), num(7766279631452241919));
    // Exactly u64::MAX reinterpreted as i64 is -1.
    assert_eq!(parse("18446744073709551615"), num(-1));
    assert_eq!(parse("9223372036854775807"), num(i64::MAX));
}

// ---- identifiers ----------------------------------------------------------

#[test]
fn bare_identifier_is_a_variable_reference() {
    assert_eq!(parse("x"), var("x"));
    assert_eq!(parse("_foo123"), var("_foo123"));
}

// ---- precedence / associativity --------------------------------------------

#[test]
fn multiplication_binds_tighter_than_addition() {
    assert_eq!(
        parse("1+2*3"),
        bin(
            ArithBinaryOp::Add,
            num(1),
            bin(ArithBinaryOp::Mul, num(2), num(3))
        )
    );
}

#[test]
fn parentheses_override_precedence() {
    assert_eq!(
        parse("(1+2)*3"),
        bin(
            ArithBinaryOp::Mul,
            bin(ArithBinaryOp::Add, num(1), num(2)),
            num(3)
        )
    );
}

#[test]
fn additive_is_left_associative() {
    // 1-2-3 == (1-2)-3, not 1-(2-3).
    assert_eq!(
        parse("1-2-3"),
        bin(
            ArithBinaryOp::Sub,
            bin(ArithBinaryOp::Sub, num(1), num(2)),
            num(3)
        )
    );
}

#[test]
fn power_is_right_associative_and_binds_tighter_than_multiplicative() {
    // Confirmed against real bash: 2**3**2 == 512 (== 2**(3**2)).
    assert_eq!(
        parse("2**3**2"),
        bin(
            ArithBinaryOp::Power,
            num(2),
            bin(ArithBinaryOp::Power, num(3), num(2))
        )
    );
    assert_eq!(
        parse("2*3**2"),
        bin(
            ArithBinaryOp::Mul,
            num(2),
            bin(ArithBinaryOp::Power, num(3), num(2))
        )
    );
}

#[test]
fn unary_minus_binds_tighter_than_power() {
    // Confirmed against real bash: -2**2 == 4 (== (-2)**2, not -(2**2)).
    assert_eq!(
        parse("-2**2"),
        bin(
            ArithBinaryOp::Power,
            ArithExpr::Unary {
                op: ArithUnaryOp::Minus,
                operand: Box::new(num(2)),
            },
            num(2)
        )
    );
}

#[test]
fn full_precedence_ladder_left_to_right_lowest_wins_outermost() {
    // A single expression touching every left-assoc binary level,
    // checked by evaluating (spot-checking the tree shape would be
    // extremely verbose) -- this doubles as an evaluator-independent
    // sanity check that the whole chain is wired together correctly.
    fn eval(e: &ArithExpr) -> i64 {
        match e {
            ArithExpr::Number(n) => *n,
            ArithExpr::Binary { op, lhs, rhs } => {
                let (l, r) = (eval(lhs), eval(rhs));
                match op {
                    ArithBinaryOp::Power => l.pow(r as u32),
                    ArithBinaryOp::Mul => l * r,
                    ArithBinaryOp::Div => l / r,
                    ArithBinaryOp::Rem => l % r,
                    ArithBinaryOp::Add => l + r,
                    ArithBinaryOp::Sub => l - r,
                    ArithBinaryOp::ShiftLeft => l << r,
                    ArithBinaryOp::ShiftRight => l >> r,
                    ArithBinaryOp::Less => i64::from(l < r),
                    ArithBinaryOp::LessEq => i64::from(l <= r),
                    ArithBinaryOp::Greater => i64::from(l > r),
                    ArithBinaryOp::GreaterEq => i64::from(l >= r),
                    ArithBinaryOp::Eq => i64::from(l == r),
                    ArithBinaryOp::NotEq => i64::from(l != r),
                    ArithBinaryOp::BitAnd => l & r,
                    ArithBinaryOp::BitXor => l ^ r,
                    ArithBinaryOp::BitOr => l | r,
                    ArithBinaryOp::LogicalAnd => i64::from(l != 0 && r != 0),
                    ArithBinaryOp::LogicalOr => i64::from(l != 0 || r != 0),
                }
            }
            _ => unreachable!(),
        }
    }
    let expr = parse("1 + 2 * 3 - 4 / 2 == 3 && 1 | 2 == 3");
    // Real bash: echo $((1 + 2 * 3 - 4 / 2 == 3 && 1 | 2 == 3)) -> 0
    // ( (1 + (2*3) - (4/2)) == 3 ) -> (1+6-2)==3 -> 5==3 -> 0
    // && ( 1 | (2==3) ) -> 1|0 -> 1
    // 0 && 1 -> 0
    assert_eq!(eval(&expr), 0);
}

// ---- unary / postfix / prefix increment-decrement --------------------------

#[test]
fn logical_and_bitwise_negation() {
    assert_eq!(
        parse("!0"),
        ArithExpr::Unary {
            op: ArithUnaryOp::LogicalNot,
            operand: Box::new(num(0)),
        }
    );
    assert_eq!(
        parse("~5"),
        ArithExpr::Unary {
            op: ArithUnaryOp::BitNot,
            operand: Box::new(num(5)),
        }
    );
}

#[test]
fn postfix_and_prefix_increment_decrement() {
    assert_eq!(
        parse("x++"),
        ArithExpr::IncrDecr {
            op: IncrDecrOp::Increment,
            target: "x".into(),
            prefix: false,
        }
    );
    assert_eq!(
        parse("--x"),
        ArithExpr::IncrDecr {
            op: IncrDecrOp::Decrement,
            target: "x".into(),
            prefix: true,
        }
    );
}

#[test]
fn increment_decrement_requires_identifier_operand() {
    assert!(matches!(
        parse_arithmetic_expr("5++"),
        Err(ArithError::NotAnIdentifier { .. })
    ));
    assert!(matches!(
        parse_arithmetic_expr("++5"),
        Err(ArithError::NotAnIdentifier { .. })
    ));
    assert!(matches!(
        parse_arithmetic_expr("(x+1)++"),
        Err(ArithError::NotAnIdentifier { .. })
    ));
}

// ---- ternary / assignment / comma -----------------------------------------

#[test]
fn ternary_false_branch_is_right_associative_without_parens() {
    // Confirmed against real bash: 0 ? 1 : 2 ? 3 : 4 == 3.
    assert_eq!(
        parse("0 ? 1 : 2 ? 3 : 4"),
        ArithExpr::Conditional {
            cond: Box::new(num(0)),
            if_true: Box::new(num(1)),
            if_false: Box::new(ArithExpr::Conditional {
                cond: Box::new(num(2)),
                if_true: Box::new(num(3)),
                if_false: Box::new(num(4)),
            }),
        }
    );
}

#[test]
fn ternary_true_branch_accepts_a_bare_comma() {
    // Confirmed against real bash: echo $((1 ? 2,3 : 4)) == 3.
    assert_eq!(
        parse("1 ? 2,3 : 4"),
        ArithExpr::Conditional {
            cond: Box::new(num(1)),
            if_true: Box::new(ArithExpr::Comma {
                first: Box::new(num(2)),
                second: Box::new(num(3)),
            }),
            if_false: Box::new(num(4)),
        }
    );
}

#[test]
fn ternary_true_branch_accepts_a_bare_assignment() {
    // Confirmed against real bash: x=0; echo $((1 ? x=5 : 0)) sets x.
    assert_eq!(
        parse("1 ? x=5 : 0"),
        ArithExpr::Conditional {
            cond: Box::new(num(1)),
            if_true: Box::new(ArithExpr::Assign {
                op: ArithAssignOp::Assign,
                target: "x".into(),
                value: Box::new(num(5)),
            }),
            if_false: Box::new(num(0)),
        }
    );
}

#[test]
fn assignment_is_right_associative() {
    // Confirmed against real bash: x=1; y=2; echo $((x=y=5)) sets both.
    assert_eq!(
        parse("x=y=5"),
        ArithExpr::Assign {
            op: ArithAssignOp::Assign,
            target: "x".into(),
            value: Box::new(ArithExpr::Assign {
                op: ArithAssignOp::Assign,
                target: "y".into(),
                value: Box::new(num(5)),
            }),
        }
    );
}

#[test]
fn compound_assignment_operators() {
    assert_eq!(
        parse("x+=1"),
        ArithExpr::Assign {
            op: ArithAssignOp::AddAssign,
            target: "x".into(),
            value: Box::new(num(1)),
        }
    );
    assert_eq!(
        parse("x<<=2"),
        ArithExpr::Assign {
            op: ArithAssignOp::ShiftLeftAssign,
            target: "x".into(),
            value: Box::new(num(2)),
        }
    );
}

#[test]
fn assignment_requires_identifier_target() {
    assert!(matches!(
        parse_arithmetic_expr("1=2"),
        Err(ArithError::NotAnIdentifier { .. })
    ));
}

#[test]
fn comma_is_left_associative_and_lowest_precedence() {
    assert_eq!(
        parse("1,2,3"),
        ArithExpr::Comma {
            first: Box::new(ArithExpr::Comma {
                first: Box::new(num(1)),
                second: Box::new(num(2)),
            }),
            second: Box::new(num(3)),
        }
    );
}

// ---- empty / whitespace-only expression ------------------------------------

#[test]
fn empty_expression_is_zero() {
    // Confirmed against real bash: echo $(()) -> 0.
    assert_eq!(parse(""), num(0));
    assert_eq!(parse("   "), num(0));
}

// ---- trailing garbage / malformed input -------------------------------------

#[test]
fn trailing_tokens_after_a_complete_expression_error() {
    assert!(parse_arithmetic_expr("1 2").is_err());
}

#[test]
fn unbalanced_parens_error() {
    assert!(parse_arithmetic_expr("(1+2").is_err());
    assert!(parse_arithmetic_expr("1+2)").is_err());
}

#[test]
fn invalid_character_errors() {
    // A bare single-quote is never valid arithmetic syntax (see the
    // module docs' quoting grounding).
    assert!(matches!(
        parse_arithmetic_expr("'x'"),
        Err(ArithError::UnexpectedChar { .. })
    ));
}

// ---- phase 1: parse_arithmetic_body ----------------------------------------

#[test]
fn body_with_no_expansions_is_one_literal_segment() {
    assert_eq!(
        parse_arithmetic_body("1 + 2").unwrap(),
        Word::new(vec![WordSegment::Literal("1 + 2".into())])
    );
}

#[test]
fn body_recognizes_dollar_expansion_sites() {
    assert_eq!(
        parse_arithmetic_body("$x + 1").unwrap(),
        Word::new(vec![
            WordSegment::Parameter(Parameter::Name("x".into())),
            WordSegment::Literal(" + 1".into()),
        ])
    );
}

#[test]
fn body_recognizes_command_substitution_sites() {
    assert_eq!(
        parse_arithmetic_body("$(echo 3) + 1").unwrap(),
        Word::new(vec![
            WordSegment::CommandSubstitution(CommandSubstitution {
                style: SubstitutionStyle::DollarParen,
                body: "echo 3".into(),
            }),
            WordSegment::Literal(" + 1".into()),
        ])
    );
}

#[test]
fn body_recognizes_backquote_command_substitution() {
    assert_eq!(
        parse_arithmetic_body("`echo 3` + 1").unwrap(),
        Word::new(vec![
            WordSegment::CommandSubstitution(CommandSubstitution {
                style: SubstitutionStyle::Backtick,
                body: "echo 3".into(),
            }),
            WordSegment::Literal(" + 1".into()),
        ])
    );
}

#[test]
fn body_recognizes_nested_arithmetic_expansion() {
    // $(( $((1+2)) + 3 ))
    assert_eq!(
        parse_arithmetic_body("$((1+2)) + 3").unwrap(),
        Word::new(vec![
            WordSegment::ArithmeticExpansion("1+2".into()),
            WordSegment::Literal(" + 3".into()),
        ])
    );
}

#[test]
fn body_deletes_unescaped_double_quotes() {
    // Confirmed against real bash: echo $(( 1 + "x)" + 2 )) shows the
    // fully-expanded text as `1 + x) + 2` -- the quote characters
    // vanish with no trace, including not pairing up with each other.
    assert_eq!(
        parse_arithmetic_body(r#" 1 + "x)" + 2 "#).unwrap(),
        Word::new(vec![WordSegment::Literal(" 1 + x) + 2 ".into())])
    );
}

#[test]
fn body_keeps_escaped_double_quote_literal() {
    // Confirmed against real bash: echo $(( 1 + \"2\" )) shows the text
    // as `1 + "2"` -- an *escaped* quote survives (unlike a bare one).
    assert_eq!(
        parse_arithmetic_body(r#" 1 + \"2\" "#).unwrap(),
        Word::new(vec![WordSegment::Literal(r#" 1 + "2" "#.into())])
    );
}

#[test]
fn body_single_quote_is_always_literal() {
    // Confirmed against real bash: single-quote never quotes inside
    // $((...)), unlike inside an ordinary word or even a `${...}`
    // operand nested in double quotes.
    assert_eq!(
        parse_arithmetic_body("'x)' + 1").unwrap(),
        Word::new(vec![WordSegment::Literal("'x)' + 1".into())])
    );
}

#[test]
fn body_backslash_only_escapes_the_double_quote_style_set() {
    // Confirmed against real bash: echo $((1\+2)) reports the error
    // token as literally `\+2` -- backslash before a non-special
    // character stays literal (both characters kept), same rule as
    // inside an ordinary double-quoted string.
    assert_eq!(
        parse_arithmetic_body(r"1\+2").unwrap(),
        Word::new(vec![WordSegment::Literal(r"1\+2".into())])
    );
}

#[test]
fn body_backslash_newline_is_a_line_continuation() {
    assert_eq!(
        parse_arithmetic_body("1 +\\\n2").unwrap(),
        Word::new(vec![WordSegment::Literal("1 +2".into())])
    );
}

#[test]
fn body_trailing_backslash_errors() {
    assert_eq!(
        parse_arithmetic_body("1+2\\"),
        Err(ArithError::TrailingBackslash { pos: 3 })
    );
}

#[test]
fn end_to_end_nested_command_substitution_then_expression_parse() {
    // Simulates what the evaluator will actually do: parse the body,
    // pretend-expand it (no shell state needed since there's no
    // expansion site here), then parse the resulting text as an
    // expression.
    let body = parse_arithmetic_body("1 + 2 * 3").unwrap();
    let Some(WordSegment::Literal(text)) = body.segments.first() else {
        unreachable!()
    };
    assert_eq!(
        parse_arithmetic_expr(text).unwrap(),
        bin(
            ArithBinaryOp::Add,
            num(1),
            bin(ArithBinaryOp::Mul, num(2), num(3))
        )
    );
}
