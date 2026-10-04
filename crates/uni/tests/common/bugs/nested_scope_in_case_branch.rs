// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Comprehensions, quantifiers and `reduce` work inside a `CASE` branch, and
//! mix numeric types.
//!
//! These expressions compile their body against the input schema plus their
//! own variable and keep the body out of `children()`. DataFusion's `CASE`
//! evaluates each branch on a batch projected down to the columns its
//! children reference, renumbered. The hidden body then read a batch that
//! lacked its columns or held them elsewhere: "Column references column 'v' at
//! index 6 but input schema only has 2 columns", or — for a pattern
//! comprehension whose anchor was dropped — an empty result for every row.
//! The outer columns a body reads are now exposed, and the body reads the
//! batch realigned to its compiled layout.
//!
//! Separately, an operand holding one of these expressions is compiled
//! outside the logical path that coerces types, so `size([x IN xs | x]) * 1.5`
//! failed ("Invalid arithmetic operation: Int64 * Float64"), as did `> 1.5`.
//!
//! Found through the `aggregate` relation of `metamorphic::dqp::topo`.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(nested_scope_in_case_branch)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("N")
        .property("id", DataType::Int)
        .property("v", DataType::Int)
        .edge_type("K", &["N"], &["N"])
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute(
        "CREATE (a:N {id: 1, v: 10}), (b:N {id: 2, v: 20}), (c:N {id: 3, v: 30}), \
         (a)-[:K]->(b), (a)-[:K]->(c), (b)-[:K]->(c)",
    )
    .await?;
    tx.commit().await?;
    Ok(db)
}

/// A list's elements sorted, so two lists compare as bags; other values as is.
fn as_bag(value: &Value) -> Value {
    match value {
        Value::List(items) => {
            let mut items = items.clone();
            items.sort_by_key(|v| format!("{v:?}"));
            Value::List(items)
        }
        other => other.clone(),
    }
}

async fn rows(db: &Uni, query: &str) -> Result<Vec<Vec<Value>>> {
    let result = db.session().query(query).await?;
    Ok(result.rows().iter().map(|r| r.values().to_vec()).collect())
}

/// Each expression inside a CASE branch equals the same expression outside
/// one, on rows the branch takes.
#[tokio::test]
async fn nested_scope_in_case_branch_matches_the_plain_expression() -> Result<()> {
    let db = open().await?;
    let head = "MATCH (a:N) OPTIONAL MATCH (a)-[:K]->(b:N) \
                WITH a, a.id AS k, collect(b.v) AS xs ";
    let bodies = [
        "[x IN xs | x + k]",
        "any(x IN xs WHERE x > k * 10)",
        "reduce(s = 0, x IN xs | s + x + k)",
        "size([(a)-[:K]->(y) WHERE y.id > k | y.id])",
        "[(a)-[:K]->(y) | k]",
    ];
    for body in bodies {
        let plain = rows(
            &db,
            &format!("{head}RETURN a.id AS id, {body} AS t ORDER BY id"),
        )
        .await?;
        let branched = rows(
            &db,
            &format!(
                "{head}RETURN a.id AS id, \
                 CASE WHEN size(xs) = 0 THEN null ELSE {body} END AS t ORDER BY id"
            ),
        )
        .await?;
        assert_eq!(plain.len(), 3, "{body}");
        for (p, b) in plain.iter().zip(&branched) {
            // The branch returns NULL where the list is empty (node 3) and the
            // plain expression otherwise. The two queries collect separately,
            // and `collect` promises no order, so lists compare as bags.
            let want = if p[0] == Value::Int(3) {
                Value::Null
            } else {
                p[1].clone()
            };
            assert_eq!(as_bag(&b[1]), as_bag(&want), "{body}: row {:?}", p[0]);
        }
    }
    Ok(())
}

#[tokio::test]
async fn nested_scope_in_case_branch_mixed_numeric_operands() -> Result<()> {
    let db = open().await?;
    assert_eq!(
        rows(
            &db,
            "UNWIND [[1, 2]] AS xs RETURN size([x IN xs | x]) * 1.5, \
             1.5 + size([x IN xs WHERE x > 1]), size([x IN xs | x]) > 1.5, \
             toFloat(reduce(s = 0, x IN xs | s + x)) / size(xs)"
        )
        .await?,
        vec![vec![
            Value::Float(3.0),
            Value::Float(2.5),
            Value::Bool(true),
            Value::Float(1.5),
        ]]
    );
    Ok(())
}
