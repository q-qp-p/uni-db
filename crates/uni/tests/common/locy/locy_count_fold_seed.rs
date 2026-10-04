// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! A rule that seeds a column in one clause and folds into it in another.
//!
//! ```text
//! CREATE RULE f AS MATCH (e:E) WHERE e.uid IN ['x','y'] YIELD KEY e, <seed> AS v
//! CREATE RULE f AS MATCH (o:E)-[r:OWNS]->(e:E) WHERE o IS f FOLD v = MCOUNT(r) YIELD KEY e, v
//! ```
//!
//! A FOLD aggregates, per KEY, every row of the rule: a folding clause
//! contributes its aggregate's input, a seeding clause contributes the value
//! it yields. That is SQL's `AGG(v) ... FROM (seeds UNION ALL contributions)
//! GROUP BY key`, and it is what `MSUM` has always done (seed 100 plus an edge
//! of 20 is 120). For a count it means a seed is one counted row, and
//! `NULL AS v` seeds a key without counting it, exactly as `COUNT` skips NULL.
//!
//! `MCOUNT` over a node or relationship used to fail outright:
//! `concatenate arrays of different data types (Int64, LargeBinary)`. The
//! folding clause carried the entity itself in `v` while the seed carried an
//! integer, and nothing reconciled them. A count only needs to know whether its
//! input is NULL, so the input is now projected as a non-NULL marker, and a
//! seed column takes its type from the clause that folds into it.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(count_fold_seed)'

// Rust guideline compliant

use anyhow::{Context, Result};
use uni_db::{DataType, Uni, Value};

/// `x -> a` three times, `y -> a` once, `y -> b` twice.
async fn build() -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    db.schema()
        .label("E")
        .property("uid", DataType::String)
        .done()
        .edge_type("OWNS", &["E"], &["E"])
        .property("pct", DataType::Float64)
        .done()
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:E {uid: 'x'}), (:E {uid: 'y'}), (:E {uid: 'a'}), (:E {uid: 'b'})")
        .await?;
    for (o, e, pct) in [
        ("x", "a", 30.0),
        ("x", "a", 40.0),
        ("x", "a", 40.0),
        ("y", "a", 20.0),
        ("y", "b", 10.0),
        ("y", "b", 10.0),
    ] {
        tx.execute_with("MATCH (o:E {uid: $o}), (e:E {uid: $e}) CREATE (o)-[:OWNS {pct: $p}]->(e)")
            .param("o", o)
            .param("e", e)
            .param("p", pct)
            .run()
            .await?;
    }
    tx.commit().await?;
    Ok(db)
}

/// Runs `program` and returns `uid -> v`, sorted by uid.
async fn values(db: &Uni, program: &str) -> Result<Vec<(String, f64)>> {
    let result = db.session().locy(program).await?;
    let rows = result
        .command_results()
        .iter()
        .find_map(|c| c.as_query())
        .expect("program has a QUERY");
    let mut out: Vec<(String, f64)> = rows
        .iter()
        .map(|row| {
            let uid = match row.get("uid") {
                Some(Value::String(s)) => s.clone(),
                other => panic!("uid must be a string, got {other:?}"),
            };
            let v = match row.get("v") {
                Some(Value::Float(f)) => *f,
                Some(Value::Int(i)) => *i as f64,
                other => panic!("v must be numeric, got {other:?}"),
            };
            (uid, v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn program(seed: &str, fold: &str, edge: &str, recursive: bool) -> String {
    let guard = if recursive { "WHERE o IS f" } else { "" };
    format!(
        "CREATE RULE f AS MATCH (e:E) WHERE e.uid IN ['x','y'] YIELD KEY e, {seed} AS v \
         CREATE RULE f AS MATCH (o:E)-[{edge}:OWNS]->(e:E) {guard} \
         FOLD v = {fold} YIELD KEY e, v \
         QUERY f RETURN e.uid AS uid, v"
    )
}

fn expect(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
    pairs.iter().map(|(u, v)| (u.to_string(), *v)).collect()
}

/// `MCOUNT` over a relationship or a node counts one per derivation, and a
/// NULL seed contributes nothing — recursive and non-recursive alike.
#[tokio::test]
async fn count_fold_seed_null_seed_counts_nothing() -> Result<()> {
    let db = build().await?;
    let want = expect(&[("a", 4.0), ("b", 2.0), ("x", 0.0), ("y", 0.0)]);
    for (fold, edge) in [("MCOUNT(r)", "r"), ("MCOUNT(o)", ""), ("MCOUNT(o)", "r")] {
        for recursive in [true, false] {
            let got = values(&db, &program("NULL", fold, edge, recursive))
                .await
                .with_context(|| format!("{fold} over `{edge}`, recursive={recursive}"))?;
            assert_eq!(got, want, "{fold} over `{edge}`, recursive={recursive}");
        }
    }
    Ok(())
}

/// A non-NULL seed is one counted row: `0 AS v` gives the seeded keys 1, not 0.
/// The compiler warns, because a literal seed into a count reads like a
/// starting value.
#[tokio::test]
async fn count_fold_seed_literal_seed_counts_once_and_warns() -> Result<()> {
    let db = build().await?;
    let text = program("0", "MCOUNT(r)", "r", true);
    let got = values(&db, &text).await?;
    assert_eq!(
        got,
        expect(&[("a", 4.0), ("b", 2.0), ("x", 1.0), ("y", 1.0)])
    );

    let result = db.session().locy(&text).await?;
    assert!(
        result
            .compile_warnings()
            .iter()
            .any(|w| w.code.as_str() == "count_fold_seed_counted"),
        "a literal seed into MCOUNT must warn; got {:?}",
        result.compile_warnings()
    );
    Ok(())
}

/// A sum seeded with an integer literal folds float inputs; the seed is part
/// of its key's sum (`MSUM` has always included it).
#[tokio::test]
async fn count_fold_seed_integer_seed_into_float_sum() -> Result<()> {
    let db = build().await?;
    for recursive in [true, false] {
        let got = values(&db, &program("0", "MSUM(r.pct)", "r", recursive))
            .await
            .with_context(|| format!("MSUM, recursive={recursive}"))?;
        assert_eq!(
            got,
            expect(&[("a", 130.0), ("b", 20.0), ("x", 0.0), ("y", 0.0)]),
            "recursive={recursive}"
        );
    }
    Ok(())
}

/// Fixpoint values below 1e-12 survive: an ALONG product of 1e-7 × 1e-7 is
/// 1e-14, not 0. The fixpoint rounded every float to an absolute 1e-12 for
/// stable dedup, which zeroed anything smaller.
#[tokio::test]
async fn count_fold_seed_small_fixpoint_floats_survive() -> Result<()> {
    let db = build().await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("MATCH ()-[r:OWNS]->() DELETE r").await?;
    tx.execute("MATCH (x:E {uid: 'x'}), (y:E {uid: 'y'}) CREATE (x)-[:OWNS {pct: 1.0e-7}]->(y)")
        .await?;
    tx.execute("MATCH (y:E {uid: 'y'}), (a:E {uid: 'a'}) CREATE (y)-[:OWNS {pct: 1.0e-7}]->(a)")
        .await?;
    tx.commit().await?;
    let program = "CREATE RULE reach AS MATCH (s:E {uid: 'x'})-[r:OWNS]->(e:E) YIELD KEY e, r.pct AS v \
         CREATE RULE reach AS MATCH (m:E)-[r:OWNS]->(e:E) WHERE m IS reach \
         ALONG v = prev.v * r.pct YIELD KEY e, v \
         QUERY reach RETURN e.uid AS uid, v";
    let got = values(&db, program).await?;
    let a = got.iter().find(|(u, _)| u == "a").map(|(_, v)| *v);
    let a = a.expect("a is reached through y");
    assert!((a - 1e-14).abs() < 1e-26, "a = {a}, want 1e-14");
    Ok(())
}
