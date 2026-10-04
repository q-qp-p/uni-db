// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Mutually recursive rules keep their own facts.
//!
//! Rules that reach each other through `IS` form one stratum, evaluated by one
//! fixpoint. Inside it each rule had its own state, but the fixpoint's output
//! stream concatenates every rule, and that concatenation was stored under
//! each rule's name: `QUERY even` returned `odd`'s rows too (5 rows where 3
//! were right, all with `a.v` NULL), and a later stratum's `IS even` read them
//! as well. The stratum's rule order also came from a hash set, so which of
//! these symptoms appeared could change from run to run.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(mutual_recursion_per_rule)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

/// Chain a1 -> b1 -> a2 -> b2 -> a3; B nodes carry v = 10, 20 so a leak
/// between the two rules is visible in the values.
async fn open() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("A")
        .property("v", DataType::Int)
        .label("B")
        .property("v", DataType::Int)
        .edge_type("R", &["A", "B"], &["A", "B"])
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:A {v: 1}), (:A {v: 2}), (:A {v: 3}), (:B {v: 10}), (:B {v: 20})")
        .await?;
    for (a, al, b, bl) in [
        ("A", 1, "B", 10),
        ("B", 10, "A", 2),
        ("A", 2, "B", 20),
        ("B", 20, "A", 3),
    ] {
        tx.execute(&format!(
            "MATCH (x:{a} {{v: {al}}}), (y:{b} {{v: {bl}}}) CREATE (x)-[:R]->(y)"
        ))
        .await?;
    }
    tx.commit().await?;
    Ok(db)
}

const RULES: &str = "\
    CREATE RULE even AS MATCH (a:A {v: 1}) YIELD KEY a \
    CREATE RULE odd AS MATCH (a:A)-[:R]->(b:B) WHERE a IS even YIELD KEY b \
    CREATE RULE even AS MATCH (b:B)-[:R]->(a:A) WHERE b IS odd YIELD KEY a ";

async fn query_ints(db: &Uni, program: &str) -> Result<Vec<i64>> {
    let result = db.session().locy(program).await?;
    let rows = result
        .command_results()
        .iter()
        .find_map(|c| c.as_query())
        .expect("program has a QUERY");
    let mut out: Vec<i64> = rows
        .iter()
        .map(|r| match r.get("v") {
            Some(Value::Int(i)) => *i,
            other => panic!("v must be an integer, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    Ok(out)
}

#[tokio::test]
async fn mutual_recursion_per_rule_facts_are_separate() -> Result<()> {
    let db = open().await?;
    // Repeated: the rule order within the stratum used to vary per run.
    for _ in 0..5 {
        assert_eq!(
            query_ints(&db, &format!("{RULES} QUERY even RETURN a.v AS v")).await?,
            vec![1, 2, 3]
        );
        assert_eq!(
            query_ints(&db, &format!("{RULES} QUERY odd RETURN b.v AS v")).await?,
            vec![10, 20]
        );
    }
    Ok(())
}

/// A later stratum reads each rule's own facts.
#[tokio::test]
async fn mutual_recursion_per_rule_facts_across_strata() -> Result<()> {
    let db = open().await?;
    let program = format!(
        "{RULES} CREATE RULE later AS MATCH (n) WHERE n IS even YIELD KEY n \
         QUERY later RETURN n.v AS v"
    );
    assert_eq!(query_ints(&db, &program).await?, vec![1, 2, 3]);
    let program = format!(
        "{RULES} CREATE RULE later AS MATCH (n) WHERE n IS odd YIELD KEY n \
         QUERY later RETURN n.v AS v"
    );
    assert_eq!(query_ints(&db, &program).await?, vec![10, 20]);
    Ok(())
}
