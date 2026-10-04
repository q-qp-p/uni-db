// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Reachability keeps an endpoint when *any* trail reaches it.
//!
//! Under `DISTINCT` or `EXISTS`, an anonymous variable-length relationship
//! answers only whether a trail reaches each endpoint, by BFS over a
//! predecessor DAG. The trail check ran when an endpoint was first discovered
//! at a depth — seeing only the predecessors recorded so far. If the first one
//! reached lay on a walk that reused an edge, the endpoint was dropped, even
//! when a predecessor added later in the same depth had a valid trail. Which
//! predecessor came first depended on iteration order, so the same query
//! returned different rows run to run: on the W3 topology fixture,
//! `MATCH (a:Person)-[:KNOWS*2..3]-(b:Person) RETURN DISTINCT a.id, b.id`
//! gave anywhere from 95 to 103 of its 103 rows. Undirected hops make edge
//! reuse, and so the miss, common.
//!
//! The check now runs once the depth's predecessors are all recorded. The
//! reference is the same pattern with a named step variable, which enumerates
//! paths instead.
//!
//! Found by the `exists` and `optional` relations of `metamorphic::dqp::topo`.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(reachability_trail_any_predecessor)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::Uni;

use crate::metamorphic::dqp::topo::{Layout, build_topo};

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

/// The miss was order-dependent, so each query runs repeatedly. Measured on the
/// defect: each of these returned a short answer in most of 40 runs.
#[tokio::test(flavor = "multi_thread")]
async fn reachability_trail_any_predecessor_is_order_independent() -> Result<()> {
    let db = build_topo(Layout::Flushed, None).await?;
    let pairs = [
        (
            "MATCH (a:Person)-[:KNOWS*2..3]-(b:Person) RETURN DISTINCT a.id, b.id",
            "MATCH (a:Person)-[r:KNOWS*2..3]-(b:Person) RETURN DISTINCT a.id, b.id",
        ),
        (
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS*2..3]-(b:Person) \
             WHERE b.id = 0 OR b.age IS NULL } RETURN a.id",
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[r:KNOWS*2..3]-(b:Person) \
             WHERE b.id = 0 OR b.age IS NULL } RETURN a.id",
        ),
    ];
    for (reachability, enumerated) in pairs {
        let want = bag(&db, enumerated).await?;
        assert!(!want.is_empty(), "{enumerated}");
        for run in 0..20 {
            assert_eq!(
                bag(&db, reachability).await?,
                want,
                "run {run}: {reachability}"
            );
        }
    }
    Ok(())
}
