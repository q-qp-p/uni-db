// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! `sum` over no non-null value is 0.
//!
//! Cypher's `sum`, like `count`, is 0 when there is nothing to add (Neo4j);
//! SQL's, and so DataFusion's, is NULL. uni-db returned NULL from DataFusion's
//! `sum` and from its own Cypher-value `sum`, while its row executor's
//! accumulator returned 0 — so the answer also depended on the execution
//! path. The NULL spread: `sum(x) + 1` was NULL and `WHERE total < 10` dropped
//! the group. `min`, `max` and `avg` of nothing stay NULL.
//!
//! A `sum(...) OVER (...)` window is held to the same rule, and also cast
//! every argument to an integer before adding it, so a float window sum was
//! truncated: `0.5` and `1.25` summed to `1`.
//!
//! Found by the `aggregate` relation of `metamorphic::dqp::topo`.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(sum_of_nothing_is_zero)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("N")
        .property("g", DataType::Int)
        .property_nullable("i", DataType::Int)
        .property_nullable("f", DataType::Float)
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:N {g: 1, i: 2, f: 0.5}), (:N {g: 2}), (:N {g: 2})")
        .await?;
    tx.commit().await?;
    Ok(db)
}

async fn rows(db: &Uni, query: &str) -> Result<Vec<Vec<Value>>> {
    let result = db.session().query(query).await?;
    Ok(result.rows().iter().map(|r| r.values().to_vec()).collect())
}

#[tokio::test]
async fn sum_of_nothing_is_zero_on_every_path() -> Result<()> {
    let db = open().await?;
    let cases: &[(&str, Vec<Vec<Value>>)] = &[
        // No rows at all.
        (
            "MATCH (n:Nope) RETURN sum(n.x) AS s",
            vec![vec![Value::Int(0)]],
        ),
        // A typed integer and a typed float column, all NULL in group 2.
        (
            "MATCH (n:N) RETURN n.g AS g, sum(n.i) AS si, sum(n.f) AS sf ORDER BY g",
            vec![
                vec![Value::Int(1), Value::Int(2), Value::Float(0.5)],
                vec![Value::Int(2), Value::Int(0), Value::Float(0.0)],
            ],
        ),
        // Cypher values (no declared type).
        (
            "UNWIND [null, null] AS x RETURN sum(x) AS s",
            vec![vec![Value::Int(0)]],
        ),
        ("RETURN sum(null) AS s", vec![vec![Value::Int(0)]]),
        (
            "MATCH (n:N {g: 2}) RETURN sum(DISTINCT n.i) AS s",
            vec![vec![Value::Int(0)]],
        ),
        // The result takes part in arithmetic and filters as 0.
        (
            "MATCH (n:N {g: 2}) WITH sum(n.i) AS t WHERE t < 10 RETURN t + 1 AS s",
            vec![vec![Value::Int(1)]],
        ),
        // Controls: the other aggregates of nothing stay NULL.
        (
            "MATCH (n:N {g: 2}) RETURN min(n.i), max(n.i), avg(n.i), count(n.i)",
            vec![vec![Value::Null, Value::Null, Value::Null, Value::Int(0)]],
        ),
    ];
    for (query, want) in cases {
        assert_eq!(&rows(&db, query).await?, want, "{query}");
    }
    Ok(())
}

#[tokio::test]
async fn sum_of_nothing_is_zero_in_a_window() -> Result<()> {
    let db = open().await?;
    let tx = db.session().tx().await?;
    tx.execute("CREATE (:N {g: 1, i: 3, f: 1.25})").await?;
    tx.commit().await?;
    assert_eq!(
        rows(
            &db,
            "MATCH (n:N) RETURN n.g AS g, sum(n.i) OVER (PARTITION BY n.g) AS si, \
             sum(n.f) OVER (PARTITION BY n.g) AS sf, avg(n.f) OVER (PARTITION BY n.g) AS af \
             ORDER BY g"
        )
        .await?,
        vec![
            vec![
                Value::Int(1),
                Value::Int(5),
                Value::Float(1.75),
                Value::Float(0.875)
            ],
            vec![
                Value::Int(1),
                Value::Int(5),
                Value::Float(1.75),
                Value::Float(0.875)
            ],
            vec![Value::Int(2), Value::Int(0), Value::Float(0.0), Value::Null],
            vec![Value::Int(2), Value::Int(0), Value::Float(0.0), Value::Null],
        ]
    );
    Ok(())
}
