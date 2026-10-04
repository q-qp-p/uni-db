// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Regression for <https://github.com/rustic-ai/uni-db/issues/294>
//!
//! A **recursive** FOLD rule kept only one of two parallel edges between the
//! same pair of nodes. With `x -[30]-> a`, `x -[40]-> a`, `y -[20]-> a`, the
//! `MSUM` for `a` came back as `60` or `50` depending on the run — never `90`.
//!
//! ## Root cause
//!
//! Issue #159 gave recursive FOLD / ALONG rules hidden derivation
//! discriminators so distinct derivations of one KEY stay apart through the
//! fixpoint's dedup. They were built from MATCH-bound **node** variables only.
//! Two parallel edges bind the same nodes, so both rows got one derivation
//! key, and `merge_fold_contributions` (which keeps the *newest* row per
//! derivation key) discarded one of them. Which one survived depended on the
//! row order of the candidate batch, which varies with partitioning — hence
//! the nondeterminism.
//!
//! The non-recursive form of the same rule was always correct: it never builds
//! a `FixpointState`, so it aggregates the plain bag of MATCH rows. That makes
//! it the specification the recursive path is checked against below.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(issue_294)'

// Rust guideline compliant

use anyhow::{Context, Result};
use uni_db::{DataType, Uni, Value};

/// Builds an ownership graph of `E {uid}` nodes and `(owner, asset, pct)`
/// `OWNS` edges. Repeated `(owner, asset)` pairs become parallel edges.
async fn build(edges: &[(&str, &str, f64)]) -> Result<Uni> {
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

    let mut uids: Vec<&str> = edges.iter().flat_map(|(a, b, _)| [*a, *b]).collect();
    uids.sort_unstable();
    uids.dedup();

    let session = db.session();
    let tx = session.tx().await?;
    for uid in uids {
        tx.execute_with("CREATE (:E {uid: $u})")
            .param("u", uid)
            .run()
            .await?;
    }
    for (a, b, pct) in edges {
        tx.execute_with("MATCH (a:E {uid: $a}), (b:E {uid: $b}) CREATE (a)-[:OWNS {pct: $p}]->(b)")
            .param("a", *a)
            .param("b", *b)
            .param("p", *pct)
            .run()
            .await?;
    }
    tx.commit().await?;
    Ok(db)
}

/// Runs `program` and returns `uid -> value` from its single QUERY.
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

fn value_of(rows: &[(String, f64)], uid: &str) -> Option<f64> {
    rows.iter().find(|(u, _)| u == uid).map(|(_, v)| *v)
}

const PARALLEL: [(&str, &str, f64); 3] = [("x", "a", 30.0), ("x", "a", 40.0), ("y", "a", 20.0)];

/// The issue's program verbatim (with `v` in place of `agg` in the QUERY).
const ISSUE_PROGRAM: &str = r#"
CREATE RULE b AS
    MATCH (e:E) WHERE e.uid IN ['x','y']
    YIELD KEY e, 100.0 AS agg
CREATE RULE b AS
    MATCH (o:E)-[r:OWNS]->(e:E)
    WHERE o IS b AND r.pct IS NOT NULL
    FOLD agg = MSUM(r.pct)
    REQUIRE agg >= 50.0
    YIELD KEY e, agg
QUERY b RETURN e.uid AS uid, agg AS v
"#;

/// The reported program: both of `x`'s parallel stakes must count.
#[tokio::test]
async fn issue_294_recursive_msum_counts_both_parallel_edges() -> Result<()> {
    let db = build(&PARALLEL).await?;
    let rows = values(&db, ISSUE_PROGRAM).await?;
    assert_eq!(
        value_of(&rows, "a"),
        Some(90.0),
        "a = 30 + 40 + 20; one parallel edge was dropped: {rows:?}"
    );
    Ok(())
}

/// The issue's second configuration: a downstream edge out of `a` must not
/// disturb `a`'s own total, and `b` sees `a` as an owner.
#[tokio::test]
async fn issue_294_parallel_edges_with_downstream_edge() -> Result<()> {
    let mut edges = PARALLEL.to_vec();
    edges.push(("a", "b", 60.0));
    let db = build(&edges).await?;
    let rows = values(&db, ISSUE_PROGRAM).await?;
    assert_eq!(value_of(&rows, "a"), Some(90.0), "{rows:?}");
    assert_eq!(value_of(&rows, "b"), Some(60.0), "{rows:?}");
    Ok(())
}

/// Parallel edges carrying the **same** value are distinct derivations too —
/// the #159 equal-value case, reached through edges instead of child nodes.
#[tokio::test]
async fn issue_294_equal_valued_parallel_edges_both_count() -> Result<()> {
    let db = build(&[("x", "a", 30.0), ("x", "a", 30.0), ("y", "a", 20.0)]).await?;
    let rows = values(&db, ISSUE_PROGRAM).await?;
    assert_eq!(value_of(&rows, "a"), Some(80.0), "{rows:?}");
    Ok(())
}

/// Recursive and non-recursive forms of the same fold must agree on a
/// multigraph, for each monotonic fold, whether or not the edge is named.
///
/// The non-recursive rule aggregates the plain bag of MATCH rows, so it is the
/// reference. The recursive rule seeds every owner through a base clause and
/// reads `o IS f`, so for the asset keys it matches exactly the same rows.
#[tokio::test]
async fn issue_294_recursive_fold_agrees_with_non_recursive_on_multigraph() -> Result<()> {
    let edges = [
        ("x", "a", 30.0),
        ("x", "a", 40.0),
        ("x", "a", 40.0),
        ("y", "a", 20.0),
        ("y", "b", 10.0),
        ("y", "b", 10.0),
    ];
    let db = build(&edges).await?;

    // (fold, argument, edge variable, seed value for the base clause). The
    // anonymous-edge case sums a constant: the edge has no variable to read,
    // and parallel anonymous edges must still count once each.
    let cases = [
        ("MSUM", "r.pct", "r", "0.0"),
        ("MSUM", "1.0", "r", "0.0"),
        ("MSUM", "1.0", "", "0.0"),
    ];
    let mut failures = Vec::new();
    for (agg, arg, edge, seed) in cases {
        let flat = format!(
            "CREATE RULE f AS MATCH (o:E)-[{edge}:OWNS]->(e:E) \
             FOLD v = {agg}({arg}) YIELD KEY e, v \
             QUERY f RETURN e.uid AS uid, v"
        );
        let recursive = format!(
            "CREATE RULE f AS MATCH (e:E) WHERE e.uid IN ['x','y'] YIELD KEY e, {seed} AS v \
             CREATE RULE f AS MATCH (o:E)-[{edge}:OWNS]->(e:E) WHERE o IS f \
             FOLD v = {agg}({arg}) YIELD KEY e, v \
             QUERY f RETURN e.uid AS uid, v"
        );
        let case = format!("{agg}({arg}) over edge `{edge}`");
        let want = values(&db, &flat)
            .await
            .with_context(|| format!("non-recursive {case}"))?;
        match values(&db, &recursive).await {
            Ok(rows) => {
                let got: Vec<(String, f64)> = rows
                    .into_iter()
                    .filter(|(uid, _)| uid == "a" || uid == "b")
                    .collect();
                if got != want {
                    failures.push(format!(
                        "{case}: recursive {got:?} != non-recursive {want:?}"
                    ));
                }
            }
            Err(e) => failures.push(format!("{case}: recursive form failed: {e:#}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

/// Control: the same stakes held by two *different* owner nodes were always
/// counted correctly, pinning the trigger to parallel edges.
#[tokio::test]
async fn issue_294_control_distinct_owners_are_counted() -> Result<()> {
    let db = build(&[("x", "a", 30.0), ("z", "a", 40.0), ("y", "a", 20.0)]).await?;
    let program = ISSUE_PROGRAM.replace("['x','y']", "['x','y','z']");
    let rows = values(&db, &program).await?;
    assert_eq!(value_of(&rows, "a"), Some(90.0), "{rows:?}");
    Ok(())
}

/// Two distinct 2-hop paths between the same owner and asset are distinct
/// derivations, exactly like two parallel edges. A variable-length
/// relationship is identified by its whole edge list.
///
/// The answer is asserted directly rather than against the non-recursive form:
/// with an **anonymous** relationship, plain Cypher itself currently returns
/// one row per endpoint pair instead of one per path, so that reference would
/// be wrong too.
#[tokio::test]
async fn issue_294_distinct_variable_length_paths_both_count() -> Result<()> {
    let db = build(&[
        ("x", "m1", 1.0),
        ("m1", "a", 1.0),
        ("x", "m2", 1.0),
        ("m2", "a", 1.0),
    ])
    .await?;
    for edge in ["p", ""] {
        let recursive = format!(
            "CREATE RULE f AS MATCH (e:E) WHERE e.uid = 'x' YIELD KEY e, 0.0 AS v \
             CREATE RULE f AS MATCH (o:E)-[{edge}:OWNS*2..2]->(e:E) WHERE o IS f \
             FOLD v = MSUM(1.0) YIELD KEY e, v \
             QUERY f RETURN e.uid AS uid, v"
        );
        let got: Vec<(String, f64)> = values(&db, &recursive)
            .await?
            .into_iter()
            .filter(|(uid, _)| uid == "a")
            .collect();
        assert_eq!(
            got,
            vec![("a".to_string(), 2.0)],
            "two paths x->m1->a and x->m2->a over edge `{edge}`"
        );
    }
    Ok(())
}
