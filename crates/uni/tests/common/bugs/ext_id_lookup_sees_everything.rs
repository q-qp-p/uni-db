// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! An unlabelled `(n {ext_id: ...})` behaves like any other node match.
//!
//! It had its own lookup operator, which read flushed rows only — a committed
//! but unflushed vertex was not found — projected no properties
//! (`RETURN n.name` failed to plan with `No field named "n.name"`), and
//! returned every property it did carry as a string. It now plans as the
//! ordinary unlabelled scan, with the equality pushed to the `ext_id` index.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(ext_id_lookup_sees)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{Uni, UniConfig, Value};

async fn open() -> Result<Uni> {
    Ok(Uni::in_memory()
        .config(UniConfig {
            auto_flush_threshold: usize::MAX,
            auto_flush_interval: None,
            ..Default::default()
        })
        .build()
        .await?)
}

async fn run(db: &Uni, stmt: &str) -> Result<()> {
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute(stmt).await?;
    tx.commit().await?;
    Ok(())
}

async fn row(db: &Uni, ext_id: &str) -> Result<Vec<Vec<Value>>> {
    let result = db
        .session()
        .query(&format!(
            "MATCH (n {{ext_id: '{ext_id}'}}) RETURN n.name AS name, n.age AS age"
        ))
        .await?;
    Ok(result.rows().iter().map(|r| r.values().to_vec()).collect())
}

#[tokio::test]
async fn ext_id_lookup_sees_everything_unflushed_and_flushed() -> Result<()> {
    let db = open().await?;
    run(&db, "CREATE ({ext_id: 'a', name: 'Ann', age: 30})").await?;
    let want = vec![vec![Value::String("Ann".into()), Value::Int(30)]];
    assert_eq!(row(&db, "a").await?, want, "unflushed");
    db.flush().await?;
    assert_eq!(row(&db, "a").await?, want, "flushed");
    Ok(())
}

#[tokio::test]
async fn ext_id_lookup_sees_everything_after_changes() -> Result<()> {
    let db = open().await?;
    run(&db, "CREATE ({ext_id: 'a', name: 'Ann', age: 30})").await?;
    db.flush().await?;
    run(&db, "MATCH (n {ext_id: 'a'}) SET n.ext_id = 'b'").await?;
    assert!(
        row(&db, "a").await?.is_empty(),
        "the old ext_id no longer matches"
    );
    assert_eq!(row(&db, "b").await?.len(), 1, "the new ext_id matches");
    db.flush().await?;
    assert!(
        row(&db, "a").await?.is_empty(),
        "nor after the change is flushed"
    );
    assert_eq!(row(&db, "b").await?.len(), 1);
    run(&db, "MATCH (n {ext_id: 'b'}) DETACH DELETE n").await?;
    assert!(
        row(&db, "b").await?.is_empty(),
        "a deleted vertex does not match"
    );
    Ok(())
}
