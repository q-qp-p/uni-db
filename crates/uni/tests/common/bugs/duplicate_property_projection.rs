// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A predicate that names one property twice reads it once.
//!
//! The property fetch listed each property a query reads as it was given. A
//! repeated equality on the labelled target of a variable-length hop from an
//! already-bound node (`MATCH (a) MATCH (a)-[*1..2]->(b:N) WHERE b.id = 2 AND
//! b.id = 2`) listed `id` twice, and Lance rejected the projection: "Duplicate column
//! name: id". The fetch now drops repeats.
//!
//! Found by the `named_rel` relation of `metamorphic::dqp::topo`.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(duplicate_property_projection)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

#[tokio::test]
async fn duplicate_property_projection_is_read_once() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("N")
        .property("id", DataType::Int)
        .edge_type("K", &["N"], &["N"])
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (a:N {id: 1})-[:K]->(b:N {id: 2})-[:K]->(:N {id: 3})")
        .await?;
    tx.commit().await?;
    db.flush().await?;

    for (query, want) in [
        (
            "MATCH (a:N) MATCH (a)-[:K*1..2]->(b:N) WHERE b.id = 2 AND b.id = 2 \
             RETURN count(*)",
            1,
        ),
        (
            "MATCH (a:N) MATCH (a)-[:K*1..2]->(b:N) WHERE b.id < 0 AND (b.id = 0 AND b.id = 0) \
             RETURN count(*)",
            0,
        ),
    ] {
        let result = db.session().query(query).await?;
        assert_eq!(result.rows()[0].values()[0], Value::Int(want), "{query}");
    }
    Ok(())
}
