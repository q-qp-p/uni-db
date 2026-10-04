// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Declaring a `NOT NULL` property must account for rows that are not yet
//! flushed.
//!
//! A `NOT NULL` property added to a label or edge type that already has rows
//! is recorded as nullable, because those rows have no value for it. "Already
//! has rows" was answered by counting flushed Lance tables only, so a label
//! whose rows were all still in L0 looked empty: the property was recorded
//! `NOT NULL`, and every flush after it failed with `Column 'req' is declared
//! as non-nullable but contains null values` — the unflushed rows could no
//! longer be written at all.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(not_null_on_unflushed)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, UniConfig, Value};

/// No automatic flush, so committed rows stay in L0 until the test flushes.
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
        .label("V")
        .property("k", DataType::Int)
        .edge_type("R", &["V"], &["V"])
        .property("w", DataType::Int)
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:V {k: 1})-[:R {w: 1}]->(:V {k: 2})")
        .await?;
    tx.commit().await?;
    Ok(db)
}

async fn assert_rows_survive_flush(db: &Uni, property_query: &str) -> Result<()> {
    db.flush().await?;
    let rows = db.session().query(property_query).await?;
    for row in rows.rows() {
        assert_eq!(row.values()[0], Value::Null, "{property_query}");
    }
    assert!(!rows.rows().is_empty(), "{property_query}: rows vanished");
    Ok(())
}

/// Through the schema builder, on a label and on an edge type.
#[tokio::test]
async fn not_null_on_unflushed_rows_via_schema_api() -> Result<()> {
    let db = open().await?;
    db.schema()
        .label("V")
        .property("req", DataType::Int)
        .apply()
        .await?;
    db.schema()
        .edge_type("R", &["V"], &["V"])
        .property("req", DataType::Int)
        .apply()
        .await?;
    assert_rows_survive_flush(&db, "MATCH (v:V) RETURN v.req").await?;
    assert_rows_survive_flush(&db, "MATCH ()-[r:R]->() RETURN r.req").await
}

/// Through Cypher DDL.
#[tokio::test]
async fn not_null_on_unflushed_rows_via_alter() -> Result<()> {
    let db = open().await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("ALTER LABEL V ADD PROPERTY req INT NOT NULL")
        .await?;
    tx.execute("ALTER EDGE TYPE R ADD PROPERTY req INT NOT NULL")
        .await?;
    tx.commit().await?;
    assert_rows_survive_flush(&db, "MATCH (v:V) RETURN v.req").await?;
    assert_rows_survive_flush(&db, "MATCH ()-[r:R]->() RETURN r.req").await
}
