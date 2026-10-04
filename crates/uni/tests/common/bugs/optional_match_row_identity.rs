// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Every row entering an `OPTIONAL MATCH` yields its matches or one NULL row.
//!
//! The operators that emit the NULL rows recovered "which entering row is
//! this" from the bound node ids, so entering rows that bind the same node
//! merged: a second row that differed only in a scalar (`x`), or repeated
//! exactly, lost its NULL row, and under a `WHERE` one entering row's match
//! suppressed another's NULL. Rows now carry an id assigned as they enter the
//! clause (`df_graph::optional_source`).
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(optional_match_row_identity)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("A")
        .property("v", DataType::Int)
        .label("B")
        .property("v", DataType::Int)
        .edge_type("R", &["A", "B"], &["A", "B"])
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:A {v: 1}), (:A {v: 2}), (:B {v: 2})")
        .await?;
    tx.execute("MATCH (a:A {v: 2}), (b:B {v: 2}) CREATE (a)-[:R]->(b)")
        .await?;
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

fn rows(expected: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn optional_match_row_identity_unmatched_rows() -> Result<()> {
    let db = open().await?;
    let cases = [
        (
            "MATCH (a:A {v: 1}) UNWIND [1, 2] AS x OPTIONAL MATCH (a)-[:R]->(b) RETURN x, b.v",
            rows(&["[Int(1), Null]", "[Int(2), Null]"]),
        ),
        (
            "MATCH (a:A {v: 1}) UNWIND [1, 1] AS x OPTIONAL MATCH (a)-[:R]->(b) RETURN x, b.v",
            rows(&["[Int(1), Null]", "[Int(1), Null]"]),
        ),
        (
            "UNWIND [1, 2] AS x MATCH (a:A {v: 1}) OPTIONAL MATCH (a)-[:R]->(b) RETURN x, b.v",
            rows(&["[Int(1), Null]", "[Int(2), Null]"]),
        ),
    ];
    for (query, want) in cases {
        assert_eq!(bag(&db, query).await?, want, "{query}");
    }
    Ok(())
}

#[tokio::test]
async fn optional_match_row_identity_filtered_rows() -> Result<()> {
    let db = open().await?;
    let cases = [
        (
            "UNWIND [2, 3] AS x MATCH (a:A {v: 2}) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.v = x \
             RETURN x, b.v",
            rows(&["[Int(2), Int(2)]", "[Int(3), Null]"]),
        ),
        (
            "MATCH (a:A {v: 2}) UNWIND [2, 3] AS x OPTIONAL MATCH (a)-[:R]->(b) WHERE b.v = x \
             RETURN x, b.v",
            rows(&["[Int(2), Int(2)]", "[Int(3), Null]"]),
        ),
        (
            "UNWIND [3, 3] AS x MATCH (a:A {v: 2}) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.v = x \
             RETURN x, b.v",
            rows(&["[Int(3), Null]", "[Int(3), Null]"]),
        ),
        // Two hops; relationship uniqueness forbids reusing the single R edge.
        (
            "UNWIND [2, 3] AS x MATCH (a:A {v: 2}) \
             OPTIONAL MATCH (a)-[:R]->(b)<-[:R]-(c) WHERE b.v = x RETURN x, c.v",
            rows(&["[Int(2), Null]", "[Int(3), Null]"]),
        ),
    ];
    for (query, want) in cases {
        assert_eq!(bag(&db, query).await?, want, "{query}");
    }
    Ok(())
}

/// The row id is internal: it must not surface through `RETURN *` or `WITH *`.
#[tokio::test]
async fn optional_match_row_identity_column_stays_hidden() -> Result<()> {
    let db = open().await?;
    for query in [
        "MATCH (a:A {v: 1}) OPTIONAL MATCH (a)-[:R]->(b) RETURN *",
        "MATCH (a:A {v: 1}) OPTIONAL MATCH (a)-[:R]->(b) WITH * RETURN *",
    ] {
        let result = db.session().query(query).await?;
        for column in result.columns() {
            assert!(
                !column.starts_with("__"),
                "{query} exposed internal column {column}"
            );
        }
    }
    Ok(())
}
