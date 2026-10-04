// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A label disjunction on a traversal target admits every listed label.
//!
//! `MATCH (n)-[:R]->(m:Robot|C)` returned nothing. `LabelExpr` derefs to its
//! bare label names, and two consumers took those names without the operator:
//! the target filter ANDed them (`:Robot` *and* `:C`), and the traversal
//! pinned its target to the first label's id (`:Robot` only). A plain scan
//! `MATCH (m:Robot|C)` was always right, which is the control here.
//!
//! A quantified pattern's inner node can carry only one label constraint, so a
//! second label there is now refused instead of being ignored.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(label_disjunction_on_traversal)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    let all = ["Person", "Robot", "C"];
    db.schema()
        .label("Person")
        .property("id", DataType::Int)
        .label("Robot")
        .property("id", DataType::Int)
        .label("C")
        .property("id", DataType::Int)
        .edge_type("R", &all, &all)
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:Person {id: 1}), (:Robot {id: 2}), (:C {id: 3}), (:Person {id: 4})")
        .await?;
    for (a, b) in [(1, 2), (1, 3), (1, 4)] {
        tx.execute(&format!(
            "MATCH (x {{id: {a}}}), (y {{id: {b}}}) CREATE (x)-[:R]->(y)"
        ))
        .await?;
    }
    tx.commit().await?;
    Ok(db)
}

async fn ids(db: &Uni, query: &str) -> Result<Vec<String>> {
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
async fn label_disjunction_on_traversal_target_admits_each_label() -> Result<()> {
    let db = open().await?;
    let want = vec!["[Int(2)]".to_string(), "[Int(3)]".to_string()];
    assert_eq!(
        ids(&db, "MATCH (m:Robot|C) RETURN m.id").await?,
        want,
        "control"
    );
    for query in [
        "MATCH (n)-[:R]->(m:Robot|C) RETURN m.id",
        "MATCH (n:Person {id: 1})-[:R]->(m:Robot|C) RETURN m.id",
        "MATCH (m:Robot|C)<-[:R]-(n) RETURN m.id",
        "MATCH (n {id: 1}) OPTIONAL MATCH (n)-[:R]->(m:Robot|C) RETURN m.id",
    ] {
        assert_eq!(ids(&db, query).await?, want, "{query}");
    }
    assert_eq!(
        ids(
            &db,
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(:Robot|C) } RETURN n.id"
        )
        .await?,
        vec!["[Int(1)]".to_string()]
    );
    Ok(())
}

#[tokio::test]
async fn label_disjunction_on_traversal_target_refused_in_quantified_pattern() -> Result<()> {
    let db = open().await?;
    let err = db
        .session()
        .query("MATCH (a {id: 1}) ((x)-[:R]->(y:Robot|C)){1} RETURN y.id")
        .await
        .expect_err("a quantified pattern cannot express a label disjunction on an inner node");
    assert!(err.to_string().contains("at most one label"), "{err}");
    Ok(())
}
