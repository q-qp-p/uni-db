// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A relationship after a variable-length one may not reuse its edges.
//!
//! Relationship uniqueness spans a whole MATCH pattern. A hop *before* a
//! variable-length relationship was excluded from it, but a hop *after* one
//! was not: the fixed hop excluded only single edge-id columns, and a
//! variable-length relationship publishes its edges as a list (its step
//! variable) — or, anonymous, publishes none. So
//! `(a)<-[:K*1..2]-(x)-[:K]->(b)` walked back out along the edge it came in
//! by. Over the W3 topology fixture that returned 221 rows where brute force
//! over the edge list gives 104. The list column now counts as used edges, and
//! an anonymous variable-length relationship in a pattern with another
//! relationship gets a hidden step variable so it has one.
//!
//! Found by the `unroll` relation of `metamorphic::dqp::topo`: `*1..2` against
//! one fixed hop plus two fixed hops.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(vlp_relationship_uniqueness)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

/// `1 -> 2` and a self-loop on `3`: every walk that turns back reuses an edge.
async fn open() -> Result<Uni> {
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
    tx.execute("CREATE (a:N {id: 1})-[:K]->(:N {id: 2}), (c:N {id: 3})-[:K]->(c)")
        .await?;
    tx.commit().await?;
    Ok(db)
}

async fn ids(db: &Uni, query: &str) -> Result<Vec<Value>> {
    let result = db.session().query(query).await?;
    let mut ids: Vec<Value> = result
        .rows()
        .iter()
        .map(|r| r.values()[0].clone())
        .collect();
    ids.sort_by_key(|v| format!("{v:?}"));
    Ok(ids)
}

#[tokio::test]
async fn vlp_relationship_uniqueness_after_a_variable_length_hop() -> Result<()> {
    let db = open().await?;
    let reused: &[&str] = &[
        "MATCH (a:N {id: 2})<-[:K*1..1]-(x)-[:K]->(b) RETURN b.id",
        "MATCH (a:N {id: 2})<-[r:K*1..2]-(x)-[:K]->(b) RETURN b.id",
        "MATCH (a:N {id: 2})<-[:K*1..2]-(x)-[s:K]->(b) RETURN b.id",
        "MATCH (a:N {id: 3})-[:K*1..1]->(x)-[:K]->(b) RETURN b.id",
        "MATCH (a:N {id: 1})-[:K*1..1]-(x)-[:K]-(b) RETURN b.id",
        // Multiplicity-insensitive consumers take the reachability path.
        "MATCH (a:N {id: 2})<-[:K*1..1]-(x)-[:K]->(b) RETURN DISTINCT b.id",
        "MATCH (a:N {id: 2}) WHERE EXISTS { MATCH (a)<-[:K*1..1]-(x)-[:K]->(b) } RETURN a.id",
        // Comma-separated paths share the uniqueness scope.
        "MATCH (a:N {id: 2})<-[:K*1..1]-(x), (x)-[:K]->(b) RETURN b.id",
    ];
    for query in reused {
        assert_eq!(ids(&db, query).await?, Vec::<Value>::new(), "{query}");
    }
    // One row, not two: the zero-length walk leaves the self-loop free for the
    // fixed hop; the one-hop walk has used it.
    assert_eq!(
        ids(
            &db,
            "MATCH (a:N {id: 3})-[:K*0..1]->(x)-[:K]->(b) RETURN b.id"
        )
        .await?,
        vec![Value::Int(3)]
    );
    Ok(())
}

/// Control: separate MATCH clauses may reuse an edge.
#[tokio::test]
async fn vlp_relationship_uniqueness_controls() -> Result<()> {
    let db = open().await?;
    assert_eq!(
        ids(
            &db,
            "MATCH (a:N {id: 2})<-[:K*1..1]-(x) MATCH (x)-[:K]->(b) RETURN b.id"
        )
        .await?,
        vec![Value::Int(2)]
    );
    Ok(())
}

/// The same rule for a Locy rule body, which is planned by another entry
/// point and missed the fix at first.
#[tokio::test]
async fn vlp_relationship_uniqueness_in_a_locy_rule_body() -> Result<()> {
    let db = open().await?;
    let result = db
        .session()
        .locy(
            "CREATE RULE r AS MATCH (a:N {id: 2})<-[:K*1..1]-(x)-[:K]->(b) YIELD KEY a, KEY b \
             QUERY r RETURN b.id AS id",
        )
        .await?;
    let rows = result
        .command_results()
        .iter()
        .find_map(|c| c.as_query())
        .expect("program has a QUERY");
    assert!(rows.is_empty(), "{rows:?}");
    Ok(())
}
