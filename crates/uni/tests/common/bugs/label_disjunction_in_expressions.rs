// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A label disjunction `n:A|B` is a predicate in an expression, as it is in a
//! pattern. In a `WHERE` (Cypher or a Locy rule body) it was a parse error.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(label_disjunction_in_expressions)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::Uni;

async fn bag(db: &Uni, query: &str) -> Result<Vec<String>> {
    let result = db.session().query(query).await?;
    let mut rows: Vec<String> = result
        .rows()
        .iter()
        .map(|r| format!("{:?}", r.values()))
        .collect();
    rows.sort();
    Ok(rows)
}

#[tokio::test]
async fn label_disjunction_in_expressions_matches_the_pattern_form() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    let tx = db.session().tx().await?;
    tx.execute("CREATE (:A {id: 1}), (:B {id: 2}), (:C {id: 3}), (:A:C {id: 4})")
        .await?;
    tx.commit().await?;
    for (expr, pattern) in [
        (
            "MATCH (n) WHERE n:A|B RETURN n.id",
            "MATCH (n:A|B) RETURN n.id",
        ),
        (
            "MATCH (n) WHERE n:A|:C RETURN n.id",
            "MATCH (n:A|C) RETURN n.id",
        ),
        (
            "MATCH (n) WHERE n IS :B|C RETURN n.id",
            "MATCH (n:B|C) RETURN n.id",
        ),
        (
            "MATCH (n) WHERE NOT n:A|B RETURN n.id",
            "MATCH (n:C) WHERE NOT n:A RETURN n.id",
        ),
        (
            "MATCH (n) RETURN n.id, n:A|B",
            "MATCH (n) RETURN n.id, n:A OR n:B",
        ),
        // Controls: one label, and a conjunction.
        (
            "MATCH (n) WHERE n IS :A RETURN n.id",
            "MATCH (n:A) RETURN n.id",
        ),
        (
            "MATCH (n) WHERE n:A:C RETURN n.id",
            "MATCH (n:A:C) RETURN n.id",
        ),
    ] {
        let want = bag(&db, pattern).await?;
        assert!(!want.is_empty(), "{pattern}");
        assert_eq!(bag(&db, expr).await?, want, "{expr}");
    }
    // In a Locy rule body.
    let result = db
        .session()
        .locy("CREATE RULE r AS MATCH (n) WHERE n:B|C YIELD KEY n QUERY r RETURN n.id AS id")
        .await?;
    let rows = result
        .command_results()
        .iter()
        .find_map(|c| c.as_query())
        .expect("a QUERY");
    let mut ids: Vec<String> = rows.iter().map(|r| format!("{:?}", r.get("id"))).collect();
    ids.sort();
    assert_eq!(ids.len(), 3, "{ids:?}");
    // A conjunction mixed with a disjunction is refused.
    assert!(
        db.session()
            .query("MATCH (n) WHERE n:A:B|C RETURN n")
            .await
            .is_err()
    );
    Ok(())
}
