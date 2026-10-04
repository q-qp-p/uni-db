// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Full-text search matches a node's *current* text.
//!
//! When an unflushed update replaced a matching text with a non-matching one,
//! the flushed index hit survived: the L0 merge only recorded updated nodes
//! whose new text still matched, so it had nothing to override the stale hit
//! with. The query kept returning a node whose text no longer contains the
//! term.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(fts_sees_current_text)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, UniConfig, Value};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory()
        .config(UniConfig {
            auto_flush_threshold: usize::MAX,
            auto_flush_interval: None,
            ..Default::default()
        })
        .build()
        .await?;
    db.schema()
        .label("Doc")
        .property("id", DataType::Int)
        .property_nullable("content", DataType::String)
        .property("tag", DataType::String)
        .apply()
        .await?;
    run(
        &db,
        "CREATE FULLTEXT INDEX doc_fts FOR (d:Doc) ON EACH [d.content]",
    )
    .await?;
    run(
        &db,
        "CREATE (:Doc {id: 1, content: 'foo apple', tag: 'a'}), \
         (:Doc {id: 2, content: 'foo cherry', tag: 'b'}), \
         (:Doc {id: 3, content: 'other text', tag: 'c'})",
    )
    .await?;
    db.flush().await?;
    Ok(db)
}

async fn run(db: &Uni, stmt: &str) -> Result<()> {
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute(stmt).await?;
    tx.commit().await?;
    Ok(())
}

async fn hits(db: &Uni, term: &str) -> Result<Vec<i64>> {
    let result = db
        .session()
        .query(&format!(
            "CALL uni.fts.query('Doc', 'content', '{term}', 10) YIELD node RETURN node.id AS id"
        ))
        .await?;
    let mut ids: Vec<i64> = result
        .rows()
        .iter()
        .map(|r| match &r.values()[0] {
            Value::Int(i) => *i,
            other => panic!("id: {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

#[tokio::test]
async fn fts_sees_current_text_after_unflushed_update() -> Result<()> {
    let db = open().await?;
    run(&db, "MATCH (d:Doc {id: 1}) SET d.content = 'bar banana'").await?;
    assert_eq!(
        hits(&db, "foo").await?,
        vec![2],
        "the updated text no longer matches"
    );
    run(&db, "MATCH (d:Doc {id: 2}) SET d.content = null").await?;
    assert_eq!(
        hits(&db, "foo").await?,
        Vec::<i64>::new(),
        "a NULL text matches nothing"
    );
    run(&db, "MATCH (d:Doc {id: 3}) SET d.content = 'foo now'").await?;
    assert_eq!(
        hits(&db, "foo").await?,
        vec![3],
        "an update can also create a match"
    );
    Ok(())
}

#[tokio::test]
async fn fts_sees_current_text_after_flushed_update() -> Result<()> {
    let db = open().await?;
    run(&db, "MATCH (d:Doc {id: 1}) SET d.content = 'bar banana'").await?;
    db.flush().await?;
    assert_eq!(hits(&db, "foo").await?, vec![2]);
    Ok(())
}

/// An update to another property leaves the indexed text, and the hit, alone.
#[tokio::test]
async fn fts_sees_current_text_other_property_update_keeps_hit() -> Result<()> {
    let db = open().await?;
    run(&db, "MATCH (d:Doc {id: 1}) SET d.tag = 'z'").await?;
    assert_eq!(hits(&db, "foo").await?, vec![1, 2]);
    Ok(())
}

/// The vector search twin: a flushed update that moves a vector away must not
/// leave the old row's vector matching.
#[tokio::test]
async fn fts_sees_current_text_vector_after_flushed_update() -> Result<()> {
    let db = Uni::in_memory()
        .config(UniConfig {
            auto_flush_threshold: usize::MAX,
            auto_flush_interval: None,
            ..Default::default()
        })
        .build()
        .await?;
    db.schema()
        .label("V")
        .property("id", DataType::Int)
        .property("emb", DataType::Vector { dimensions: 2 })
        .apply()
        .await?;
    run(
        &db,
        "CREATE (:V {id: 1, emb: [1.0, 0.0]}), (:V {id: 2, emb: [0.6, 0.4]}), \
         (:V {id: 3, emb: [0.0, 1.0]})",
    )
    .await?;
    db.flush().await?;
    run(&db, "MATCH (v:V {id: 1}) SET v.emb = [0.0, 1.0]").await?;
    db.flush().await?;
    let result = db
        .session()
        .query("CALL uni.vector.query('V', 'emb', [1.0, 0.0], 1) YIELD node RETURN node.id")
        .await?;
    let ids: Vec<Value> = result
        .rows()
        .iter()
        .map(|r| r.values()[0].clone())
        .collect();
    assert_eq!(
        ids,
        vec![Value::Int(2)],
        "node 1 no longer has the [1, 0] vector"
    );
    Ok(())
}
