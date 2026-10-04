// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! An anonymous variable-length relationship yields one row **per path**.
//!
//! openCypher binds a MATCH row per matched path whether or not the
//! relationship is named, so naming it must not change the rows. It did: with
//! no step, path or group variable the planner chose an existence-only BFS that
//! emits one row per `(endpoint, depth)`, so on the diamond
//! `x -> {m1, m2} -> a` the query
//! `MATCH (x)-[:R*2..2]->(e) RETURN count(*)` returned 1 while the same query
//! with `[r:R*2..2]` returned 2. That BFS is only a valid shortcut where the
//! consumer ignores multiplicity (`EXISTS`, a pattern predicate, `DISTINCT`),
//! and those contexts must keep it: on a dense cyclic graph the path count is
//! exponential while reachability is linear.
//!
//! The main oracle is metamorphic: every query runs with the relationship
//! anonymous and named, and the two must agree.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(vlp_anonymous)'

// Rust guideline compliant

use std::time::Duration;

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

/// Builds `N {uid}` nodes and `R` edges from `(src, dst)` pairs.
async fn build(edges: &[(&str, &str)]) -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("N")
        .property("uid", DataType::String)
        .done()
        .edge_type("R", &["N"], &["N"])
        .done()
        .apply()
        .await?;
    let mut uids: Vec<&str> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
    uids.sort_unstable();
    uids.dedup();
    let session = db.session();
    let tx = session.tx().await?;
    for uid in uids {
        tx.execute_with("CREATE (:N {uid: $u})")
            .param("u", uid)
            .run()
            .await?;
    }
    for (a, b) in edges {
        tx.execute_with("MATCH (a:N {uid: $a}), (b:N {uid: $b}) CREATE (a)-[:R]->(b)")
            .param("a", *a)
            .param("b", *b)
            .run()
            .await?;
    }
    tx.commit().await?;
    Ok(db)
}

/// Two diamonds in series: 2 paths `x -> a`, 4 paths `x -> b`.
const DIAMONDS: [(&str, &str); 8] = [
    ("x", "m1"),
    ("x", "m2"),
    ("m1", "a"),
    ("m2", "a"),
    ("a", "n1"),
    ("a", "n2"),
    ("n1", "b"),
    ("n2", "b"),
];

/// Runs `query` and returns its rows rendered as sorted strings, so two runs
/// can be compared as multisets.
async fn bag(db: &Uni, query: &str) -> Result<Vec<String>> {
    let result = db.session().query(query).await?;
    let mut rows: Vec<String> = result
        .rows()
        .iter()
        .map(|row| format!("{:?}", row.values()))
        .collect();
    rows.sort();
    Ok(rows)
}

/// Runs `query` and returns its single integer cell.
async fn scalar(db: &Uni, query: &str) -> Result<i64> {
    let result = db.session().query(query).await?;
    match result.rows().first().map(|r| r.values()[0].clone()) {
        Some(Value::Int(n)) => Ok(n),
        other => panic!("{query}: expected one integer, got {other:?}"),
    }
}

/// Every query below is written with `{rel}` where the relationship variable
/// goes; it runs once with `{rel}` empty and once with it named, and the two
/// result bags must be identical.
const NAMED_VS_ANONYMOUS: &[&str] = &[
    "MATCH (x:N {uid: 'x'})-[{rel}:R*2..2]->(e:N) RETURN e.uid",
    "MATCH (x:N {uid: 'x'})-[{rel}:R*1..4]->(e:N) RETURN e.uid",
    "MATCH (x:N {uid: 'x'})-[{rel}:R*]->(e:N) RETURN e.uid, count(*)",
    "MATCH (s:N)-[{rel}:R*1..4]->(e:N) RETURN s.uid, e.uid",
    "MATCH (x:N {uid: 'x'}) OPTIONAL MATCH (x)-[{rel}:R*4..4]->(e:N) RETURN e.uid",
    "MATCH (x:N {uid: 'x'})<-[{rel}:R*0..0]-(e:N) RETURN e.uid",
    "MATCH (b:N {uid: 'b'})<-[{rel}:R*2..4]-(e:N) RETURN e.uid",
    "MATCH (x:N {uid: 'x'})-[{rel}:R*1..4]->(e:N) RETURN DISTINCT e.uid",
    "MATCH (x:N {uid: 'x'})-[{rel}:R*1..4]->(e:N) RETURN count(DISTINCT e)",
];

#[tokio::test]
async fn vlp_anonymous_matches_named_on_every_query_shape() -> Result<()> {
    let db = build(&DIAMONDS).await?;
    let mut failures = Vec::new();
    for template in NAMED_VS_ANONYMOUS {
        let anonymous = template.replace("{rel}", "");
        let named = template.replace("{rel}", "r");
        let (a, n) = (bag(&db, &anonymous).await?, bag(&db, &named).await?);
        if a != n {
            failures.push(format!(
                "{anonymous}\n  anonymous: {a:?}\n  named:     {n:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    Ok(())
}

/// The exact counts, so the test does not pass if both forms are wrong alike.
#[tokio::test]
async fn vlp_anonymous_counts_one_row_per_path() -> Result<()> {
    let db = build(&DIAMONDS).await?;
    let cases = [
        (
            "MATCH (:N {uid: 'x'})-[:R*2..2]->(:N {uid: 'a'}) RETURN count(*)",
            2,
        ),
        (
            "MATCH (:N {uid: 'x'})-[:R*4..4]->(:N {uid: 'b'}) RETURN count(*)",
            4,
        ),
        ("MATCH (:N {uid: 'x'})-[:R*1..4]->(e:N) RETURN count(*)", 12),
        (
            "MATCH (x:N {uid: 'x'}) RETURN COUNT { (x)-[:R*4..4]->(:N) } AS n",
            4,
        ),
        (
            "MATCH (x:N {uid: 'x'}) RETURN size([(x)-[:R*2..2]->(e:N) | e]) AS n",
            2,
        ),
        (
            "MATCH (:N {uid: 'x'})-[:R*1..4]->(e:N) RETURN count(DISTINCT e)",
            6,
        ),
    ];
    for (query, want) in cases {
        assert_eq!(scalar(&db, query).await?, want, "{query}");
    }
    Ok(())
}

/// A quantified path pattern whose inner variables nothing reads was also
/// planned endpoint-only, and must count paths the same way.
#[tokio::test]
async fn vlp_anonymous_quantified_pattern_counts_paths() -> Result<()> {
    let db = build(&DIAMONDS).await?;
    let n = scalar(
        &db,
        "MATCH (:N {uid: 'x'}) ((:N)-[:R]->(:N)){2} (e:N {uid: 'a'}) RETURN count(*)",
    )
    .await?;
    assert_eq!(n, 2, "two 2-step paths x -> a");
    Ok(())
}

/// Existence contexts keep the reachability BFS.
///
/// Twenty diamonds in series: 41 vertices, but 2^20 (about a million) paths
/// from the first to the last. Reachability visits each vertex once per depth;
/// enumerating every path would take far longer and hold far more memory. The
/// graph is acyclic so reachability itself stays cheap — on a dense cyclic
/// graph the BFS's per-depth trail check is expensive in its own right, which
/// is a separate matter from the mode choice pinned here.
#[tokio::test]
async fn vlp_existence_contexts_stay_reachability_only() -> Result<()> {
    const DIAMONDS: usize = 20;
    let mut names = vec!["d0".to_string()];
    let mut edges_owned = Vec::new();
    for i in 0..DIAMONDS {
        let (from, to) = (format!("d{i}"), format!("d{}", i + 1));
        for side in ["l", "r"] {
            let mid = format!("d{i}{side}");
            edges_owned.push((from.clone(), mid.clone()));
            edges_owned.push((mid.clone(), to.clone()));
            names.push(mid);
        }
        names.push(to);
    }
    let edges: Vec<(&str, &str)> = edges_owned
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let db = build(&edges).await?;
    let last = format!("d{DIAMONDS}");
    let queries = [
        format!(
            "MATCH (a:N {{uid: 'd0'}}), (b:N {{uid: '{last}'}}) \
             WHERE EXISTS {{ (a)-[:R*]->(b) }} RETURN count(*)"
        ),
        format!(
            "MATCH (a:N {{uid: 'd0'}}), (b:N {{uid: '{last}'}}) \
             WHERE (a)-[:R*]->(b) RETURN count(*)"
        ),
        "MATCH (a:N {uid: 'd0'})-[:R*]->(b:N) RETURN count(DISTINCT b)".to_string(),
        "MATCH (a:N {uid: 'd0'})-[:R*]->(b:N) RETURN DISTINCT b.uid".to_string(),
    ];
    for query in &queries {
        // The EXISTS body runs synchronously inside one poll, so a tokio
        // timeout cannot interrupt it; measure instead, and let the harness's
        // own timeout catch a hang.
        let started = std::time::Instant::now();
        let rows = bag(&db, query).await?;
        let elapsed = started.elapsed();
        assert!(!rows.is_empty(), "{query}");
        assert!(
            elapsed < Duration::from_secs(10),
            "{query} took {elapsed:?}; it should not enumerate the 2^{DIAMONDS} paths"
        );
    }
    Ok(())
}
