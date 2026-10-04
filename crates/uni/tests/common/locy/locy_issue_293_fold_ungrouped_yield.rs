// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Regression for <https://github.com/rustic-ai/uni-db/issues/293>
//!
//! `FOLD pct = MAX(r.pct) YIELD KEY o, e, pct` returned one row per `o` with
//! `e` NULL, where the reporter expected one row per `(o, e)`.
//!
//! ## Root cause
//!
//! `KEY` marks a single YIELD item, so that YIELD declares one KEY (`o`), a
//! plain column (`e`) and a fold output (`pct`). The composite key the reporter
//! meant is `YIELD KEY o, KEY e, pct`, and that form was always correct.
//!
//! The defect is what happened to the plain column: `FoldExec` emits only the
//! KEY and fold columns, so `e` was dropped from the derived relation without
//! a diagnostic, and `QUERY ... RETURN e.uid` then read the absent column as
//! NULL. The program ran and returned a plausible wrong answer.
//!
//! ## The fix
//!
//! The compiler now rejects a FOLD clause's YIELD item that is neither a KEY
//! nor a FOLD output (nor an expression over one, which the planner evaluates
//! after the fold), with a message that spells out `KEY a, KEY b`.
//!
//! Run with:
//!   cargo nextest run -p uni-db --test integration -E 'test(issue_293)'

// Rust guideline compliant

use anyhow::Result;
use uni_db::{DataType, Uni, Value};

/// Four stakes: `x -> a 10`, `x -> b 20`, `y -> a 30`, `y -> b 40`.
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
        ("x", "a", 10.0),
        ("x", "b", 20.0),
        ("y", "a", 30.0),
        ("y", "b", 40.0),
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

/// Runs `program` and returns the error message, failing if it succeeds.
async fn compile_error(db: &Uni, program: &str) -> String {
    match db.session().locy(program).await {
        Ok(result) => panic!("program must be rejected, but it ran: {result:?}"),
        Err(e) => e.to_string(),
    }
}

/// The reported program no longer drops `e`: it is rejected, and the message
/// shows the composite-key syntax the reporter meant.
#[tokio::test]
async fn issue_293_non_key_column_in_fold_is_rejected() -> Result<()> {
    let db = build().await?;
    let msg = compile_error(
        &db,
        "CREATE RULE stake AS MATCH (o:E)-[r:OWNS]->(e:E) \
         FOLD pct = MAX(r.pct) YIELD KEY o, e, pct \
         QUERY stake RETURN o.uid AS owner, e.uid AS asset, pct",
    )
    .await;
    assert!(
        msg.contains("'e'") && msg.contains("KEY a, KEY b"),
        "error must name the column and show the composite-key form: {msg}"
    );
    Ok(())
}

/// Every shape of plain column is rejected, not only a bare node: a property,
/// a constant, and an expression over body variables.
#[tokio::test]
async fn issue_293_every_ungrouped_column_shape_is_rejected() -> Result<()> {
    let db = build().await?;
    for item in ["e.uid AS asset", "'grp' AS g", "r.pct * 2.0 AS doubled"] {
        let msg = compile_error(
            &db,
            &format!(
                "CREATE RULE stake AS MATCH (o:E)-[r:OWNS]->(e:E) \
                 FOLD pct = MAX(r.pct) YIELD KEY o, {item}, pct"
            ),
        )
        .await;
        assert!(
            msg.contains("neither a KEY nor a FOLD output"),
            "`{item}` must be rejected: {msg}"
        );
    }
    Ok(())
}

/// Control: the composite key written as the grammar defines it has always
/// grouped by both columns.
#[tokio::test]
async fn issue_293_composite_key_groups_by_both_columns() -> Result<()> {
    let db = build().await?;
    let result = db
        .session()
        .locy(
            "CREATE RULE stake AS MATCH (o:E)-[r:OWNS]->(e:E) \
             FOLD pct = MAX(r.pct) YIELD KEY o, KEY e, pct \
             QUERY stake RETURN o.uid AS owner, e.uid AS asset, pct",
        )
        .await?;
    let rows = result
        .command_results()
        .iter()
        .find_map(|c| c.as_query())
        .expect("program has a QUERY");
    let mut got: Vec<(String, String, f64)> = rows
        .iter()
        .map(|row| {
            let s = |k: &str| match row.get(k) {
                Some(Value::String(s)) => s.clone(),
                other => panic!("{k} must be a string, got {other:?}"),
            };
            let pct = match row.get("pct") {
                Some(Value::Float(f)) => *f,
                other => panic!("pct must be a float, got {other:?}"),
            };
            (s("owner"), s("asset"), pct)
        })
        .collect();
    got.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let want = [
        ("x", "a", 10.0),
        ("x", "b", 20.0),
        ("y", "a", 30.0),
        ("y", "b", 40.0),
    ]
    .map(|(o, e, p)| (o.to_string(), e.to_string(), p));
    assert_eq!(got, want);
    Ok(())
}

/// Still accepted: an expression over a FOLD output, which the planner
/// evaluates after the fold, and a non-FOLD base clause seeding a column a
/// sibling clause folds into.
#[tokio::test]
async fn issue_293_fold_derived_and_base_clause_columns_are_accepted() -> Result<()> {
    let db = build().await?;
    db.session()
        .locy(
            "CREATE RULE s AS MATCH (o:E)-[r:OWNS]->(e:E) \
             FOLD pct = MSUM(r.pct) YIELD KEY o, pct * 2.0 AS doubled",
        )
        .await?;
    db.session()
        .locy(
            "CREATE RULE b AS MATCH (e:E) WHERE e.uid IN ['x','y'] YIELD KEY e, 100.0 AS agg \
             CREATE RULE b AS MATCH (o:E)-[r:OWNS]->(e:E) WHERE o IS b \
             FOLD agg = MSUM(r.pct) YIELD KEY e, agg",
        )
        .await?;
    Ok(())
}

/// A QUERY that returns a variable the rule does not yield is an error, not a
/// column of NULLs. This is the second half of #293: once `e` was dropped,
/// `RETURN e.uid` read the absent column as NULL instead of failing.
#[tokio::test]
async fn issue_293_query_return_of_unyielded_variable_is_rejected() -> Result<()> {
    let db = build().await?;
    for program in [
        // FOLD rule
        "CREATE RULE stake AS MATCH (o:E)-[r:OWNS]->(e:E) \
         FOLD pct = MAX(r.pct) YIELD KEY o, pct \
         QUERY stake RETURN o.uid AS owner, e.uid AS asset, pct",
        // Plain rule
        "CREATE RULE owns AS MATCH (o:E)-[r:OWNS]->(e:E) YIELD KEY o \
         QUERY owns RETURN o.uid AS owner, e AS asset",
    ] {
        match db.session().locy(program).await {
            Err(_) => {}
            Ok(result) => {
                let rows = result
                    .command_results()
                    .iter()
                    .find_map(|c| c.as_query())
                    .cloned()
                    .unwrap_or_default();
                panic!("`e` is not yielded, so the QUERY must fail; it returned {rows:?}");
            }
        }
    }
    Ok(())
}
