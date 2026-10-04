// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A non-DETACH `DELETE` must see relationships created earlier in the same
//! transaction.
//!
//! The "node still has relationships" check read the adjacency CSR and its
//! overlay, which a transaction's own edges reach only at commit. So
//! `CREATE (a)-[:R]->(b)` followed by `DELETE a` in one transaction passed the
//! check, and deleting the vertex then cascaded the edge away without a word —
//! a `DELETE` that behaved as `DETACH DELETE`.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(delete_sees_tx_local_edges)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("V")
        .property("k", DataType::Int)
        .edge_type("R", &["V"], &["V"])
        .done()
        .apply()
        .await?;
    Ok(db)
}

async fn count(db: &Uni, q: &str) -> Result<i64> {
    let r = db.session().query(q).await?;
    match r.rows().first().map(|row| row.values()[0].clone()) {
        Some(Value::Int(n)) => Ok(n),
        other => panic!("{q}: {other:?}"),
    }
}

#[tokio::test]
async fn delete_sees_tx_local_edges_rejects_connected_node() -> Result<()> {
    let db = open().await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:V {k: 1})-[:R]->(:V {k: 2})").await?;
    let err = tx
        .execute("MATCH (a:V {k: 1}) DELETE a")
        .await
        .expect_err("the node has a relationship created in this transaction");
    assert!(err.to_string().contains("still has relationships"), "{err}");
    Ok(())
}

#[tokio::test]
async fn delete_sees_tx_local_edges_allows_after_edge_deleted() -> Result<()> {
    let db = open().await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:V {k: 1})-[:R]->(:V {k: 2})").await?;
    tx.execute("MATCH (:V {k: 1})-[r:R]->() DELETE r").await?;
    tx.execute("MATCH (a:V {k: 1}) DELETE a").await?;
    tx.commit().await?;
    assert_eq!(count(&db, "MATCH (v:V) RETURN count(v)").await?, 1);
    Ok(())
}

#[tokio::test]
async fn delete_sees_tx_local_edges_detach_removes_them() -> Result<()> {
    let db = open().await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:V {k: 1})-[:R]->(:V {k: 2})").await?;
    tx.execute("MATCH (a:V {k: 1}) DETACH DELETE a").await?;
    tx.commit().await?;
    assert_eq!(count(&db, "MATCH ()-[r:R]->() RETURN count(r)").await?, 0);
    assert_eq!(count(&db, "MATCH (v:V) RETURN count(v)").await?, 1);
    Ok(())
}

/// Control: an edge committed before the transaction has always blocked it.
#[tokio::test]
async fn delete_sees_tx_local_edges_control_committed_edge() -> Result<()> {
    let db = open().await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:V {k: 1})-[:R]->(:V {k: 2})").await?;
    tx.commit().await?;
    let tx = session.tx().await?;
    assert!(tx.execute("MATCH (a:V {k: 1}) DELETE a").await.is_err());
    Ok(())
}
