// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! An entity an OPTIONAL MATCH did not bind is NULL everywhere it is used.
//!
//! A node or relationship variable is carried as a struct built from its
//! flattened columns, and `named_struct` is never NULL: when an OPTIONAL MATCH
//! found nothing, the variable became a struct whose every field was NULL.
//! `b` and `b IS NULL` read the flat columns and were right, but `labels(b)`
//! failed ("requires a node argument"), `keys(b)` returned `[]`, `keys(r)`
//! returned the relationship type's declared property names, and a list or map
//! holding the variable collapsed to NULL as a whole. A labelled target hid it:
//! its label filter re-nulled the column above the struct. The struct is now
//! NULL when the entity's id is.
//!
//! Found through the W3 topology relations (`labels(b)` in a probe).
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(optional_entity_is_null)'

// Rust guideline compliant

use std::collections::HashMap;

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("A")
        .property("id", DataType::Int)
        .label("B")
        .property("id", DataType::Int)
        .edge_type("K", &["A"], &["B"])
        .property("w", DataType::Int)
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:A {id: 1})-[:K {w: 5}]->(:B {id: 2}), (:A {id: 3})")
        .await?;
    tx.commit().await?;
    Ok(db)
}

async fn row(db: &Uni, query: &str) -> Result<Vec<Value>> {
    let result = db.session().query(query).await?;
    assert_eq!(result.rows().len(), 1, "{query}");
    Ok(result.rows()[0].values().to_vec())
}

#[tokio::test]
async fn optional_entity_is_null_in_functions_and_collections() -> Result<()> {
    let db = open().await?;
    let null_map = Value::Map(HashMap::from([("k".to_string(), Value::Null)]));
    for target in ["(b)", "(b:B)"] {
        let query = format!(
            "MATCH (a:A {{id: 3}}) OPTIONAL MATCH (a)-[r:K]->{target} \
             RETURN labels(b), keys(b), keys(r), [b, 1], {{k: b}}, [x IN [b] | x], {{k: r}}"
        );
        assert_eq!(
            row(&db, &query).await?,
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::List(vec![Value::Null, Value::Int(1)]),
                null_map.clone(),
                Value::List(vec![Value::Null]),
                null_map.clone(),
            ],
            "{query}"
        );
    }
    Ok(())
}

/// Control: a bound entity keeps its labels and keys.
#[tokio::test]
async fn optional_entity_is_null_bound_entity_unchanged() -> Result<()> {
    let db = open().await?;
    assert_eq!(
        row(
            &db,
            "MATCH (a:A {id: 1}) OPTIONAL MATCH (a)-[r:K]->(b) RETURN labels(b), keys(b), keys(r)"
        )
        .await?,
        vec![
            Value::List(vec![Value::String("B".to_string())]),
            Value::List(vec![Value::String("id".to_string())]),
            Value::List(vec![Value::String("w".to_string())]),
        ]
    );
    Ok(())
}
