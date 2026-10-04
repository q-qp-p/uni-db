// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Differential tests: the production Locy engine must agree, row for row, with
//! this crate's naive reference oracle.
//!
//! Each case is generated once and fed to both sides — the engine evaluates the
//! Locy program text over the seeded graph; the oracle evaluates the equivalent
//! IR — and their derived relations are compared. A divergence is, by
//! construction, an engine bug (see rustic-ai/uni-db#94 for one such find).
//!
//! The harness returns the engine's **raw** rows (duplicates preserved) so the
//! tests can witness set-semantics breakage directly — the historical `IS NOT`
//! bug emitted a bag, and deduping in the harness would have masked it.

// Rust guideline compliant

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use proptest::prelude::*;
use uni_common::{LocyIncompleteReason, Value};
use uni_db::{Uni, UniError};

use uni_locy_oracle::eval::{Relation, evaluate, evaluate_bags};
use uni_locy_oracle::generator::{
    build_complement, build_layered_dag, build_union, expected_closure_size,
    random_program_strategy,
};
use uni_locy_oracle::ir::PROB_SCALE;
use uni_locy_oracle::ir::{Generated, Tuple};

/// Recovers the seeded integer id from a `YIELD KEY` column value.
///
/// Per the V1 finding (issue #94), KEY columns yield whole nodes, so the scalar
/// id is read back from the node's `properties["id"]`.
fn key_to_id(value: &Value) -> i64 {
    match value {
        Value::Node(node) => node
            .properties
            .get("id")
            .and_then(Value::as_i64)
            .expect("seeded node is missing an integer `id` property"),
        // Tolerated in case a future engine fixes KEY-expression projection.
        Value::Int(i) => *i,
        other => panic!("unexpected KEY column value: {other:?}"),
    }
}

/// Runs a generated program through the engine, returning relation `rel`'s **raw**
/// rows as id-tuples (duplicates preserved, so callers can check set semantics).
async fn engine_rows(generated: &Generated, rel: &str) -> Result<Vec<Tuple>> {
    let db = Uni::in_memory().build().await?;
    let tx = db.session().tx().await?;
    tx.execute(&generated.base_graph_cypher).await?;
    tx.commit().await?;

    let result = db.session().locy(&generated.program_text).await?;
    let key_cols = &generated.key_schema[rel];

    let rows = result.derived.get(rel).map_or_else(Vec::new, |rows| {
        rows.iter()
            .map(|row| {
                key_cols
                    .iter()
                    .map(|c| key_to_id(&row[c]))
                    .collect::<Tuple>()
            })
            .collect()
    });
    Ok(rows)
}

/// Returns relation `rel` from the oracle as a set (empty if the rule derived nothing).
fn oracle_facts(generated: &Generated, rel: &str) -> Relation {
    evaluate(&generated.oracle_rules)
        .remove(rel)
        .unwrap_or_default()
}

/// Set view of raw rows.
fn as_set(rows: &[Tuple]) -> HashSet<Tuple> {
    rows.iter().cloned().collect()
}

/// Compares one relation between engine and oracle; returns a failure note if they diverge.
async fn rel_divergence(generated: &Generated, rel: &str) -> Result<Option<String>> {
    let raw = engine_rows(generated, rel).await?;
    let set = as_set(&raw);
    let oracle = oracle_facts(generated, rel);
    if raw.len() != set.len() {
        return Ok(Some(format!(
            "{rel}: engine returned duplicates (raw={}, distinct={})",
            raw.len(),
            set.len(),
        )));
    }
    if set != oracle {
        return Ok(Some(format!(
            "{rel}: engine≠oracle (engine={} oracle={})",
            set.len(),
            oracle.len(),
        )));
    }
    Ok(None)
}

// ── P3: fixed cases spanning the 300-fact dedup threshold ────────────────────

/// Engine output equals the oracle on curated shapes, including the historical
/// `IS NOT`-bug closure `(4, 15) = 1350` and cases on both sides of 300.
///
/// Checks all three witnesses (no duplicates, closed-form size, oracle agreement)
/// and runs every case before asserting, so a regression reports the full
/// divergence picture rather than only the first failure.
#[tokio::test]
async fn engine_matches_oracle_fixed_cases() -> Result<()> {
    let cases = [(2, 3), (3, 5), (3, 10), (4, 15), (6, 5)];
    let mut failures = Vec::new();

    for &(stages, width) in &cases {
        let g = build_layered_dag(stages, width);
        let raw = engine_rows(&g, "reaches").await?;
        let set = as_set(&raw);
        let oracle = oracle_facts(&g, "reaches");

        if raw.len() != set.len() {
            failures.push(format!(
                "({stages},{width}): engine returned duplicates (raw={}, distinct={})",
                raw.len(),
                set.len(),
            ));
        }
        if set != oracle {
            failures.push(format!(
                "({stages},{width}): engine≠oracle (engine={} oracle={} closed-form={})",
                set.len(),
                oracle.len(),
                expected_closure_size(stages, width),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "engine↔oracle divergence:\n{}",
        failures.join("\n"),
    );
    Ok(())
}

// ── P4: threshold-straddling property test ───────────────────────────────────

proptest! {
    // PR-time budget: modest case count and graph size so the gate stays fast.
    // The higher-volume nightly run lives in the `*_soak` test below (driven by
    // the `ORACLE_SOAK_CASES` env var).
    #![proptest_config(ProptestConfig { cases: 40, ..ProptestConfig::default() })]

    /// Randomized closures straddling the 300-fact dedup-strategy switch.
    ///
    /// The `(stages, width)` bounds make `expected_closure_size` land on both
    /// sides of 300. Three independent witnesses fail the moment the engine's
    /// `LeftAnti` path stops being set-correct, on either side of the threshold:
    /// no duplicate rows, the closed-form cardinality, and exact agreement with
    /// the naive oracle.
    #[test]
    fn closure_is_set_across_dedup_threshold(stages in 2usize..6, width in 5usize..18) {
        let g = build_layered_dag(stages, width);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let raw = rt.block_on(engine_rows(&g, "reaches")).expect("engine eval");
        let set = as_set(&raw);
        let expected = expected_closure_size(stages, width);

        // W1: set semantics — the engine returns no duplicate rows.
        prop_assert_eq!(raw.len(), set.len(), "duplicate rows at ({},{})", stages, width);
        // W2: closed-form cardinality (independent of the oracle).
        prop_assert_eq!(set.len(), expected, "wrong closure size at ({},{})", stages, width);
        // W3: row-for-row agreement with the naive oracle.
        prop_assert_eq!(set, oracle_facts(&g, "reaches"), "oracle disagreement at ({},{})", stages, width);
    }
}

// ── P5: completeness guard — never silent-short ──────────────────────────────

/// Seeds the standard graph and returns a ready database for a builder call.
async fn seeded_db(generated: &Generated) -> Result<Uni> {
    let db = Uni::in_memory().build().await?;
    let tx = db.session().tx().await?;
    tx.execute(&generated.base_graph_cypher).await?;
    tx.commit().await?;
    Ok(db)
}

/// An over-budget evaluation must raise an explicit error, never return short rows.
///
/// For a program of known cardinality run under a 1ns timeout, the result must be
/// either exact (it happened to finish) or an explicit [`UniError::LocyIncomplete`]
/// — a silently-short `Ok` fails the exact-count assertion, which is the bug
/// (gap G14). The engine ships this behavior; this is the regression net.
#[tokio::test]
async fn incomplete_eval_errors_never_silent_short() -> Result<()> {
    let g = build_layered_dag(5, 15);
    let expected = expected_closure_size(5, 15);
    let db = seeded_db(&g).await?;

    let outcome = db
        .session()
        .locy_with(&g.program_text)
        .timeout(Duration::from_nanos(1))
        .run()
        .await;

    match outcome {
        Ok(result) => {
            let key_cols = &g.key_schema["reaches"];
            let n = result.derived.get("reaches").map_or(0, |rows| {
                rows.iter()
                    .map(|row| {
                        key_cols
                            .iter()
                            .map(|c| key_to_id(&row[c]))
                            .collect::<Tuple>()
                    })
                    .collect::<HashSet<_>>()
                    .len()
            });
            assert_eq!(n, expected, "completed but short — silent partial (G14)!");
        }
        Err(UniError::LocyIncomplete { detail }) => {
            assert!(
                matches!(
                    detail.reason,
                    LocyIncompleteReason::Timeout | LocyIncompleteReason::IterationLimit
                ),
                "unexpected incompleteness reason: {:?}",
                detail.reason,
            );
        }
        Err(e) => panic!("unexpected error (expected exact rows or LocyIncomplete): {e}"),
    }
    Ok(())
}

/// Under a generous budget the same program completes with the exact closure —
/// proving any partial above is caused by the budget, not the program.
#[tokio::test]
async fn generous_timeout_completes_exactly() -> Result<()> {
    let g = build_layered_dag(5, 15);
    let raw = engine_rows(&g, "reaches").await?;
    assert_eq!(as_set(&raw), oracle_facts(&g, "reaches"));
    assert_eq!(as_set(&raw).len(), expected_closure_size(5, 15));
    Ok(())
}

/// With `allow_partial(true)`, a tight budget returns `Ok` whose result is
/// explicitly flagged incomplete — proving the partial path is reachable and
/// labeled (the negative control for the guard above).
#[tokio::test]
async fn allow_partial_surfaces_incomplete_flag() -> Result<()> {
    let g = build_layered_dag(5, 15);
    let db = seeded_db(&g).await?;

    let result = db
        .session()
        .locy_with(&g.program_text)
        .timeout(Duration::from_nanos(1))
        .allow_partial(true)
        .run()
        .await?;

    assert!(
        result.timed_out(),
        "allow_partial result should be flagged as timed out",
    );
    let incomplete = result
        .incomplete
        .as_ref()
        .expect("incomplete diagnostics should be present");
    assert!(matches!(
        incomplete.reason,
        LocyIncompleteReason::Timeout | LocyIncompleteReason::IterationLimit
    ));
    Ok(())
}

// ── P6: stratified IS NOT complement + multi-clause unions ───────────────────

/// The stratified `IS NOT` complement matches the oracle — including shapes whose
/// closure exceeds 300, the exact regime of the historical `IS NOT` bug.
///
/// This is the track's headline: the bug returned 0 complement rows above the
/// threshold; here the engine and the naive oracle must agree on both `reaches`
/// and `unreached`, and the oracle catches any divergence at *any* scale,
/// independent of the 300-fact trigger.
#[tokio::test]
async fn engine_matches_oracle_complement() -> Result<()> {
    // (4,12) closure=864 and (5,12) closure=1440 are over the 300 threshold.
    let cases = [(3, 6), (3, 10), (4, 12), (5, 12)];
    let mut failures = Vec::new();

    for &(stages, width) in &cases {
        let g = build_complement(stages, width);
        for rel in ["reaches", "unreached"] {
            if let Some(note) = rel_divergence(&g, rel).await? {
                failures.push(format!("({stages},{width}) {note}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "complement divergence:\n{}",
        failures.join("\n"),
    );
    Ok(())
}

/// Multi-clause union (`EDGE` ∪ reverse `EDGE2`) matches the oracle.
#[tokio::test]
async fn engine_matches_oracle_union() -> Result<()> {
    let cases = [(2, 4), (3, 6), (4, 8)];
    let mut failures = Vec::new();

    for &(stages, width) in &cases {
        let g = build_union(stages, width);
        if let Some(note) = rel_divergence(&g, "linked").await? {
            failures.push(format!("({stages},{width}) {note}"));
        }
    }

    assert!(
        failures.is_empty(),
        "union divergence:\n{}",
        failures.join("\n"),
    );
    Ok(())
}

// ── P7: high-volume soak (nightly only) ──────────────────────────────────────

/// High-volume agreement soak: thousands of generated programs, engine vs oracle.
///
/// `#[ignore]` (nightly only) and named `*_soak` so the nightly workflow's
/// `--run-ignored ignored-only -E 'test(/soak/)'` filter selects it. Volume is
/// driven by `ORACLE_SOAK_CASES` (default 10_000) — a dedicated env var, kept
/// separate from the shared `PROPTEST_CASES` so it cannot perturb other crates'
/// soak tests in the same nightly run. Bounds include over-threshold closures so
/// the dedup-strategy switch is stressed at volume.
///
/// On the volume: each case pays a full `Uni::in_memory()` instantiation
/// (~tens of ms), which dominates over graph size — so the engine-diff rate is
/// the limiter, not the oracle. 10_000 cases completes in ~10–15 min, well
/// inside the soak profile's per-test kill window; pushing toward the plan's
/// aspirational ≥100k would exceed it (and so never complete) without amortizing
/// the database build across cases or parallelizing — a deliberate follow-up.
/// The oracle's own correctness is validated at higher volume, cheaply, by the
/// closed-form unit tests in `eval.rs` (no engine in the loop).
#[test]
#[ignore = "soak: high-volume nightly run; volume set by ORACLE_SOAK_CASES"]
fn closure_is_set_soak() {
    use proptest::test_runner::{Config, TestCaseError, TestRunner};

    let cases: u32 = std::env::var("ORACLE_SOAK_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let mut runner = TestRunner::new(Config {
        cases,
        ..Config::default()
    });

    runner
        .run(&(2usize..5, 2usize..9), |(stages, width)| {
            let g = build_layered_dag(stages, width);
            let raw = rt
                .block_on(engine_rows(&g, "reaches"))
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let set = as_set(&raw);
            prop_assert_eq!(
                raw.len(),
                set.len(),
                "duplicate rows ({},{})",
                stages,
                width
            );
            prop_assert_eq!(
                set.len(),
                expected_closure_size(stages, width),
                "wrong size ({},{})",
                stages,
                width
            );
            prop_assert_eq!(
                set,
                oracle_facts(&g, "reaches"),
                "oracle disagreement ({},{})",
                stages,
                width
            );
            Ok(())
        })
        .expect("soak: engine diverged from oracle");
}

// ── W5: random programs over a multigraph, FOLD included ─────────────────────

/// A relation column value as the oracle's `i64`: a node by its `id`, an
/// integer as is, and — only in a sum column (`s`), whose `SUM`/`MSUM` is a
/// float by definition — an integral float. Anything else is reported, not
/// coerced: an integer `MIN` that came back as `2.0` is the defect W4 found.
fn column_to_i64(column: &str, value: &Value) -> Option<i64> {
    match value {
        Value::Node(_) | Value::Int(_) if column != PROB_COLUMN => Some(key_to_id(value)),
        Value::Float(f) if column == "s" && f.fract() == 0.0 => Some(*f as i64),
        // A probability, in the oracle's fixed-point units.
        Value::Float(f) if column == PROB_COLUMN => Some((f * PROB_SCALE as f64).round() as i64),
        _ => None,
    }
}

/// The PROB column of every probabilistic relation the generator emits.
const PROB_COLUMN: &str = "pb";

/// Whether two sorted bags agree, a probability within two fixed-point units
/// (floating-point evaluation order differs between the two sides).
fn bags_agree(got: &[Tuple], want: &[Tuple], cols: &[String]) -> bool {
    got.len() == want.len()
        && got.iter().zip(want).all(|(g, w)| {
            g.iter().zip(w).zip(cols).all(|((a, b), c)| {
                if c == PROB_COLUMN {
                    (a - b).abs() <= 2
                } else {
                    a == b
                }
            })
        })
}

/// Runs a generated program once and returns every relation that differs from
/// the oracle, with the engine's raw rows (duplicates kept) and the oracle's.
/// `non_empty` collects the relations the oracle derived facts for.
async fn all_divergences(
    generated: &Generated,
    non_empty: &mut HashSet<String>,
) -> Result<Vec<String>> {
    let db = seeded_db(generated).await?;
    let result = db.session().locy(&generated.program_text).await?;
    let oracle = evaluate_bags(&generated.oracle_rules);
    let mut out = Vec::new();
    let mut rels: Vec<&String> = generated.key_schema.keys().collect();
    rels.sort();
    for rel in rels {
        let cols = &generated.key_schema[rel];
        let mut raw: Vec<Tuple> = Vec::new();
        for row in result.derived.get(rel.as_str()).into_iter().flatten() {
            let mut tuple = Vec::with_capacity(cols.len());
            for c in cols {
                match row.get(c).and_then(|v| column_to_i64(c, v)) {
                    Some(v) => tuple.push(v),
                    None => {
                        out.push(format!("{rel}.{c}: unexpected value {:?}", row.get(c)));
                        tuple.push(i64::MIN);
                    }
                }
            }
            raw.push(tuple);
        }
        // Bags: a plain rule's oracle relation is a set, so an engine
        // duplicate is a divergence; an ALONG rule's has one row per path.
        raw.sort();
        let want = oracle.get(rel.as_str()).cloned().unwrap_or_default();
        if !want.is_empty() {
            non_empty.insert(rel.clone());
        }
        if !bags_agree(&raw, &want, cols) {
            out.push(format!("{rel}: engine {raw:?}\n    oracle {want:?}"));
        }
    }
    Ok(out)
}

fn random_cases(default: u32) -> u32 {
    std::env::var("ORACLE_RANDOM_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn drive_random_programs(cases: u32) {
    use proptest::test_runner::{Config, TestCaseError, TestRunner};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut runner = TestRunner::new(Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    });
    // Per relation, how many cases derived any fact: a comparison over empty
    // relations proves nothing.
    let tally = std::cell::RefCell::new(std::collections::BTreeMap::<String, u32>::new());
    let outcome = runner.run(&random_program_strategy(), |g| {
        let mut non_empty = HashSet::new();
        let diffs = rt
            .block_on(all_divergences(&g, &mut non_empty))
            .map_err(|e| TestCaseError::fail(format!("{e}\n{}", g.program_text)))?;
        if diffs.is_empty() {
            Ok(())
        } else {
            Err(TestCaseError::fail(format!(
                "{}\n  program:\n{}\n  graph: {}",
                diffs.join("\n  "),
                g.program_text,
                g.base_graph_cypher
            )))
        }
        .inspect(|()| {
            let mut t = tally.borrow_mut();
            for rel in non_empty {
                *t.entry(rel).or_default() += 1;
            }
        })
    });
    let tally = tally.into_inner();
    eprintln!("[oracle:random] cases={cases} non-empty per relation: {tally:?}");
    outcome.expect("a random program's derived facts diverged from the oracle");
    for rel in [
        "link", "reach", "un", "deg", "span", "flow", "cost", "tot", "roll", "opt", "pr", "nor",
        "prd", "safe",
    ] {
        let n = tally.get(rel).copied().unwrap_or(0);
        assert!(
            n * 5 >= cases,
            "relation {rel} derived facts in only {n} of {cases} cases; the comparison is too often vacuous"
        );
    }
}

/// Random programs over a multigraph — parallel and identical edges, self-loops,
/// two relationship types — with recursion, stratified negation and FOLD over
/// bags, against the oracle. The FOLD rules are where a set where a bag is due
/// (or the reverse) would show: parallel edges must each count.
#[test]
fn random_programs_match_the_oracle() {
    drive_random_programs(random_cases(96));
}

/// The same at nightly volume (`ORACLE_RANDOM_CASES`, default 5 000).
#[test]
#[ignore = "soak: random oracle programs at nightly volume"]
fn random_programs_soak() {
    drive_random_programs(random_cases(5_000));
}
