// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A user's map is a map, whatever its keys are called.
//!
//! The engine recognised an entity map — one encoding of a node or edge — by
//! an id key alone: any map with `_id`, `_vid` or `vid` was "vertex N" and any
//! with `_eid` "edge N", their other keys ignored. So `{_id: 0, x: 1} =
//! {_id: 0, x: 2}` was true, `IN` agreed, `collect(DISTINCT ...)` kept one of
//! them, and `UNWIND` / `RETURN` turned the map into a node. `_id` is a common
//! key in imported JSON, and `properties(n)` of a node with an `_id` property
//! is exactly such a map. An entity map is now recognised by a structural
//! tell: `_labels` for a vertex, a type or endpoints for an edge.
//!
//! `collect(DISTINCT ...)` also keyed values by their display string, merging
//! values that print alike (`1` and `'1'`); it now keys by structure.
//!
//! Found by re-probing a W1 audit suspect on the W3 topology fixture.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(user_maps_are_not_entities)'

// Rust guideline compliant

use std::collections::HashMap;

use anyhow::Result;
use uni_db::{Uni, Value};

async fn row(db: &Uni, query: &str) -> Result<Vec<Value>> {
    let result = db.session().query(query).await?;
    assert_eq!(result.rows().len(), 1, "{query}");
    Ok(result.rows()[0].values().to_vec())
}

fn map(entries: &[(&str, i64)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::Int(*v)))
            .collect::<HashMap<_, _>>(),
    )
}

#[tokio::test]
async fn user_maps_are_not_entities_in_comparison_and_distinct() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    for key in ["_id", "_vid", "vid", "_eid"] {
        let query = format!(
            "RETURN {{{key}: 0, x: 1}} = {{{key}: 0, x: 2}} AS eq, \
             {{{key}: 0, x: 1}} IN [{{{key}: 0, x: 2}}] AS inside, \
             size(collect(DISTINCT {{{key}: 0, x: 1}}) + collect(DISTINCT {{{key}: 0, x: 2}})) AS two"
        );
        assert_eq!(
            row(&db, &query).await?,
            vec![Value::Bool(false), Value::Bool(false), Value::Int(2)],
            "{query}"
        );
        let query = format!(
            "UNWIND [{{{key}: 0, x: 1}}, {{{key}: 0, x: 2}}] AS m RETURN size(collect(DISTINCT m))"
        );
        assert_eq!(row(&db, &query).await?, vec![Value::Int(2)], "{query}");
    }
    assert_eq!(
        row(&db, "UNWIND [{_vid: 0, x: 1}] AS m RETURN m").await?,
        vec![map(&[("_vid", 0), ("x", 1)])]
    );
    assert_eq!(
        row(&db, "RETURN {_eid: 0, x: 1} AS m").await?,
        vec![map(&[("_eid", 0), ("x", 1)])]
    );
    // Values that print alike are still distinct; equal maps are one.
    assert_eq!(
        row(
            &db,
            "UNWIND [1, '1', [1], ['1'], {a: 1, b: 2}, {b: 2, a: 1}] AS v \
             RETURN size(collect(DISTINCT v))"
        )
        .await?,
        vec![Value::Int(5)]
    );
    Ok(())
}

/// `properties(n)` of nodes whose own properties are named `_id` / `vid`.
#[tokio::test]
async fn user_maps_are_not_entities_properties_with_id_keys() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    let tx = db.session().tx().await?;
    tx.execute(
        "CREATE (:S {_id: 7, x: 1}), (:S {_id: 7, x: 2}), (:S {vid: 1, x: 3}), (:S {vid: 1, x: 4})",
    )
    .await?;
    tx.commit().await?;
    assert_eq!(
        row(
            &db,
            "MATCH (s:S) RETURN size(collect(DISTINCT properties(s))), \
             count(DISTINCT properties(s)), size(collect(DISTINCT s))"
        )
        .await?,
        vec![Value::Int(4), Value::Int(4), Value::Int(4)]
    );
    Ok(())
}
