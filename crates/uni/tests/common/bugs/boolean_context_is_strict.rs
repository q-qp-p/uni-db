// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A Cypher boolean context (`WHERE`, `CASE WHEN`) over a dynamically typed
//! value is three-valued and strict, and a list slice bound must be an integer.
//!
//! A schemaless property (or any dynamically typed value) is read as a boolean
//! by `_cv_to_bool`. That read a non-boolean as `false`, so `NOT n.b` silently
//! *kept* a row whose `b` is `1` and `CASE WHEN n.b` took its `ELSE`, where
//! Cypher raises a type error. Only `NOT` and `CASE WHEN` used it: a bare
//! `WHERE n.b`, and `n.b AND ...`, failed to plan even when every `b` was a
//! boolean. Both now read the value through the strict `_cv_to_bool`.
//!
//! A slice bound that was not an integer read as 0 (start) or the list's length
//! (end), returning a slice the query never asked for.
//!
//! Found by the W6 fail-open audit.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(boolean_context_is_strict)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{Uni, Value};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    let tx = db.session().tx().await?;
    tx.execute("CREATE (:N {b: true}), (:N {b: false}), (:N {b: 1}), (:N {x: 1})")
        .await?;
    tx.commit().await?;
    Ok(db)
}

async fn single(db: &Uni, query: &str) -> Result<Value> {
    let result = db.session().query(query).await?;
    assert_eq!(result.rows().len(), 1, "{query}");
    Ok(result.rows()[0].values()[0].clone())
}

#[tokio::test]
async fn boolean_context_is_strict_on_a_non_boolean() -> Result<()> {
    let db = open().await?;
    for query in [
        "MATCH (n:N) WHERE NOT n.b RETURN count(*)",
        "MATCH (n:N) RETURN count(CASE WHEN n.b THEN 1 END)",
        "MATCH (n:N) WITH n.b AS b WHERE NOT b RETURN count(*)",
        "UNWIND [true, false, null, 1] AS b RETURN count(CASE WHEN b THEN 1 END)",
    ] {
        let err = db
            .session()
            .query(query)
            .await
            .expect_err(&format!("`{query}`: `b` is 1 on one row"));
        assert!(
            err.to_string().contains("expected a boolean"),
            "{query}: {err}"
        );
    }
    // Over booleans and NULLs only, every boolean context answers as Cypher
    // does: a NULL drops the row, under `NOT` too. A bare `WHERE n.b`, and
    // `n.b AND ...`, failed to plan here ("Filter predicate must return
    // BOOLEAN values, got LargeBinary").
    let booleans = "MATCH (n:N) WHERE n.b IS NULL OR n.b <> 1 WITH n ";
    for (query, want) in [
        (format!("{booleans}WHERE n.b RETURN count(*)"), 1),
        (format!("{booleans}WHERE NOT n.b RETURN count(*)"), 1),
        (format!("{booleans}WHERE n.b AND true RETURN count(*)"), 1),
        (format!("{booleans}WHERE n.b OR n.x = 1 RETURN count(*)"), 2),
        (
            format!("{booleans}WITH n.b AS b WHERE b RETURN count(*)"),
            1,
        ),
        (
            format!("{booleans}RETURN count(CASE WHEN n.b THEN 1 END)"),
            1,
        ),
        (
            format!("{booleans}RETURN count(CASE WHEN NOT n.b THEN 1 END)"),
            1,
        ),
        (
            format!("{booleans}WITH {{k: n.b}} AS m WHERE NOT m.k RETURN count(*)"),
            1,
        ),
        (
            format!("{booleans}WITH [n.b][0] AS v WHERE NOT v RETURN count(*)"),
            1,
        ),
        (
            "UNWIND [true, false, null] AS b WITH b WHERE b RETURN count(*)".to_string(),
            1,
        ),
        (
            "UNWIND [true, false, null] AS b WITH b WHERE NOT b RETURN count(*)".to_string(),
            1,
        ),
        (
            "UNWIND [{k: true}, {k: false}, {k: null}, {}] AS m WITH m WHERE NOT m.k \
             RETURN count(*)"
                .to_string(),
            1,
        ),
        (
            "UNWIND [[true], [false], [null]] AS l RETURN count(CASE WHEN NOT l[0] THEN 1 END)"
                .to_string(),
            1,
        ),
    ] {
        assert_eq!(single(&db, &query).await?, Value::Int(want), "{query}");
    }
    Ok(())
}

#[tokio::test]
async fn boolean_context_is_strict_on_list_slice_bounds() -> Result<()> {
    let db = open().await?;
    for query in [
        "RETURN [1, 2, 3][1.0..2]",
        "RETURN [1, 2, 3]['a'..]",
        "RETURN [1, 2, 3][..true]",
    ] {
        let err = db.session().query(query).await.expect_err(query);
        assert!(
            err.to_string().contains("must be an integer"),
            "{query}: {err}"
        );
    }
    for (query, want) in [
        ("RETURN [1, 2, 3][1..]", vec![2, 3]),
        ("RETURN [1, 2, 3][..-1]", vec![1, 2]),
        ("RETURN [1, 2, 3][0..2]", vec![1, 2]),
    ] {
        assert_eq!(
            single(&db, query).await?,
            Value::List(want.into_iter().map(Value::Int).collect()),
            "{query}"
        );
    }
    assert_eq!(single(&db, "RETURN [1, 2, 3][null..]").await?, Value::Null);
    Ok(())
}

/// A map compared with a value of another type is answered as openCypher
/// specifies: an ordering is NULL, `=` false and `<>` true (NULL when a side is
/// NULL). DataFusion has no common type for the pair, so `1 < {k: 1}` failed to
/// plan and `a.id <= {k: 1}` in a WHERE failed at run time ("Nested comparison").
#[tokio::test]
async fn boolean_context_is_strict_map_against_a_scalar() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("P")
        .property("id", uni_db::DataType::Int)
        .property_nullable("age", uni_db::DataType::Int)
        .apply()
        .await?;
    let tx = db.session().tx().await?;
    tx.execute("CREATE (:P {id: 1})").await?;
    tx.commit().await?;
    for (expr, want) in [
        ("1 < {k: 1}", Value::Null),
        ("{k: 1} >= 1", Value::Null),
        ("1 = {k: 1}", Value::Bool(false)),
        ("1 <> {k: 1}", Value::Bool(true)),
        ("a.id <= {k: 1}", Value::Null),
        ("a.id = {k: 1}", Value::Bool(false)),
        ("a.id <> {k: 1}", Value::Bool(true)),
        ("a.age = {k: 1}", Value::Null),
        ("a.age <> {k: 1}", Value::Null),
    ] {
        assert_eq!(
            single(&db, &format!("MATCH (a:P) RETURN {expr}")).await?,
            want,
            "{expr}"
        );
    }
    for (filter, want) in [
        ("NOT (a.id <= {k: 1})", 0),
        ("a.id <> {k: 1}", 1),
        ("NOT (a.id = {k: 1})", 1),
        ("a.age <> {k: 1}", 0),
    ] {
        assert_eq!(
            single(&db, &format!("MATCH (a:P) WHERE {filter} RETURN count(*)")).await?,
            Value::Int(want),
            "{filter}"
        );
    }
    Ok(())
}
