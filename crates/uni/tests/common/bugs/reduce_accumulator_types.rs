// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! `reduce` keeps each value's own type.
//!
//! `reduce` decoded an untyped list's elements *as the accumulator's type*, so
//! `reduce(s = 0, v IN [1.5, 2.5] | s + v)` truncated every float and returned
//! 3. With `null` as the start — the usual way to write a min or max — the
//! accumulator had no type at all: the list was decoded into an array whose
//! declared item type disagreed with its values, which panicked in Arrow, and
//! rows whose lists ended early could not be merged with rows that had moved
//! on. `reduce` now stays typed only when the element type is known and the
//! body keeps the accumulator's type, and otherwise runs over Cypher values.
//!
//! Found by the `aggregate` relation of `metamorphic::dqp::topo` (min/max/sum
//! against `reduce` over `collect`).
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(reduce_accumulator_types)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{Uni, Value};

async fn column(db: &Uni, query: &str) -> Result<Vec<Value>> {
    let result = db.session().query(query).await?;
    Ok(result
        .rows()
        .iter()
        .map(|r| r.values()[0].clone())
        .collect())
}

#[tokio::test]
async fn reduce_accumulator_types_are_kept_per_value() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    let cases: &[(&str, &[Value])] = &[
        (
            "RETURN reduce(s = 0, v IN [1.5, 2.5] | s + v)",
            &[Value::Float(4.0)],
        ),
        (
            "UNWIND [[1.5], [1.5, 2.5], []] AS l RETURN reduce(s = 0, v IN l | s + v)",
            &[Value::Float(1.5), Value::Float(4.0), Value::Int(0)],
        ),
        (
            "UNWIND [[3, 1, 2], [], [5], null] AS l \
             RETURN reduce(m = null, v IN l | CASE WHEN m IS NULL OR v < m THEN v ELSE m END)",
            &[Value::Int(1), Value::Null, Value::Int(5), Value::Null],
        ),
        (
            "RETURN reduce(acc = [], v IN [1, 2] | acc + [v * 2])",
            &[Value::List(vec![Value::Int(2), Value::Int(4)])],
        ),
        // Typed and type-preserving: unchanged.
        (
            "RETURN reduce(s = 0, v IN [1, 2, 3] | s + v)",
            &[Value::Int(6)],
        ),
    ];
    for (query, want) in cases {
        assert_eq!(column(&db, query).await?, want.to_vec(), "{query}");
    }
    Ok(())
}

/// Over a collected, per-group list — the shape that panicked.
#[tokio::test]
async fn reduce_accumulator_types_null_start_over_collect() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    let result = db
        .session()
        .query(
            "UNWIND [[1, 30], [1, 10], [2, 7], [3, null]] AS p \
             WITH p[0] AS g, collect(p[1]) AS xs \
             RETURN g, reduce(m = null, v IN xs | CASE WHEN m IS NULL OR v > m THEN v ELSE m END) AS hi \
             ORDER BY g",
        )
        .await?;
    let rows: Vec<Vec<Value>> = result.rows().iter().map(|r| r.values().to_vec()).collect();
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Int(30)],
            vec![Value::Int(2), Value::Int(7)],
            vec![Value::Int(3), Value::Null],
        ]
    );
    Ok(())
}
