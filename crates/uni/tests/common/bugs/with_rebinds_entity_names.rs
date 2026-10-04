// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! `WITH b AS a` rebinds `a`, even when `a` was already an entity.
//!
//! A projection carried an entity's flattened columns (`b._vid`, `b.id`, ...)
//! under the *source* name, and the planner's per-variable kind and label maps
//! looked straight through it to the node a scan had bound to `a` below. So
//! after `WITH b AS a`:
//!
//! * a filter on `a.id` or `a:Robot` failed to plan ("No field named a.id");
//! * in a swap, `WITH b AS a, a AS b WHERE a.id > b.id`, the `a.*` columns
//!   still held the old `a` — the filter compared the wrong nodes and returned
//!   14 rows of 36 (silent);
//! * `type(a)` after `WITH r AS a` used the old node's label and matched no row.
//!
//! The carried columns are now named for the alias, and a projection rebinds
//! its aliases' kinds and labels, as `UNWIND` already did.
//!
//! A property read through a *chain* of renames (`WITH n AS a WITH a AS x
//! RETURN x.id`) also never reached the scan: the planner folded an alias's
//! properties onto its source one link only, so an unlabelled node loaded just
//! its property blob and `x.id` compiled to NULL (or, after a `MERGE`
//! re-encoded the entity, failed to plan — TCK Merge5[18]/[19]).
//!
//! Found by re-probing a W1 audit suspect ("WHERE pushdown through a shadowing
//! WITH") on the W3 topology fixture. Each query below is compared with the
//! same query written without the rename.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(with_rebinds_entity_names)'

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

#[tokio::test(flavor = "multi_thread")]
async fn with_rebinds_entity_names_matches_the_unrenamed_query() -> Result<()> {
    const P: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person) ";
    const U: &str = "MATCH (a:Person)-[:KNOWS]->(b) ";
    const R: &str = "MATCH (a:Person)-[r:KNOWS]->(b) ";
    let pairs: Vec<(String, String)> = vec![
        // Swaps: silent wrong answers.
        (
            format!("{P}WITH b AS a, a AS b WHERE a.id > b.id RETURN count(*)"),
            format!("{P}WHERE b.id > a.id RETURN count(*)"),
        ),
        (
            format!("{P}WITH b AS a, a AS x WHERE a.id > 3 RETURN count(*)"),
            format!("{P}WHERE b.id > 3 RETURN count(*)"),
        ),
        (
            format!("{P}WITH b AS a, a AS b WHERE a.age = 10 RETURN a.id"),
            format!("{P}WHERE b.age = 10 RETURN b.id"),
        ),
        // Rebinding to an already-used name: failed to plan.
        (
            format!("{U}WITH b AS a WHERE a.id = 3 RETURN a.id"),
            format!("{U}WHERE b.id = 3 RETURN b.id"),
        ),
        (
            format!("{U}WITH b AS a WHERE a:Robot RETURN a.id"),
            format!("{U}WHERE b:Robot RETURN b.id"),
        ),
        (
            format!("{U}WITH b AS a WITH a WHERE a.id > 3 RETURN a.id"),
            format!("{U}WHERE b.id > 3 RETURN b.id"),
        ),
        (
            format!("{U}WITH b AS a ORDER BY a.id LIMIT 3 RETURN a.id"),
            format!("{U}WITH b ORDER BY b.id LIMIT 3 RETURN b.id"),
        ),
        (
            format!("{R}WITH r AS a WHERE a.w > 0 RETURN a.w"),
            format!("{R}WHERE r.w > 0 RETURN r.w"),
        ),
        (
            format!("{R}WITH b AS r WHERE r.id > 3 RETURN r.id"),
            format!("{R}WHERE b.id > 3 RETURN b.id"),
        ),
        // `type(a)` after an edge takes the name of a node: silently false.
        (
            format!("{R}WITH r AS a, b WHERE type(a) = 'KNOWS' RETURN type(a), count(*)"),
            format!("{R}RETURN 'KNOWS', count(*)"),
        ),
        // Controls: a fresh name and a non-entity rebinding.
        (
            format!("{U}WITH b AS c WHERE c.id = 3 RETURN c.id"),
            format!("{U}WHERE b.id = 3 RETURN b.id"),
        ),
        (
            format!("{R}WITH r.w AS a, b WHERE a = 1 RETURN b.id"),
            format!("{R}WHERE r.w = 1 RETURN b.id"),
        ),
    ];
    for layout in [Layout::Flushed, Layout::HalfUnflushed] {
        let db = build_topo(layout, None).await?;
        for (renamed, direct) in &pairs {
            let want = bag(&db, direct).await?;
            assert!(!want.is_empty(), "{direct}");
            assert_eq!(bag(&db, renamed).await?, want, "{layout:?}: {renamed}");
        }
    }
    Ok(())
}

/// Property reads through a chain of renames, on an unlabelled node.
#[tokio::test]
async fn with_rebinds_entity_names_through_a_chain() -> Result<()> {
    let db = Uni::in_memory().build().await?;
    let tx = db.session().tx().await?;
    tx.execute("CREATE ({id: 0})").await?;
    tx.commit().await?;
    for query in [
        "MATCH (n) WITH n AS a WITH a AS x RETURN x.id",
        "MATCH (n) WITH n AS a WITH a AS x WITH x AS z RETURN z.id",
        "MATCH (n) WITH n AS a WITH a AS x RETURN x['id']",
        "MATCH (n) WITH n AS a MERGE (c) WITH a AS x RETURN x.id",
    ] {
        let session = db.session();
        let tx = session.tx().await?;
        assert_eq!(
            tx_bag(&tx, query).await?,
            vec!["[Int(0)]".to_string()],
            "{query}"
        );
    }
    Ok(())
}

async fn tx_bag(tx: &uni_db::Transaction, query: &str) -> Result<Vec<String>> {
    let result = tx.query(query).await?;
    Ok(result
        .rows()
        .iter()
        .map(|r| format!("{:?}", r.values()))
        .collect())
}
