// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! An `OPTIONAL MATCH` yields, per entering row, its matches or one NULL row —
//! decided over the whole clause, not per traversal step or per batch.
//!
//! Each traversal inside the clause decided "no match" for the rows it saw.
//! That is exact only for the step rows enter by; past it, one entering row is
//! spread over several rows and batches. So every dead end of a multi-step
//! pattern emitted its own NULL row, a comma-separated second path extended
//! rows the first path had already failed (returning a value where NULL was
//! due), and a leading `OPTIONAL MATCH` emitted a NULL row per batch — which
//! the openCypher TCK only showed (`Graph6[6]`) once the execution batch size
//! was made settable and run at 1. The clause now ends in one operator that
//! decides per entering row.
//!
//! The oracle needs no reference implementation: by definition,
//! `MATCH (a) OPTIONAL MATCH p` is the bag union of `MATCH (a) MATCH p` and
//! `MATCH (a) WHERE NOT EXISTS { MATCH p }` padded with NULL.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(optional_match_clause_close)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{Uni, UniConfig};

/// `a0` reaches `C` only through `b3`; `a5` has no `R`; `a6` has an `R` that
/// leads nowhere.
const GRAPH: &str = "CREATE (a:A {v: 0}), (a)-[:R]->(:B {v: 1}), (a)-[:R]->(:B {v: 2}), \
     (a)-[:R]->(b3:B {v: 3}), (b3)-[:S]->(:C {v: 9}), (b3)-[:S]->(:C {v: 8}), \
     (:A {v: 5}), (a6:A {v: 6}), (a6)-[:R]->(:B {v: 7})";

/// `(pattern, returned expression)`, each run as `MATCH (a:A) OPTIONAL MATCH …`.
const SHAPES: &[(&str, &str)] = &[
    ("(a)-[:R]->(b)", "b.v"),
    ("(a)-[:R]->(b)-[:S]->(c)", "c.v"),
    ("(a)-[:R]->(b)-[:S]->(c:C)", "c.v"),
    ("(a)-[:R]->(b), (b)-[:S]->(c)", "c.v"),
    ("(a)-[:R]->(b)-[:S*1..1]->(c)", "c.v"),
    ("(a)-[:R*1..1]->(b)-[:S]->(c)", "c.v"),
    ("(a)-[:R*1..2]->(b)-[:S]->(c)", "c.v"),
    ("(a)-[:R]->(b)-[:S*]->(c)", "c.v"),
    ("(a)-[:R]->()-[:S]->(c)", "c.v"),
    ("(a)-[:R]->(b)<-[:R]-(x)", "x.v"),
    ("(a)-[:R]->(b)-[:S]->(c), (a)-[:R]->(d)", "d.v"),
    ("(b:B)-[:S]->(c)", "c.v"),
    ("(a)-[:R]->(b)-[:S]->(c) WHERE c.v > 8", "c.v"),
    ("(a)-[:R]->(b)-[:S]->(c) WHERE a.v = 0", "c.v"),
    ("(a)-[:R]->(b) WHERE b.v > 100", "b.v"),
];

/// Leading clauses: the whole query is the one entering row.
const LEADING: &[(&str, &str)] = &[
    ("(a:A)-[:R]->(b)", "b.v"),
    ("(x:C)-[:S]->(y)", "y.v"),
    ("(a:A)-[:R]->(b)-[:S]->(c)", "c.v"),
    ("()-[r:S]->()", "r"),
];

async fn open(execution_batch_size: Option<usize>) -> Result<Uni> {
    let db = Uni::in_memory()
        .config(UniConfig {
            execution_batch_size,
            auto_flush_interval: None,
            ..Default::default()
        })
        .build()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute(GRAPH).await?;
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

async fn check(execution_batch_size: Option<usize>) -> Result<()> {
    let db = open(execution_batch_size).await?;
    let mut failures = Vec::new();
    for (pattern, ret) in SHAPES {
        let (body, filter) = match pattern.split_once(" WHERE ") {
            Some((body, filter)) => (body, format!(" WHERE {filter}")),
            None => (*pattern, String::new()),
        };
        let query = format!("MATCH (a:A) OPTIONAL MATCH {pattern} RETURN a.v, {ret}");
        let mut want = bag(
            &db,
            &format!("MATCH (a:A) MATCH {body}{filter} RETURN a.v, {ret}"),
        )
        .await?;
        want.extend(
            bag(
                &db,
                &format!(
                    "MATCH (a:A) WHERE NOT EXISTS {{ MATCH {body}{filter} }} RETURN a.v, null"
                ),
            )
            .await?,
        );
        want.sort();
        let got = bag(&db, &query).await?;
        if got != want {
            failures.push(format!("{query}\n  got  {got:?}\n  want {want:?}"));
        }
    }
    for (pattern, ret) in LEADING {
        let query = format!("OPTIONAL MATCH {pattern} RETURN {ret}");
        let mut want = bag(&db, &format!("MATCH {pattern} RETURN {ret}")).await?;
        if want.is_empty() {
            want.push("[Null]".to_string());
        }
        let got = bag(&db, &query).await?;
        if got != want {
            failures.push(format!("{query}\n  got  {got:?}\n  want {want:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "execution_batch_size {execution_batch_size:?}:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[tokio::test]
async fn optional_match_clause_close_default_batches() -> Result<()> {
    check(None).await
}

/// A batch of one row puts every row of a fanned-out entering row in a
/// different batch.
#[tokio::test]
async fn optional_match_clause_close_one_row_batches() -> Result<()> {
    check(Some(1)).await
}
