// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! `elementId(n)` works on a node or relationship a MATCH bound.
//!
//! `elementId` is registered as needing only its argument's identity, so the
//! planner never materializes the entity itself — but `elementId` was compiled
//! as a function over the whole entity. Every call on a scan- or
//! traversal-bound variable failed to plan: "No field named n". It now reads
//! the identity column, as `id(n)` does, and returns it as a string.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(element_id_of_bound_variables)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

#[tokio::test]
async fn element_id_of_bound_variables_is_the_id_as_a_string() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("N")
        .property("k", DataType::Int)
        .edge_type("R", &["N"], &["N"])
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:N {k: 1})-[:R]->(:N {k: 2}), (:N {k: 3})")
        .await?;
    tx.commit().await?;

    let result = db
        .session()
        .query(
            "MATCH (a:N)-[r:R]->(b:N) \
             RETURN elementId(a), id(a), elementId(r), id(r), elementId(b), id(b)",
        )
        .await?;
    let row = result.rows()[0].values().to_vec();
    for pair in row.chunks(2) {
        let Value::Int(id) = pair[1] else {
            panic!("id() returned {:?}", pair[1]);
        };
        assert_eq!(pair[0], Value::String(id.to_string()));
    }

    let result = db
        .session()
        .query("MATCH (a:N {k: 3}) OPTIONAL MATCH (a)-[r:R]->(b) RETURN elementId(b), elementId(r)")
        .await?;
    assert_eq!(result.rows()[0].values(), &[Value::Null, Value::Null]);
    Ok(())
}
