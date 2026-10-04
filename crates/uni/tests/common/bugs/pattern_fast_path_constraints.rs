// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Pattern predicates and pattern comprehensions honour the whole pattern.
//!
//! Both have a vectorized operator that walks adjacency from a bound anchor.
//! It honoured only part of a pattern and silently dropped the rest: the
//! anchor's labels (`WHERE (n:Person)-[:R]->()` also matched a Robot),
//! relationship property maps, relationship uniqueness across hops, and — in
//! a comprehension — target property maps and variable-length ranges. The
//! operators now take only patterns whose every feature they implement; the
//! rest run as correlated subqueries.
//!
//! Along the way, a correlated subquery that put a label on an outer-bound
//! node (`EXISTS { MATCH (n:Person)-[:R]->() }`) failed with
//! `No field named "n._labels"`; the label is now tested against the node's
//! labels passed in from the outer row.
//!
//! Oracle: each fast-path form is compared with the same pattern written as an
//! explicit `EXISTS { MATCH … }` / `COUNT { MATCH … }`, and pinned to its
//! expected answer so the pair cannot agree on a wrong one.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(pattern_fast_path)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni};

/// Person 1 -[R w=5]-> C 3; Robot 2 -[R w=1]-> C 3; C 3 -[S]-> C 4;
/// Person 1 -[T]-> C 3.
async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    let all = ["Person", "Robot", "C"];
    db.schema()
        .label("Person")
        .property("id", DataType::Int)
        .label("Robot")
        .property("id", DataType::Int)
        .label("C")
        .property("id", DataType::Int)
        .edge_type("R", &all, &all)
        .property("w", DataType::Int)
        .edge_type("S", &all, &all)
        .done()
        .edge_type("T", &all, &all)
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    for stmt in [
        "CREATE (:Person {id: 1}), (:Robot {id: 2}), (:C {id: 3}), (:C {id: 4})",
        "MATCH (p:Person {id:1}), (c:C {id:3}) CREATE (p)-[:R {w: 5}]->(c)",
        "MATCH (r:Robot {id:2}), (c:C {id:3}) CREATE (r)-[:R {w: 1}]->(c)",
        "MATCH (c:C {id:3}), (d:C {id:4}) CREATE (c)-[:S]->(d)",
        "MATCH (p:Person {id:1}), (c:C {id:3}) CREATE (p)-[:T]->(c)",
    ] {
        tx.execute(stmt).await?;
    }
    tx.commit().await?;
    Ok(db)
}

async fn bag(db: &Uni, query: &str) -> Result<Vec<String>> {
    let result = db.session().query(query).await?;
    let mut rows: Vec<String> = result
        .rows()
        .iter()
        .map(|r| format!("{:?}", r.values()))
        .collect();
    rows.sort();
    Ok(rows)
}

/// `(fast-path form, explicit-subquery form, expected ids)` for predicates.
const PREDICATES: &[(&str, &str, &[i64])] = &[
    ("(n:Person)-[:R]->()", "(n:Person)-[:R]->()", &[1]),
    ("(n)-[:R {w: 1}]->()", "(n)-[:R {w: 1}]->()", &[2]),
    ("(n)-[:R]-()-[:R]-(n)", "(n)-[:R]-()-[:R]-(n)", &[]),
    ("(n)-[:R]->(:C:Robot)", "(n)-[:R]->(:C:Robot)", &[]),
    ("(n)-[:R]->(:Robot|C)", "(n)-[:R]->(:Robot|C)", &[1, 2]),
    (
        "(n)-[:R|T]->()<-[:R|T]-(n)",
        "(n)-[:R|T]->()<-[:R|T]-(n)",
        &[1],
    ),
    ("(n)-[:R]->({id: 3})", "(n)-[:R]->({id: 3})", &[1, 2]),
    ("(n)-[:R]->({id: 4})", "(n)-[:R]->({id: 4})", &[]),
];

#[tokio::test]
async fn pattern_fast_path_predicates_match_exists_subquery() -> Result<()> {
    let db = open().await?;
    for (fast, explicit, want) in PREDICATES {
        let fast_q = format!("MATCH (n) WHERE {fast} RETURN n.id");
        let explicit_q = format!("MATCH (n) WHERE EXISTS {{ MATCH {explicit} }} RETURN n.id");
        let want: Vec<String> = want.iter().map(|i| format!("[Int({i})]")).collect();
        assert_eq!(bag(&db, &explicit_q).await?, want, "{explicit_q}");
        assert_eq!(bag(&db, &fast_q).await?, want, "{fast_q}");
    }
    Ok(())
}

/// `(comprehension pattern, expected count per id 1..=4)`.
const COMPREHENSIONS: &[(&str, [i64; 4])] = &[
    ("(n)-[:R {w: 1}]->(x)", [0, 1, 0, 0]),
    ("(n:Person)-[:R]->(x)", [1, 0, 0, 0]),
    ("(n)-[:R]->(x {id: 3})", [1, 1, 0, 0]),
    ("(n)-[:R|S*1..2]->(x)", [2, 2, 1, 0]),
    ("(n)-[:R|T]->(x)<-[:R|T]-(n)", [2, 0, 0, 0]),
];

#[tokio::test]
async fn pattern_fast_path_comprehensions_match_count_subquery() -> Result<()> {
    let db = open().await?;
    for (pattern, want) in COMPREHENSIONS {
        let fast_q = format!("MATCH (n) RETURN n.id, size([{pattern} | x]) AS k");
        let explicit_q = format!("MATCH (n) RETURN n.id, COUNT {{ MATCH {pattern} }} AS k");
        let want: Vec<String> = want
            .iter()
            .enumerate()
            .map(|(i, k)| format!("[Int({}), Int({k})]", i + 1))
            .collect();
        assert_eq!(bag(&db, &explicit_q).await?, want, "{explicit_q}");
        assert_eq!(bag(&db, &fast_q).await?, want, "{fast_q}");
    }
    Ok(())
}

/// A bound node later in a comprehension's pattern is the same node, not a
/// fresh binding.
#[tokio::test]
async fn pattern_fast_path_comprehension_respects_bound_target() -> Result<()> {
    let db = open().await?;
    let q = "MATCH (n:Person), (c:C {id: 4}) RETURN size([(n)-[:R]->(c) | c]) AS k";
    assert_eq!(bag(&db, q).await?, vec!["[Int(0)]".to_string()]);
    Ok(())
}
