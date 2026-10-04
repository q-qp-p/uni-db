// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! An OPTIONAL traversal emitted in chunks decides "no match" once.
//!
//! When one input batch expands to more rows than the execution batch size,
//! the traversal hydrates and emits its expansions a chunk at a time (#202).
//! Each chunk was built as if it were the whole expansion set, so every input
//! row whose matches fell in *another* chunk got a NULL row — once per chunk —
//! beside its real matches. At the default batch size of 8192 that takes an
//! input batch expanding past 8192 rows, e.g. an OPTIONAL MATCH over a hub;
//! found by the W3 topology relations at an execution batch size of 2, where
//! `MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b)` returned 90 rows for 70.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(optional_traverse_chunked)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, UniConfig};

async fn open(execution_batch_size: Option<usize>) -> Result<Uni> {
    let db = Uni::in_memory()
        .config(UniConfig {
            execution_batch_size,
            ..Default::default()
        })
        .build()
        .await?;
    db.schema()
        .label("A")
        .property("id", DataType::Int)
        .label("B")
        .property("id", DataType::Int)
        .edge_type("K", &["A"], &["B"])
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute(
        "CREATE (a:A {id: 1}), (:A {id: 2}), (a)-[:K]->(:B {id: 10}), \
         (a)-[:K]->(:B {id: 11}), (a)-[:K]->(:B {id: 12})",
    )
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

#[tokio::test]
async fn optional_traverse_chunked_emits_one_null_per_unmatched_row() -> Result<()> {
    let want = vec![
        "[Int(1), Int(10)]".to_string(),
        "[Int(1), Int(11)]".to_string(),
        "[Int(1), Int(12)]".to_string(),
        "[Int(2), Null]".to_string(),
    ];
    // Batch sizes below `a1`'s three expansions split them into chunks; the
    // default does not and is the control.
    for batch in [Some(1), Some(2), None] {
        let db = open(batch).await?;
        for query in [
            "MATCH (a:A) OPTIONAL MATCH (a)-[:K]->(b) RETURN a.id, b.id",
            "MATCH (a:A) OPTIONAL MATCH (a)-[:K]->(b:B) RETURN a.id, b.id",
        ] {
            assert_eq!(bag(&db, query).await?, want, "batch {batch:?}: {query}");
        }
    }
    Ok(())
}
