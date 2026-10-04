// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A vertex `UNIQUE` key is free again once no live vertex holds it.
//!
//! The uniqueness probe treated "some row matches the key" as "the key is
//! taken". Rows outlive the value: an unflushed `DELETE` leaves the flushed row
//! in place until the tombstone is flushed, vertex tables are append-only so a
//! flushed `SET` leaves the older row version behind, and the in-memory key
//! index kept a key after an unflushed `SET` moved it. Each rejected a key no
//! vertex held. Candidates are now resolved to their current values first.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(unique_key_reuse)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, UniConfig};

/// No automatic flush, so the test decides what is flushed.
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
        .label("U")
        .property("code", DataType::String)
        .apply()
        .await?;
    run(
        &db,
        "CREATE CONSTRAINT u_code ON (u:U) ASSERT u.code IS UNIQUE",
    )
    .await?;
    Ok(db)
}

async fn run(db: &Uni, stmt: &str) -> Result<()> {
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute(stmt).await?;
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn unique_key_reuse_after_unflushed_delete() -> Result<()> {
    let db = open().await?;
    run(&db, "CREATE (:U {code: 'k'})").await?;
    db.flush().await?;
    run(&db, "MATCH (u:U {code: 'k'}) DELETE u").await?;
    run(&db, "CREATE (:U {code: 'k'})").await?;
    Ok(())
}

#[tokio::test]
async fn unique_key_reuse_after_flushed_set() -> Result<()> {
    let db = open().await?;
    run(&db, "CREATE (:U {code: 'k'})").await?;
    db.flush().await?;
    run(&db, "MATCH (u:U {code: 'k'}) SET u.code = 'moved'").await?;
    db.flush().await?;
    run(&db, "CREATE (:U {code: 'k'})").await?;
    Ok(())
}

#[tokio::test]
async fn unique_key_reuse_after_unflushed_set() -> Result<()> {
    let db = open().await?;
    run(&db, "CREATE (:U {code: 'k'})").await?;
    run(&db, "MATCH (u:U {code: 'k'}) SET u.code = 'moved'").await?;
    run(&db, "CREATE (:U {code: 'k'})").await?;
    Ok(())
}

/// Controls: a key a live vertex still holds stays taken, wherever it lives.
#[tokio::test]
async fn unique_key_reuse_live_duplicates_still_rejected() -> Result<()> {
    let db = open().await?;
    run(&db, "CREATE (:U {code: 'unflushed'})").await?;
    assert!(run(&db, "CREATE (:U {code: 'unflushed'})").await.is_err());

    run(&db, "CREATE (:U {code: 'flushed'})").await?;
    db.flush().await?;
    assert!(run(&db, "CREATE (:U {code: 'flushed'})").await.is_err());

    run(&db, "CREATE (:U {code: 'a'})").await?;
    db.flush().await?;
    run(&db, "MATCH (u:U {code: 'a'}) SET u.code = 'b'").await?;
    assert!(
        run(&db, "CREATE (:U {code: 'b'})").await.is_err(),
        "the key a SET moved a vertex onto is taken"
    );

    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:U {code: 'tx'})").await?;
    assert!(tx.execute("CREATE (:U {code: 'tx'})").await.is_err());
    Ok(())
}
