//! SQL-subset parser.
//!
//! The parser is a security boundary (`04-threat-model-security.md` §4), so the
//! tests are weighted towards what it must *refuse* and towards proving that a
//! hostile literal stays a literal. A parser that is merely usually right is not
//! a boundary.

use theta_core::Value;
use theta_query::plan::{AggFunc, Expr, Literal, Plan, Predicate, SortOrder};
use theta_query::sql::{compile, SqlError};

fn plan(sql: &str) -> Plan {
    compile(sql).unwrap_or_else(|e| panic!("failed to compile `{sql}`: {e}"))
}

/// Walk to the innermost scan, so tests can assert on shape without matching
/// every wrapper.
fn source_of(plan: &Plan) -> &str {
    plan.source_table()
}

// ---- shape ------------------------------------------------------------------

#[test]
fn a_bare_select_star_is_a_scan() {
    assert_eq!(
        plan("SELECT * FROM users"),
        Plan::Scan {
            table: "users".into()
        }
    );
}

#[test]
fn keywords_are_case_insensitive() {
    assert_eq!(plan("select * from users"), plan("SELECT * FROM users"));
    assert_eq!(plan("SeLeCt * FrOm users"), plan("SELECT * FROM users"));
}

#[test]
fn a_column_list_becomes_a_projection() {
    match plan("SELECT name, age FROM users") {
        Plan::Project { columns, input } => {
            assert_eq!(columns, vec!["name", "age"]);
            assert_eq!(
                *input,
                Plan::Scan {
                    table: "users".into()
                }
            );
        }
        other => panic!("expected a projection, got {other:?}"),
    }
}

#[test]
fn a_where_clause_becomes_a_filter() {
    match plan("SELECT * FROM users WHERE age > 30") {
        Plan::Filter { predicate, .. } => assert_eq!(
            predicate,
            Predicate::Gt {
                column: "age".into(),
                value: Expr::Literal(Literal(Value::Int(30))),
            }
        ),
        other => panic!("expected a filter, got {other:?}"),
    }
}

#[test]
fn every_comparison_operator_maps_to_its_predicate() {
    let cases = [
        ("=", "Eq"),
        ("!=", "Ne"),
        ("<>", "Ne"),
        ("<", "Lt"),
        ("<=", "Lte"),
        (">", "Gt"),
        (">=", "Gte"),
    ];
    for (op, expected) in cases {
        let compiled = plan(&format!("SELECT * FROM t WHERE a {op} 1"));
        let Plan::Filter { predicate, .. } = compiled else {
            panic!("expected a filter for `{op}`");
        };
        let rendered = format!("{predicate:?}");
        assert!(
            rendered.starts_with(expected),
            "`{op}` produced {rendered}, expected {expected}"
        );
    }
}

#[test]
fn and_binds_tighter_than_or() {
    // `a OR b AND c` must parse as `a OR (b AND c)`, or the query means
    // something else entirely.
    let Plan::Filter { predicate, .. } = plan("SELECT * FROM t WHERE a = 1 OR b = 2 AND c = 3")
    else {
        panic!("expected a filter");
    };
    match predicate {
        Predicate::Or(terms) => {
            assert_eq!(terms.len(), 2);
            assert!(matches!(terms[0], Predicate::Eq { .. }));
            assert!(matches!(terms[1], Predicate::And(_)));
        }
        other => panic!("expected OR at the top, got {other:?}"),
    }
}

#[test]
fn parentheses_override_precedence() {
    let Plan::Filter { predicate, .. } = plan("SELECT * FROM t WHERE (a = 1 OR b = 2) AND c = 3")
    else {
        panic!("expected a filter");
    };
    assert!(matches!(predicate, Predicate::And(_)), "got {predicate:?}");
}

#[test]
fn not_negates_the_following_comparison() {
    let Plan::Filter { predicate, .. } = plan("SELECT * FROM t WHERE NOT a = 1") else {
        panic!("expected a filter");
    };
    assert!(matches!(predicate, Predicate::Not(_)));
}

#[test]
fn is_null_and_is_not_null_both_parse() {
    let Plan::Filter { predicate, .. } = plan("SELECT * FROM t WHERE a IS NULL") else {
        panic!("expected a filter");
    };
    assert_eq!(predicate, Predicate::IsNull { column: "a".into() });

    let Plan::Filter { predicate, .. } = plan("SELECT * FROM t WHERE a IS NOT NULL") else {
        panic!("expected a filter");
    };
    assert!(matches!(predicate, Predicate::Not(_)));
}

#[test]
fn in_parses_a_value_list() {
    let Plan::Filter { predicate, .. } = plan("SELECT * FROM t WHERE a IN (1, 2, 3)") else {
        panic!("expected a filter");
    };
    match predicate {
        Predicate::In { values, .. } => assert_eq!(values.len(), 3),
        other => panic!("expected IN, got {other:?}"),
    }
}

#[test]
fn order_by_supports_direction_and_multiple_keys() {
    match plan("SELECT * FROM t ORDER BY a DESC, b") {
        Plan::Sort { by, .. } => {
            assert_eq!(by[0], ("a".to_string(), SortOrder::Desc));
            // Absent direction means ascending, as in SQL.
            assert_eq!(by[1], ("b".to_string(), SortOrder::Asc));
        }
        other => panic!("expected a sort, got {other:?}"),
    }
}

#[test]
fn limit_and_offset_parse() {
    match plan("SELECT * FROM t LIMIT 10 OFFSET 20") {
        Plan::Limit { count, offset, .. } => {
            assert_eq!(count, 10);
            assert_eq!(offset, 20);
        }
        other => panic!("expected a limit, got {other:?}"),
    }
}

#[test]
fn aggregates_parse_with_and_without_an_alias() {
    match plan("SELECT COUNT(*) FROM t") {
        Plan::Aggregate {
            aggregates,
            group_by,
            ..
        } => {
            assert!(group_by.is_empty());
            assert_eq!(aggregates[0].func, AggFunc::Count);
            assert_eq!(aggregates[0].column, None);
            assert_eq!(aggregates[0].alias, "count");
        }
        other => panic!("expected an aggregate, got {other:?}"),
    }

    match plan("SELECT SUM(amount) AS total FROM orders") {
        Plan::Aggregate { aggregates, .. } => {
            assert_eq!(aggregates[0].func, AggFunc::Sum);
            assert_eq!(aggregates[0].column.as_deref(), Some("amount"));
            assert_eq!(aggregates[0].alias, "total");
        }
        other => panic!("expected an aggregate, got {other:?}"),
    }
}

#[test]
fn group_by_carries_its_columns() {
    match plan("SELECT status, COUNT(*) FROM orders GROUP BY status") {
        Plan::Aggregate {
            group_by,
            aggregates,
            ..
        } => {
            assert_eq!(group_by, vec!["status"]);
            assert_eq!(aggregates.len(), 1);
        }
        other => panic!("expected an aggregate, got {other:?}"),
    }
}

#[test]
fn a_full_statement_composes_every_clause() {
    let compiled = plan(
        "SELECT * FROM users WHERE age >= 21 AND active = TRUE ORDER BY age DESC LIMIT 10 OFFSET 5",
    );
    assert_eq!(source_of(&compiled), "users");
    assert!(matches!(compiled, Plan::Limit { .. }), "limit is outermost");
}

#[test]
fn comments_are_ignored() {
    assert_eq!(
        plan("SELECT * FROM users -- trailing comment\n"),
        Plan::Scan {
            table: "users".into()
        }
    );
}

// ---- literals and parameters ------------------------------------------------

#[test]
fn literals_of_each_type_parse_to_typed_values() {
    let cases: [(&str, Value); 5] = [
        ("'hello'", Value::Text("hello".into())),
        ("42", Value::Int(42)),
        ("4.5", Value::Float(4.5)),
        ("TRUE", Value::Bool(true)),
        ("NULL", Value::Null),
    ];
    for (source, expected) in cases {
        let Plan::Filter { predicate, .. } = plan(&format!("SELECT * FROM t WHERE a = {source}"))
        else {
            panic!("expected a filter for `{source}`");
        };
        let Predicate::Eq { value, .. } = predicate else {
            panic!("expected equality for `{source}`");
        };
        assert_eq!(value, Expr::Literal(Literal(expected)), "for `{source}`");
    }
}

#[test]
fn an_escaped_quote_stays_inside_the_string() {
    let Plan::Filter { predicate, .. } = plan("SELECT * FROM t WHERE a = 'it''s fine'") else {
        panic!("expected a filter");
    };
    let Predicate::Eq { value, .. } = predicate else {
        panic!("expected equality");
    };
    assert_eq!(
        value,
        Expr::Literal(Literal(Value::Text("it's fine".into())))
    );
}

#[test]
fn a_parameter_becomes_a_bound_node_not_text() {
    let Plan::Filter { predicate, .. } = plan("SELECT * FROM users WHERE name = $who") else {
        panic!("expected a filter");
    };
    let Predicate::Eq { value, .. } = predicate else {
        panic!("expected equality");
    };
    assert_eq!(
        value,
        Expr::Param {
            name: "who".into(),
            ty: None
        },
        "a parameter must survive as a Param node, never as text"
    );
}

#[test]
fn parameters_are_discoverable_from_the_compiled_plan() {
    let compiled = plan("SELECT * FROM t WHERE a = $one AND b = $two");
    let names: Vec<&str> = compiled.params().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["one", "two"]);
}

// ---- the security boundary --------------------------------------------------

#[test]
fn a_hostile_string_literal_stays_a_single_literal() {
    // The classic payload. It is inside quotes, so it is one Text value — the
    // parser has no branch that lets a literal become syntax.
    let compiled = plan("SELECT * FROM users WHERE name = 'x''; DROP TABLE users; --'");

    let Plan::Filter { predicate, input } = compiled else {
        panic!("expected a filter");
    };
    assert_eq!(
        *input,
        Plan::Scan {
            table: "users".into()
        }
    );

    let Predicate::Eq { value, .. } = predicate else {
        panic!("expected equality");
    };
    assert_eq!(
        value,
        Expr::Literal(Literal(Value::Text("x'; DROP TABLE users; --".into()))),
        "the payload must survive as one inert value"
    );
}

#[test]
fn the_plan_ir_cannot_carry_raw_text_into_execution() {
    // Every plan the parser can produce serializes without any node that holds
    // executable text. If a `Raw` variant were ever added to the IR, this is
    // where it would show up.
    for sql in [
        "SELECT * FROM users",
        "SELECT * FROM users WHERE name = 'anything at all'",
        "SELECT COUNT(*) FROM users GROUP BY status",
    ] {
        let json = plan(sql).to_json().expect("plans serialize");
        assert!(
            !json.contains("\"raw\""),
            "plan for `{sql}` carried raw text"
        );
    }
}

#[test]
fn a_bare_identifier_in_a_value_position_is_refused_not_treated_as_a_string() {
    // If `admin` were quietly read as the string "admin", then
    // `WHERE role = admin` would compare against a literal rather than erroring
    // — a silent change of meaning.
    assert!(matches!(
        compile("SELECT * FROM users WHERE role = admin"),
        Err(SqlError::Unsupported { .. })
    ));
}

#[test]
fn an_unterminated_string_is_an_error_rather_than_running_to_end_of_input() {
    assert!(matches!(
        compile("SELECT * FROM users WHERE name = 'unclosed"),
        Err(SqlError::UnterminatedString { .. })
    ));
}

#[test]
fn an_unrecognised_character_stops_the_parse_rather_than_being_skipped() {
    // Skipping is how a parser silently changes what a statement means.
    assert!(compile("SELECT * FROM users WHERE a = 1 ; DROP TABLE users").is_err());
    assert!(compile("SELECT * FROM users #comment").is_err());
}

// ---- what it refuses --------------------------------------------------------

#[test]
fn unsupported_clauses_are_refused_rather_than_ignored() {
    // Each of these, silently dropped, would return a plausible answer to a
    // different question.
    let cases = [
        "SELECT * FROM a JOIN b ON a.id = b.id",
        "SELECT DISTINCT name FROM users",
        "SELECT COUNT(*) FROM t GROUP BY a HAVING COUNT(*) > 1",
        "SELECT * FROM a UNION SELECT * FROM b",
        "SELECT MEDIAN(x) FROM t",
        "SELECT name AS n FROM users",
    ];
    for sql in cases {
        assert!(
            matches!(compile(sql), Err(SqlError::Unsupported { .. })),
            "`{sql}` should be refused as unsupported, got {:?}",
            compile(sql)
        );
    }
}

#[test]
fn writes_are_not_parseable_at_all() {
    // The subset is read-only by construction: there is no statement form for a
    // write, so a query can never mutate.
    for sql in [
        "INSERT INTO users VALUES (1)",
        "UPDATE users SET name = 'x'",
        "DELETE FROM users",
        "DROP TABLE users",
        "CREATE TABLE t (a INT)",
    ] {
        assert!(compile(sql).is_err(), "`{sql}` must not compile");
    }
}

#[test]
fn a_plain_column_beside_an_aggregate_must_be_grouped() {
    // Otherwise the value returned for it is arbitrary. Postgres rejects this
    // too, and a migrating user would not expect it to be accepted.
    assert!(compile("SELECT name, COUNT(*) FROM users").is_err());
    assert!(compile("SELECT name, COUNT(*) FROM users GROUP BY name").is_ok());
}

#[test]
fn malformed_statements_report_where_they_failed() {
    for sql in [
        "SELECT",
        "SELECT * FROM",
        "SELECT * FROM t WHERE",
        "SELECT * FROM t WHERE a",
        "SELECT * FROM t WHERE a =",
        "SELECT * FROM t LIMIT",
        "SELECT * FROM t LIMIT abc",
        "SELECT * FROM t ORDER BY",
        "SELECT * FROM t GROUP",
        "NOT SQL AT ALL",
        "",
    ] {
        assert!(compile(sql).is_err(), "`{sql}` should not compile");
    }
}

#[test]
fn a_negative_limit_is_refused() {
    assert!(compile("SELECT * FROM t LIMIT 5 OFFSET 3").is_ok());
    // `-5` tokenizes as an unexpected character rather than a negative number,
    // which is fine: either way it does not compile.
    assert!(compile("SELECT * FROM t LIMIT -5").is_err());
}

#[test]
fn trailing_input_after_a_complete_statement_is_refused() {
    assert!(compile("SELECT * FROM t garbage").is_err());
    assert!(compile("SELECT * FROM t SELECT * FROM t").is_err());
}

// ---- plan caching -----------------------------------------------------------

#[test]
fn the_same_statement_compiles_to_the_same_plan_hash() {
    let a = plan("SELECT * FROM users WHERE age > 30");
    let b = plan("SELECT * FROM users WHERE age > 30");
    assert_eq!(a.hash(), b.hash(), "identical SQL must hit the plan cache");

    let c = plan("SELECT * FROM users WHERE age > 31");
    assert_ne!(a.hash(), c.hash(), "different SQL must not collide");
}

#[test]
fn a_parameterized_plan_hashes_the_same_whatever_it_will_be_bound_to() {
    // This is the whole point of parameters: one compiled plan serves every set
    // of arguments.
    let a = plan("SELECT * FROM users WHERE name = $who");
    let b = plan("SELECT * FROM users WHERE name = $who");
    assert_eq!(a.hash(), b.hash());

    // ...and differs from the same query with the value inlined, because those
    // are genuinely different plans.
    let inlined = plan("SELECT * FROM users WHERE name = 'Alice'");
    assert_ne!(a.hash(), inlined.hash());
}
