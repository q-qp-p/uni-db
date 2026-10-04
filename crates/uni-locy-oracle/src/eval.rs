//! The naive Datalog fixpoint evaluator — the reference the engine must match.
//!
//! Evaluates an [`OracleProgram`] stratum by stratum,
//! re-deriving every clause to a least fixpoint with structural set semantics.
//! Unoptimized and obviously correct by construction; that is the whole point.
//!
//! The structural set semantics are what make this a useful oracle: a clause's
//! output feeds a [`HashSet`], so duplicate tuples emitted via different
//! intermediates collapse automatically — at *any* scale, with no threshold.
//! That is precisely the property the engine's optimized anti-join must preserve.

// Rust guideline compliant

use std::collections::{HashMap, HashSet};

use crate::ir::{Agg, OracleBest, OracleClause, OracleFold, OracleProgram, PROB_SCALE, Tuple};

/// A relation: the set of derived (or base) fact tuples for one rule.
pub type Relation = HashSet<Tuple>;

/// A rule's facts as a sorted bag: a set for a plain rule, one row per group
/// for a `FOLD` or `BEST BY` rule, and one row per derivation path (duplicates
/// kept) for an `ALONG` rule.
pub type Bag = Vec<Tuple>;

/// Iterations a stratum may take before the oracle gives up: only a per-path
/// or folded rule over a cycle fails to converge, which is a generator bug.
const MAX_ITERATIONS: usize = 100_000;

/// A probability from its [`PROB_SCALE`] integer form.
fn from_scale(x: i64) -> f64 {
    x as f64 / PROB_SCALE as f64
}

/// A probability in [`PROB_SCALE`] integer form.
fn to_scale(p: f64) -> i64 {
    (p * PROB_SCALE as f64).round() as i64
}

/// Whether `fact` begins with `subj` (matching on the leading subject columns).
fn matches_subject(fact: &[i64], subj: &[i64]) -> bool {
    fact.len() >= subj.len() && fact[..subj.len()] == *subj
}

/// Evaluates a program to its least fixpoint, returning each rule's relation
/// as a set. See [`evaluate_bags`] for the multiplicities.
///
/// # Panics
/// As [`evaluate_bags`].
#[must_use]
pub fn evaluate(program: &OracleProgram) -> HashMap<String, Relation> {
    evaluate_bags(program)
        .into_iter()
        .map(|(name, bag)| (name, bag.into_iter().collect()))
        .collect()
}

/// Evaluates a program to its fixpoint, returning each rule's facts as a bag.
///
/// Strata are processed in order. Within a stratum every rule is recomputed
/// from the current relations until none changes: a plain rule accumulates
/// (a set), while a `FOLD`, `BEST BY` or per-path rule is *replaced* by its
/// recomputation — so a self-reference reads the previous iteration's folded
/// values, best rows or paths. Because negated references only target strictly
/// earlier strata, those relations are complete when a later stratum reads them.
///
/// # Panics
/// Panics if a clause references a variable it does not bind, or a stratum
/// does not converge (a per-path or folded rule over a cycle) — generator bugs.
#[must_use]
pub fn evaluate_bags(program: &OracleProgram) -> HashMap<String, Bag> {
    let mut rels: HashMap<String, Bag> = HashMap::new();
    for stratum in &program.strata {
        let mut iterations = 0;
        loop {
            let mut changed = false;
            for rule in stratum {
                let rows: Vec<Tuple> = rule
                    .clauses
                    .iter()
                    .flat_map(|c| eval_clause(c, &rels))
                    .collect();
                let mut next = if let Some(fold) = &rule.fold {
                    fold_rows(fold, rows)
                } else if let Some(best) = &rule.best {
                    best_rows(best, rows)
                } else if rule.per_path {
                    rows
                } else {
                    let mut all = rels.get(&rule.name).cloned().unwrap_or_default();
                    all.extend(rows);
                    all.sort();
                    all.dedup();
                    all
                };
                next.sort();
                if rels.get(&rule.name) != Some(&next) {
                    changed = true;
                    rels.insert(rule.name.clone(), next);
                }
            }
            if !changed {
                break;
            }
            iterations += 1;
            assert!(
                iterations < MAX_ITERATIONS,
                "a stratum did not converge: a per-path or folded rule over a cycle"
            );
        }
    }
    rels
}

/// A FOLD over the bag of a rule's rows, grouped by key: one fact per group.
///
/// # Panics
/// Panics if a row is shorter than the fold's columns (a generator bug).
fn fold_rows(fold: &OracleFold, rows: Vec<Tuple>) -> Bag {
    let mut groups: HashMap<Tuple, Vec<Tuple>> = HashMap::new();
    for row in rows {
        groups
            .entry(row[..fold.key_count].to_vec())
            .or_default()
            .push(row);
    }
    groups
        .into_iter()
        .map(|(key, rows)| {
            let mut fact = key;
            for a in &fold.aggs {
                let inputs = || rows.iter().map(|r| r[a.input.expect("an input column")]);
                fact.push(match a.agg {
                    Agg::CountStar => rows.len() as i64,
                    Agg::Count => inputs().count() as i64,
                    Agg::Sum => inputs().sum(),
                    Agg::Min => inputs().min().expect("a group has a row"),
                    Agg::Max => inputs().max().expect("a group has a row"),
                    Agg::NoisyOr => {
                        to_scale(1.0 - inputs().map(|x| 1.0 - from_scale(x)).product::<f64>())
                    }
                    Agg::Product => to_scale(inputs().map(from_scale).product::<f64>()),
                });
            }
            fact
        })
        .collect()
}

/// `BEST BY`: per key, the rows with the best criterion (duplicates removed).
fn best_rows(best: &OracleBest, rows: Vec<Tuple>) -> Bag {
    let mut winners: HashMap<Tuple, Vec<Tuple>> = HashMap::new();
    for row in rows {
        let group = winners.entry(row[..best.key_count].to_vec()).or_default();
        let better = |a: i64, b: i64| if best.ascending { a < b } else { a > b };
        match group.first().map(|r| r[best.criterion]) {
            Some(current) if better(current, row[best.criterion]) => {}
            Some(current) if current == row[best.criterion] => group.push(row),
            _ => *group = vec![row],
        }
    }
    let mut out: Bag = winners.into_values().flatten().collect();
    out.sort();
    out.dedup();
    out
}

/// Evaluates one clause against the current relations: join, anti-join,
/// compute, project.
///
/// The pipeline is: seed bindings from the clause's base tuples, extend them
/// through each positive `IS` reference (binding the `TO` target and the
/// reference's trailing value columns), drop bindings matched by each negated
/// reference, compute the clause's derived variables, then project to the
/// `YIELD` columns. Every row of a referenced bag joins, so a per-path relation
/// contributes once per path.
///
/// # Panics
/// Panics if the clause binds a variable to a missing base column or projects a
/// variable that was never bound (malformed program).
fn eval_clause(clause: &OracleClause, rels: &HashMap<String, Bag>) -> Vec<Tuple> {
    // 1. Seed bindings: one per base tuple, mapping each local variable to its value.
    let mut bindings: Vec<HashMap<String, i64>> = clause
        .base
        .iter()
        .map(|row| {
            clause
                .var_cols
                .iter()
                .map(|(var, &col)| (var.clone(), row[col]))
                .collect()
        })
        .collect();

    // 2. Positive references: join each binding against the referenced relation,
    //    binding the `TO` target (the column following the subject columns) and
    //    then the value columns.
    let empty = Bag::new();
    for r in &clause.pos_refs {
        let target_rel = rels.get(&r.rule).unwrap_or(&empty);
        let mut next = Vec::new();
        for b in &bindings {
            let subj: Tuple = r.subjects.iter().map(|s| b[s]).collect();
            for fact in target_rel {
                if matches_subject(fact, &subj) {
                    let mut nb = b.clone();
                    let mut col = subj.len();
                    if let Some(t) = &r.target {
                        nb.insert(t.clone(), fact[col]);
                        col += 1;
                    }
                    for v in &r.values {
                        nb.insert(v.clone(), fact[col]);
                        col += 1;
                    }
                    next.push(nb);
                }
            }
        }
        bindings = next;
    }

    // 3. Negated references: keep only bindings whose subject tuple is absent.
    for r in &clause.neg_refs {
        let banned = rels.get(&r.rule).unwrap_or(&empty);
        bindings.retain(|b| {
            let subj: Tuple = r.subjects.iter().map(|s| b[s]).collect();
            !banned.iter().any(|fact| matches_subject(fact, &subj))
        });
    }

    // 4. Computed variables, then probabilistic complements, then the
    //    projection. A plain rule's caller dedups.
    for b in &mut bindings {
        for c in &clause.computed {
            let value = c.constant + c.factor * c.vars.iter().map(|v| b[v]).sum::<i64>();
            b.insert(c.name.clone(), value);
        }
        for pc in &clause.prob_complements {
            let referenced = rels.get(&pc.reference.rule).unwrap_or(&empty);
            let subj: Tuple = pc.reference.subjects.iter().map(|s| b[s]).collect();
            let none = referenced
                .iter()
                .filter(|fact| matches_subject(fact, &subj))
                .map(|fact| 1.0 - from_scale(fact[pc.prob_column]))
                .product::<f64>();
            // `1 - q` with `q = 1 - none`: the complement is `none` itself.
            let p = from_scale(b[&pc.prob_var]) * none;
            b.insert(pc.prob_var.clone(), to_scale(p));
        }
    }
    bindings
        .into_iter()
        .map(|b| clause.yield_vars.iter().map(|v| b[v]).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::{
        build_complement, build_layered_dag, build_union, expected_closure_size,
    };
    use crate::ir::{IsRef, OracleClause, OracleProgram, OracleRule};

    /// Oracle closure must equal the closed-form size across a grid spanning 300.
    #[test]
    fn closure_matches_closed_form_grid() {
        for stages in 1..6 {
            for width in 1..11 {
                let g = build_layered_dag(stages, width);
                let rels = evaluate(&g.oracle_rules);
                let got = rels.get("reaches").map_or(0, Relation::len);
                assert_eq!(
                    got,
                    expected_closure_size(stages, width),
                    "stages={stages} width={width}"
                );
            }
        }
    }

    /// Specific cases that straddle the engine's 300-fact threshold.
    #[test]
    fn closure_matches_closed_form_across_threshold() {
        for &(s, w) in &[(3, 10), (2, 20), (6, 5), (4, 15)] {
            let g = build_layered_dag(s, w);
            let rels = evaluate(&g.oracle_rules);
            assert_eq!(
                rels["reaches"].len(),
                expected_closure_size(s, w),
                "stages={s} width={w}"
            );
        }
    }

    /// Evaluation is order-independent: two runs yield identical relations.
    #[test]
    fn evaluation_is_deterministic() {
        let g = build_layered_dag(3, 8);
        assert_eq!(evaluate(&g.oracle_rules), evaluate(&g.oracle_rules));
    }

    /// Generated complement: `unreached` = all-pairs minus the closure, exactly.
    #[test]
    fn build_complement_oracle_is_exact() {
        for &(s, w) in &[(3, 5), (4, 12)] {
            let g = build_complement(s, w);
            let rels = evaluate(&g.oracle_rules);
            let total_pairs = (s * w) * (s * w);
            assert_eq!(
                rels["unreached"].len(),
                total_pairs - expected_closure_size(s, w),
                "({s},{w})"
            );
        }
    }

    /// Generated union: `linked` = disjoint forward ∪ reverse = `2 · |edges|`.
    #[test]
    fn build_union_oracle_is_exact() {
        for &(s, w) in &[(2, 4), (3, 5)] {
            let g = build_union(s, w);
            let rels = evaluate(&g.oracle_rules);
            let forward = w * w * s.saturating_sub(1);
            assert_eq!(rels["linked"].len(), 2 * forward, "({s},{w})");
        }
    }

    /// A FOLD reads the bag: two identical parallel edges count twice.
    #[test]
    fn fold_counts_every_row() {
        use crate::ir::{Agg, FoldAgg, OracleFold};
        // Edges [src, dst, index, weight]: two identical 0->1 edges and 0->2.
        let edges: Vec<Tuple> = vec![vec![0, 1, 0, 2], vec![0, 1, 1, 2], vec![0, 2, 2, 5]];
        let deg = OracleRule {
            per_path: false,
            best: None,
            name: "deg".to_string(),
            clauses: vec![OracleClause {
                computed: Vec::new(),
                prob_complements: Vec::new(),
                base: edges,
                var_cols: HashMap::from([("a".to_string(), 0), ("w".to_string(), 3)]),
                pos_refs: Vec::new(),
                neg_refs: Vec::new(),
                yield_vars: vec!["a".to_string(), "w".to_string()],
            }],
            fold: Some(OracleFold {
                key_count: 1,
                aggs: [
                    (Agg::CountStar, None),
                    (Agg::Sum, Some(1)),
                    (Agg::Min, Some(1)),
                    (Agg::Max, Some(1)),
                    (Agg::Count, Some(1)),
                ]
                .into_iter()
                .map(|(agg, input)| FoldAgg { agg, input })
                .collect(),
            }),
        };
        let rels = evaluate(&OracleProgram {
            strata: vec![vec![deg]],
        });
        let expected: Relation = [vec![0, 3, 9, 2, 5, 3]].into_iter().collect();
        assert_eq!(rels["deg"], expected);
    }

    /// Generator-built rules for a hand-sized graph, evaluated as bags.
    fn shape_bags(
        edges: Vec<crate::generator::RandomEdge>,
        nodes: usize,
        configure: impl FnOnce(&mut crate::generator::RandomShape),
    ) -> HashMap<String, Bag> {
        let mut shape = crate::generator::RandomShape {
            nodes,
            edges,
            reverse: false,
            union: false,
            negation: false,
            folds: [false; 3],
            along: false,
            rollup: false,
            best: None,
            prob: false,
        };
        configure(&mut shape);
        evaluate_bags(&crate::generator::build_random_program(&shape).oracle_rules)
    }

    /// ALONG facts are per path: 0->1 then two parallel 1->2 edges of equal
    /// weight give two `(0, 2, 3)` facts, and a FOLD over them counts both.
    #[test]
    fn along_is_one_fact_per_path() {
        let rels = shape_bags(
            vec![(0, 1, 1, false), (1, 2, 2, false), (1, 2, 2, false)],
            3,
            |s| {
                s.along = true;
            },
        );
        assert_eq!(
            rels["cost"],
            vec![
                vec![0, 1, 1],
                vec![0, 2, 3],
                vec![0, 2, 3],
                vec![1, 2, 2],
                vec![1, 2, 2]
            ]
        );
        assert_eq!(rels["tot"], vec![vec![0, 3, 7], vec![1, 2, 4]]);
    }

    /// BEST BY ASC over a cycle is the shortest path; DESC over the acyclic
    /// edges the longest.
    #[test]
    fn best_by_is_the_shortest_and_longest_path() {
        // 0 -1-> 1 -1-> 2, 0 -5-> 2, 2 -1-> 0 (a cycle).
        let edges = vec![
            (0, 1, 1, false),
            (1, 2, 1, false),
            (0, 2, 5, false),
            (2, 0, 1, false),
        ];
        let short = shape_bags(edges.clone(), 3, |s| s.best = Some(true));
        let pick = |rels: &HashMap<String, Bag>, a: i64, b: i64| {
            rels["opt"]
                .iter()
                .find(|r| r[0] == a && r[1] == b)
                .map(|r| r[2])
        };
        assert_eq!(pick(&short, 0, 2), Some(2));
        assert_eq!(pick(&short, 0, 0), Some(3));
        assert_eq!(pick(&short, 2, 1), Some(2));
        let long = shape_bags(edges, 3, |s| s.best = Some(false));
        assert_eq!(pick(&long, 0, 2), Some(5));
        assert_eq!(pick(&long, 2, 0), None, "the 2->0 edge is not acyclic");
    }

    /// A recursive FOLD reads each child's folded value: `s(a) = 1 +
    /// sum over a->c of (s(c) + w)`.
    #[test]
    fn recursive_fold_rolls_up_folded_values() {
        // 0 -2-> 1 -3-> 2, 0 -1-> 2. s(2)=1, s(1)=1+(1+3)=5, s(0)=1+(5+2)+(1+1)=10.
        let rels = shape_bags(
            vec![(0, 1, 2, false), (1, 2, 3, false), (0, 2, 1, false)],
            3,
            |s| {
                s.rollup = true;
            },
        );
        assert_eq!(rels["roll"], vec![vec![0, 10], vec![1, 5], vec![2, 1]]);
    }

    /// MNOR / MPROD over the bag of edges, and the probabilistic complement.
    #[test]
    fn probabilistic_rules() {
        // Two parallel 0->1 edges (0.2, 0.5) and 1->0 (0.3).
        let rels = shape_bags(
            vec![(0, 1, 2, false), (0, 1, 5, false), (1, 0, 3, false)],
            2,
            |s| {
                s.prob = true;
            },
        );
        let scale = |p: f64| (p * PROB_SCALE as f64).round() as i64;
        assert_eq!(
            rels["nor"],
            vec![vec![0, 1, scale(0.6)], vec![1, 0, scale(0.3)]]
        );
        assert_eq!(
            rels["prd"],
            vec![vec![0, 1, scale(0.1)], vec![1, 0, scale(0.3)]]
        );
        assert_eq!(
            rels["pr"],
            vec![
                vec![0, 1, scale(0.2)],
                vec![0, 1, scale(0.5)],
                vec![1, 0, scale(0.3)]
            ]
        );
        assert_eq!(
            rels["safe"],
            vec![
                vec![0, 0, scale(1.0)],
                vec![0, 1, scale(0.4)],
                vec![1, 0, scale(0.7)],
                vec![1, 1, scale(1.0)],
            ]
        );
    }

    /// Stratified `IS NOT`: `unreached` is the exact complement of `reaches`.
    ///
    /// Graph 0->1->2. reaches = {(0,1),(1,2),(0,2)}; all 9 ordered pairs minus
    /// those 3 = 6 unreached pairs. Exercises the anti-join + stratum ordering.
    #[test]
    fn stratified_complement_is_exact() {
        let edges: Vec<Tuple> = vec![vec![0, 1], vec![1, 2]];
        let all_pairs: Vec<Tuple> = (0..3)
            .flat_map(|a| (0..3).map(move |b| vec![a, b]))
            .collect();

        let reaches = OracleRule {
            per_path: false,
            best: None,
            fold: None,
            name: "reaches".to_string(),
            clauses: vec![
                OracleClause {
                    computed: Vec::new(),
                    prob_complements: Vec::new(),
                    base: edges.clone(),
                    var_cols: HashMap::from([("a".to_string(), 0), ("b".to_string(), 1)]),
                    pos_refs: Vec::new(),
                    neg_refs: Vec::new(),
                    yield_vars: vec!["a".to_string(), "b".to_string()],
                },
                OracleClause {
                    computed: Vec::new(),
                    prob_complements: Vec::new(),
                    base: edges,
                    var_cols: HashMap::from([("a".to_string(), 0), ("mid".to_string(), 1)]),
                    pos_refs: vec![IsRef {
                        values: Vec::new(),
                        rule: "reaches".to_string(),
                        subjects: vec!["mid".to_string()],
                        target: Some("b".to_string()),
                    }],
                    neg_refs: Vec::new(),
                    yield_vars: vec!["a".to_string(), "b".to_string()],
                },
            ],
        };
        let unreached = OracleRule {
            per_path: false,
            best: None,
            fold: None,
            name: "unreached".to_string(),
            clauses: vec![OracleClause {
                computed: Vec::new(),
                prob_complements: Vec::new(),
                base: all_pairs,
                var_cols: HashMap::from([("a".to_string(), 0), ("b".to_string(), 1)]),
                pos_refs: Vec::new(),
                neg_refs: vec![IsRef {
                    values: Vec::new(),
                    rule: "reaches".to_string(),
                    subjects: vec!["a".to_string(), "b".to_string()],
                    target: None,
                }],
                yield_vars: vec!["a".to_string(), "b".to_string()],
            }],
        };

        let program = OracleProgram {
            strata: vec![vec![reaches], vec![unreached]],
        };
        let rels = evaluate(&program);

        let expected_reaches: Relation = [vec![0, 1], vec![1, 2], vec![0, 2]].into_iter().collect();
        assert_eq!(rels["reaches"], expected_reaches);

        let expected_unreached: Relation = [
            vec![0, 0],
            vec![1, 0],
            vec![1, 1],
            vec![2, 0],
            vec![2, 1],
            vec![2, 2],
        ]
        .into_iter()
        .collect();
        assert_eq!(rels["unreached"], expected_unreached);
    }
}
