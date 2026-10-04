// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A chunked label scan must see the label's unflushed (L0) rows.
//!
//! `GraphScanExec` walks a large label in `_vid` ranges (#214). Two of its
//! decisions consulted only flushed storage:
//!
//! * **Sizing** counted rows with `count_rows` on the backend, so a label whose
//!   rows were all still in L0 sized as 0 and was built as one batch — the
//!   unbounded materialization #214 exists to avoid, chosen or not depending
//!   on whether a background flush happened to land first.
//! * **End of walk** asked the backend whether the label had any row at or
//!   above the next range. Vids are allocated globally, so a label's newer rows
//!   can sit past a gap filled by another label. Reaching an empty range in
//!   that gap, the walk asked flushed storage, heard "no", and stopped — never
//!   reaching the L0 rows above the gap.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(scan_range_walk)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, UniConfig, Value};

/// No automatic flush, so what is in L0 stays there until the test flushes.
fn config() -> UniConfig {
    UniConfig {
        auto_flush_threshold: usize::MAX,
        auto_flush_interval: None,
        ..Default::default()
    }
}

async fn create(db: &Uni, label: &str, from: i64, to: i64) -> Result<()> {
    let tx = db.session().tx().await?;
    tx.execute(&format!(
        "UNWIND range({from}, {to}) AS i CREATE (:{label} {{n: i}})"
    ))
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn count(db: &Uni, query: &str) -> Result<i64> {
    let result = db.session().query(query).await?;
    match result.rows().first().map(|r| r.values()[0].clone()) {
        Some(Value::Int(n)) => Ok(n),
        other => panic!("{query}: expected one integer, got {other:?}"),
    }
}

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().config(config()).build().await?;
    db.schema()
        .label("P")
        .property("n", DataType::Int)
        .label("Q")
        .property("n", DataType::Int)
        .apply()
        .await?;
    Ok(db)
}

/// Flushed `P` rows, then a block of `Q` vids, then unflushed `P` rows above
/// that gap. Every `P` row must come back, through both a counting and a
/// row-returning query, and through a cursor.
#[tokio::test]
async fn scan_range_walk_reaches_l0_rows_past_a_vid_gap() -> Result<()> {
    let db = open().await?;
    // More flushed rows than one scan slice (the session batch size, 8192),
    // so the label is walked in ranges; then a block of other-label vids wider
    // than the range the walk will have doubled to; then new rows above it.
    create(&db, "P", 0, 8_999).await?;
    db.flush().await?;
    create(&db, "Q", 0, 59_999).await?;
    db.flush().await?;
    create(&db, "P", 9_000, 9_006).await?;

    assert_eq!(count(&db, "MATCH (p:P) RETURN count(p)").await?, 9007);
    assert_eq!(
        count(&db, "MATCH (p:P) WHERE p.n >= 9000 RETURN count(*)").await?,
        7
    );
    let rows = db.session().query("MATCH (p:P) RETURN p.n").await?;
    assert_eq!(rows.rows().len(), 9007);

    let mut cursor = db
        .session()
        .query_with("MATCH (p:P) RETURN p.n")
        .cursor()
        .await?;
    let mut streamed = 0usize;
    while let Some(batch) = cursor.next_batch().await {
        streamed += batch?.len();
    }
    assert_eq!(streamed, 9007, "cursor");
    Ok(())
}
