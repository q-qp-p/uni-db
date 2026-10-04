// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! An inline element `WHERE` filters the match.
//!
//! `(m WHERE m.id = 3)` and `[r WHERE r.w = 1]` parse into the pattern
//! element, but no planner turned them into a filter: every probe returned
//! the rows the predicate should have removed — a traversal target, a
//! relationship, a lone node, an `OPTIONAL MATCH`. Inside `EXISTS { MATCH … }`
//! the same predicate failed with `UndefinedVariable`. The predicates are now
//! ANDed into the clause's `WHERE`; on a variable-length relationship, where
//! that would change their meaning, they are refused.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(inline_element_where)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("N")
        .property("id", DataType::Int)
        .edge_type("R", &["N"], &["N"])
        .property("w", DataType::Int)
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:N {id: 1}), (:N {id: 2}), (:N {id: 3})")
        .await?;
    tx.execute("MATCH (a:N {id: 1}), (b:N {id: 2}) CREATE (a)-[:R {w: 1}]->(b)")
        .await?;
    tx.execute("MATCH (a:N {id: 1}), (b:N {id: 3}) CREATE (a)-[:R {w: 2}]->(b)")
        .await?;
    tx.commit().await?;
    Ok(db)
}

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
async fn inline_element_where_filters_the_match() -> Result<()> {
    let db = open().await?;
    let cases: &[(&str, &[&str])] = &[
        (
            "MATCH (a:N {id: 1})-[:R]->(m WHERE m.id = 3) RETURN m.id",
            &["[Int(3)]"],
        ),
        (
            "MATCH (a:N {id: 1})-[r:R WHERE r.w = 1]->(m) RETURN m.id",
            &["[Int(2)]"],
        ),
        ("MATCH (a WHERE a.id = 2) RETURN a.id", &["[Int(2)]"]),
        (
            "MATCH (a:N {id: 1})-[r:R WHERE r.w > 0]->(m WHERE m.id <> 2) WHERE a.id = 1 RETURN m.id",
            &["[Int(3)]"],
        ),
        (
            "MATCH (a:N {id: 1}) OPTIONAL MATCH (a)-[:R]->(m WHERE m.id = 9) RETURN m.id",
            &["[Null]"],
        ),
        (
            "MATCH (n:N) WHERE EXISTS { MATCH (n)-[:R]->(m WHERE m.id = 2) } RETURN n.id",
            &["[Int(1)]"],
        ),
        (
            "MATCH (n:N) RETURN n.id, COUNT { MATCH (n)-[:R]->(m WHERE m.id = 3) } AS k",
            &["[Int(1), Int(1)]", "[Int(2), Int(0)]", "[Int(3), Int(0)]"],
        ),
    ];
    for (query, want) in cases {
        let want: Vec<String> = want.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(bag(&db, query).await?, want, "{query}");
    }
    Ok(())
}

#[tokio::test]
async fn inline_element_where_in_a_locy_rule_body() -> Result<()> {
    let db = open().await?;
    let result = db
        .session()
        .locy(
            "CREATE RULE heavy AS MATCH (a:N)-[r:R WHERE r.w = 2]->(m) YIELD KEY m \
             QUERY heavy RETURN m.id AS id",
        )
        .await?;
    let rows = result
        .command_results()
        .iter()
        .find_map(|c| c.as_query())
        .expect("program has a QUERY");
    let ids: Vec<Value> = rows.iter().filter_map(|r| r.get("id").cloned()).collect();
    assert_eq!(ids, vec![Value::Int(3)]);
    Ok(())
}

#[tokio::test]
async fn inline_element_where_on_variable_length_is_refused() -> Result<()> {
    let db = open().await?;
    let err = db
        .session()
        .query("MATCH (a:N {id: 1})-[r:R* WHERE r.w = 1]->(m) RETURN m.id")
        .await
        .expect_err("a per-edge predicate on a variable-length relationship is refused");
    assert!(err.to_string().contains("variable-length"), "{err}");
    Ok(())
}
