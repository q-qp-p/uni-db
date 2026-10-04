// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Delta tracking and IS-reference parameter building for Locy native execution.
//!
//! Relocated from `uni-locy/src/orchestrator/delta.rs` and parts of
//! `uni-locy/src/orchestrator/fixpoint.rs` during Phase 7.

use std::collections::HashMap;

use uni_common::Value;
use uni_cypher::ast::Expr;
use uni_cypher::locy_ast::{IsReference, RuleCondition};
use uni_locy::types::{CompiledClause, CompiledRule};
use uni_locy::{FactRow, LocyError};

use super::locy_ast_builder::expr_references_var;
use super::locy_eval::{
    eval_condition, eval_locy_expr, record_batches_to_locy_rows, values_equal_for_join,
};
use super::locy_traits::DerivedFactSource;

/// A tuple of key column values used for indexing derived facts.
pub type KeyTuple = Vec<Value>;

/// A minimal row-based relation for SLG/EXPLAIN command dispatch.
#[derive(Debug, Clone)]
pub struct RowRelation {
    pub columns: Vec<String>,
    pub rows: Vec<FactRow>,
}

impl RowRelation {
    pub fn new(columns: Vec<String>, rows: Vec<FactRow>) -> Self {
        Self { columns, rows }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// A map from rule name to its RowRelation (row-based store for SLG/EXPLAIN).
pub type RowStore = HashMap<String, RowRelation>;

/// Extracts the key tuple from a row given key column names.
pub fn extract_key(row: &FactRow, key_cols: &[String]) -> KeyTuple {
    key_cols
        .iter()
        .map(|k| row.get(k).cloned().unwrap_or(Value::Null))
        .collect()
}

/// Extract Cypher expression conditions from rule conditions (skip IS references).
pub fn extract_cypher_conditions(conditions: &[RuleCondition]) -> Vec<Expr> {
    conditions
        .iter()
        .filter_map(|c| match c {
            RuleCondition::Expression(expr) => Some(expr.clone()),
            _ => None,
        })
        .collect()
}

// ── In-memory IS-ref join helpers ─────────────────────────────────────────────

/// How an unbound subject is treated when matching an IS-ref.
///
/// The two modes exist because `is_ref_matches` backs both the positive
/// semi-join and the negation paths, and relaxing it for one silently changes
/// the semantics of the other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SubjectMode {
    /// A subject absent from the base row is a **generator**: it matches every
    /// derived fact and is bound from it by [`semi_join_is_ref`].
    ///
    /// This is what makes `WHERE (r, p) IS role_perm` work when `p` appears
    /// nowhere in the MATCH pattern. Previously the absent subject collapsed to
    /// `Value::Null`, compared unequal to every fact, and the whole query
    /// silently returned zero rows while the fixpoint derived facts normally
    /// (issue #160).
    Bind,
    /// A subject absent from the base row compares as `Null` and therefore
    /// matches nothing — the historical behaviour, kept for the negation and
    /// probabilistic-complement paths.
    ///
    /// Applying `Bind` there would flip `IS NOT` with an unbindable subject
    /// from keep-all to keep-**nothing**. That case is meaningless (an unbound
    /// variable under negation) and is rejected at compile time instead.
    Strict,
}

/// Check whether a derived fact matches the subject bindings from a base row.
///
/// Positional: `subjects[i]` maps to `schema[i]` in the derived relation.
fn is_ref_matches(
    base_row: &FactRow,
    derived_fact: &FactRow,
    is_ref: &IsReference,
    schema: &[String],
    mode: SubjectMode,
) -> bool {
    // Check subject columns.
    for (i, subject) in is_ref.subjects.iter().enumerate() {
        if i >= schema.len() {
            break;
        }
        // Distinguish "not bound at all" from "bound to NULL". Only the former
        // is a generator; a genuine NULL binding must still compare.
        //
        // `Strict` deliberately keeps the historical `unwrap_or(Value::Null)`
        // rather than short-circuiting: `values_equal(Null, Null)` is `true`, so
        // an absent subject matching an absent fact column is an existing
        // behaviour the negation paths may depend on.
        let base_val = match base_row.get(subject) {
            Some(v) => v.clone(),
            None if mode == SubjectMode::Bind => continue,
            None => Value::Null,
        };
        let fact_val = derived_fact.get(&schema[i]).cloned().unwrap_or(Value::Null);
        if !values_equal_for_join(&base_val, &fact_val) {
            return false;
        }
    }
    // Also check target variable for composite-key matching (negated IS NOT).
    // Without this, `d IS NOT known TO dis` only checks d, not (d, dis).
    // Skip when base_val is Null (target not yet bound — positive IS-ref binding).
    if let Some(target) = &is_ref.target {
        let target_idx = is_ref.subjects.len();
        if target_idx < schema.len() {
            let base_val = base_row.get(target).cloned().unwrap_or(Value::Null);
            let fact_val = derived_fact
                .get(&schema[target_idx])
                .cloned()
                .unwrap_or(Value::Null);
            if base_val != Value::Null
                && fact_val != Value::Null
                && !values_equal_for_join(&base_val, &fact_val)
            {
                return false;
            }
        }
    }
    true
}

/// Positive IS semi-join: for each base row, produce one output row per matching
/// derived fact, binding the optional target variable and stashing derived fact
/// columns under `__prev_{col}` for ALONG PrevRef resolution.
fn semi_join_is_ref(
    base_rows: &[FactRow],
    derived_facts: &[FactRow],
    is_ref: &IsReference,
    schema: &[String],
) -> Vec<FactRow> {
    let mut result = Vec::new();
    for base_row in base_rows {
        for derived_fact in derived_facts {
            if is_ref_matches(base_row, derived_fact, is_ref, schema, SubjectMode::Bind) {
                let mut row = base_row.clone();

                // Bind subject variables the MATCH pattern did not provide.
                // This loop is what turns an IS-ref into a generator: the outer
                // nested loop is already a cross product, so a fresh subject
                // simply fans one base row out to one row per derived fact.
                // Mirrors the target binding just below.
                for (i, subject) in is_ref.subjects.iter().enumerate() {
                    if i >= schema.len() || base_row.contains_key(subject) {
                        continue;
                    }
                    if let Some(val) = derived_fact.get(&schema[i]) {
                        row.insert(subject.clone(), val.clone());
                    }
                }

                // Bind target variable (column after subjects in the derived schema).
                if let Some(target) = &is_ref.target {
                    let target_col_idx = is_ref.subjects.len();
                    if target_col_idx < schema.len() {
                        let val = derived_fact
                            .get(&schema[target_col_idx])
                            .cloned()
                            .unwrap_or(Value::Null);
                        row.insert(target.clone(), val);
                    }
                }

                // Bind non-KEY value columns from the derived fact so that
                // downstream YIELD expressions can reference them (e.g.,
                // `WHERE d IS signal TO dis ... YIELD KEY d, KEY dis, agg`
                // needs `agg` to be present in the row).
                let bound_count =
                    is_ref.subjects.len() + if is_ref.target.is_some() { 1 } else { 0 };
                for col in schema.iter().skip(bound_count) {
                    if let Some(val) = derived_fact.get(col) {
                        row.entry(col.clone()).or_insert_with(|| val.clone());
                    }
                }

                // Stash derived fact columns for ALONG PrevRef lookups.
                for (col, val) in derived_fact {
                    row.insert(format!("__prev_{}", col), val.clone());
                }

                result.push(row);
            }
        }
    }
    result
}

/// Negative IS anti-join: retain base rows where no derived fact matches.
fn anti_join_is_ref(
    base_rows: &[FactRow],
    derived_facts: &[FactRow],
    is_ref: &IsReference,
    schema: &[String],
) -> Vec<FactRow> {
    base_rows
        .iter()
        .filter(|base_row| {
            !derived_facts
                .iter()
                .any(|df| is_ref_matches(base_row, df, is_ref, schema, SubjectMode::Strict))
        })
        .cloned()
        .collect()
}

/// Probabilistic complement for IS NOT with PROB target.
///
/// Instead of filtering out matched rows (anti-join), retains ALL rows and
/// attaches a complement column: `1-p` for matched keys, `1.0` for absent keys.
/// When multiple derived facts match the same key, probabilities are combined
/// via noisy-OR: `p_combined = 1 - ∏(1 - pᵢ)`.
///
/// Mirrors [`super::locy_fixpoint::apply_prob_complement_composite`] but
/// operates on `Vec<FactRow>` instead of `Vec<RecordBatch>`.
fn prob_complement_is_ref(
    base_rows: &[FactRow],
    derived_facts: &[FactRow],
    is_ref: &IsReference,
    schema: &[String],
    target_prob_col: &str,
    complement_col_name: &str,
) -> Vec<FactRow> {
    base_rows
        .iter()
        .map(|base_row| {
            // Find all matching derived facts and combine via noisy-OR.
            let mut combined_p = 0.0_f64;
            for df in derived_facts {
                if is_ref_matches(base_row, df, is_ref, schema, SubjectMode::Strict) {
                    let p = df
                        .get(target_prob_col)
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0)
                        .clamp(0.0, 1.0);
                    combined_p = 1.0 - (1.0 - combined_p) * (1.0 - p);
                }
            }
            let complement = 1.0 - combined_p;
            let mut row = base_row.clone();
            row.insert(complement_col_name.to_string(), Value::Float(complement));
            row
        })
        .collect()
}

/// Multiply `__prob_complement_*` columns into the PROB column and remove them.
///
/// Mirrors [`super::locy_fixpoint::multiply_prob_factors`] but operates on
/// `Vec<FactRow>` instead of `Vec<RecordBatch>`.
pub fn multiply_prob_factors_rows(
    rows: &mut [FactRow],
    prob_col: &str,
    complement_cols: &[String],
) {
    for row in rows.iter_mut() {
        let mut factor = 1.0_f64;
        for col in complement_cols {
            if let Some(v) = row.get(col).and_then(|v| v.as_f64()) {
                factor *= v;
            }
            row.remove(col);
        }
        let current = row.get(prob_col).and_then(|v| v.as_f64()).unwrap_or(1.0);
        row.insert(prob_col.to_string(), Value::Float(current * factor));
    }
}

/// Resolve a clause by executing its MATCH pattern and applying IS-ref joins in memory.
///
/// Replaces the serialize-UNWIND-execute round-trip used by SLG and EXPLAIN Mode B.
/// IS-refs are resolved as in-memory semi-joins (positive) or anti-joins (negated).
/// ALONG expressions are evaluated last using `__prev_*` stash from semi-join results.
pub async fn resolve_clause_with_is_refs(
    clause: &CompiledClause,
    fact_source: &dyn DerivedFactSource,
    derived_store: &RowStore,
    rule_catalog: &HashMap<String, CompiledRule>,
    calling_rule_prob_col: Option<&str>,
) -> Result<Vec<FactRow>, LocyError> {
    // Collect all variable names introduced by IS-ref joins:
    // subjects the MATCH pattern does not bind, target variables, AND non-KEY
    // value columns from target rules. These must be deferred until after the
    // IS-ref join resolves.
    let match_bound = super::mutation_common::pattern_variable_names(&clause.match_pattern);
    let mut is_ref_vars: Vec<String> = Vec::new();
    for cond in &clause.where_conditions {
        if let RuleCondition::IsReference(is_ref) = cond
            && !is_ref.negated
        {
            // A subject the MATCH pattern does not provide is bound *by* this
            // IS-ref (see `SubjectMode::Bind`). Without listing it here, a
            // predicate referencing it is classified `match_safe` and pushed
            // into the base Cypher MATCH with an unbound variable.
            for subject in &is_ref.subjects {
                if !match_bound.contains(subject) {
                    is_ref_vars.push(subject.clone());
                }
            }
            if let Some(target) = &is_ref.target {
                is_ref_vars.push(target.clone());
            }
            let rule_name = is_ref.rule_name.to_string();
            if let Some(target_rule) = rule_catalog.get(&rule_name) {
                for col in &target_rule.yield_schema {
                    if !col.is_key {
                        is_ref_vars.push(col.name.clone());
                    }
                }
            }
        }
    }

    // Partition Cypher conditions: match-safe vs. IS-ref-dependent.
    let all_cypher = extract_cypher_conditions(&clause.where_conditions);
    let mut match_safe = Vec::new();
    let mut target_dependent = Vec::new();
    for expr in all_cypher {
        if is_ref_vars.iter().any(|v| expr_references_var(&expr, v)) {
            target_dependent.push(expr);
        } else {
            match_safe.push(expr);
        }
    }

    // Execute base MATCH query.
    let raw_batches = fact_source
        .execute_pattern(&clause.match_pattern, &match_safe)
        .await?;
    let mut rows = record_batches_to_locy_rows(&raw_batches);

    // Apply IS-ref joins/anti-joins sequentially.
    for cond in &clause.where_conditions {
        if let RuleCondition::IsReference(is_ref) = cond {
            let rule_name = is_ref.rule_name.to_string();
            let (schema, derived_facts) = if let Some(relation) = derived_store.get(&rule_name) {
                (relation.columns.clone(), relation.rows.clone())
            } else {
                (Vec::new(), Vec::new())
            };

            if is_ref.negated {
                // Check if target rule has PROB column for complement semantics.
                let target_prob_col = rule_catalog.get(&rule_name).and_then(|r| {
                    r.yield_schema
                        .iter()
                        .find(|c| c.is_prob)
                        .map(|c| c.name.clone())
                });

                if let (Some(tpc), Some(_)) = (&target_prob_col, &calling_rule_prob_col) {
                    // Probabilistic complement: keep all rows, compute 1-p.
                    let complement_col = format!("__prob_complement_{}", rule_name);
                    rows = prob_complement_is_ref(
                        &rows,
                        &derived_facts,
                        is_ref,
                        &schema,
                        tpc,
                        &complement_col,
                    );
                } else {
                    // Boolean anti-join (existing behavior).
                    rows = anti_join_is_ref(&rows, &derived_facts, is_ref, &schema);
                }
            } else {
                rows = semi_join_is_ref(&rows, &derived_facts, is_ref, &schema);
            }
        }
    }

    // NOTE: __prob_complement_* columns are left in the rows for the caller
    // (locy_slg.rs) to multiply into the PROB column AFTER yield projections.
    // This is because apply_yield_projections() re-evaluates the YIELD expression
    // (e.g., "1.0 AS safety PROB") and would overwrite any pre-multiplied value.

    // Apply target-dependent Cypher conditions.
    if !target_dependent.is_empty() {
        let mut kept = Vec::with_capacity(rows.len());
        'rows: for row in rows {
            for expr in &target_dependent {
                if !eval_condition(expr, &row, "rule WHERE")? {
                    continue 'rows;
                }
            }
            kept.push(row);
        }
        rows = kept;
    }

    // Evaluate ALONG expressions (using __prev_* stash for PrevRef lookups).
    if !clause.along.is_empty() {
        let mut new_rows = Vec::with_capacity(rows.len());
        for mut row in rows {
            let prev_values: FactRow = row
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix("__prev_")
                        .map(|col| (col.to_string(), v.clone()))
                })
                .collect();

            for along in &clause.along {
                let result = eval_locy_expr(&along.expr, &row, Some(&prev_values))?;
                row.insert(along.name.clone(), result);
            }
            new_rows.push(row);
        }
        rows = new_rows;
    }

    // Remove __prev_* staging keys from output rows.
    for row in &mut rows {
        row.retain(|k, _| !k.starts_with("__prev_"));
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uni_common::Value;
    use uni_cypher::locy_ast::{IsReference, QualifiedName};

    fn make_is_ref(subject: &str, rule: &str) -> IsReference {
        IsReference {
            subjects: vec![subject.to_string()],
            rule_name: QualifiedName {
                parts: vec![rule.to_string()],
            },
            target: None,
            negated: true,
        }
    }

    fn row_int(pairs: &[(&str, i64)]) -> FactRow {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::Int(*v)))
            .collect()
    }

    #[test]
    fn test_prob_complement_basic() {
        // Alice in risky with p=0.7 → complement=0.3
        // Bob in risky with p=0.3 → complement=0.7
        // Charlie absent → complement=1.0
        let base = vec![
            row_int(&[("n", 1)]),
            row_int(&[("n", 2)]),
            row_int(&[("n", 3)]),
        ];
        let derived = vec![
            {
                let mut r = row_int(&[("n", 1)]);
                r.insert("risk_score".into(), Value::Float(0.7));
                r
            },
            {
                let mut r = row_int(&[("n", 2)]);
                r.insert("risk_score".into(), Value::Float(0.3));
                r
            },
        ];
        let is_ref = make_is_ref("n", "risky");
        let schema = vec!["n".to_string(), "risk_score".to_string()];

        let result = prob_complement_is_ref(&base, &derived, &is_ref, &schema, "risk_score", "__c");

        assert_eq!(result.len(), 3);
        let c0 = result[0].get("__c").unwrap().as_f64().unwrap();
        let c1 = result[1].get("__c").unwrap().as_f64().unwrap();
        let c2 = result[2].get("__c").unwrap().as_f64().unwrap();
        assert!((c0 - 0.3).abs() < 1e-10, "Alice: expected 0.3, got {c0}");
        assert!((c1 - 0.7).abs() < 1e-10, "Bob: expected 0.7, got {c1}");
        assert!((c2 - 1.0).abs() < 1e-10, "Charlie: expected 1.0, got {c2}");
    }

    #[test]
    fn test_prob_complement_noisy_or_duplicates() {
        // n=1 appears twice with p=0.3 and p=0.5
        // Noisy-OR: 1-(1-0.3)(1-0.5) = 0.65, complement = 0.35
        let base = vec![row_int(&[("n", 1)])];
        let derived = vec![
            {
                let mut r = row_int(&[("n", 1)]);
                r.insert("prob".into(), Value::Float(0.3));
                r
            },
            {
                let mut r = row_int(&[("n", 1)]);
                r.insert("prob".into(), Value::Float(0.5));
                r
            },
        ];
        let is_ref = make_is_ref("n", "rule");
        let schema = vec!["n".to_string(), "prob".to_string()];

        let result = prob_complement_is_ref(&base, &derived, &is_ref, &schema, "prob", "__c");
        let c = result[0].get("__c").unwrap().as_f64().unwrap();
        assert!((c - 0.35).abs() < 1e-10, "expected 0.35, got {c}");
    }

    #[test]
    fn test_multiply_prob_factors_rows_basic() {
        // safety=1.0, complement=0.3 → result safety=0.3, complement col removed
        let mut rows = vec![{
            let mut r = FactRow::new();
            r.insert("safety".into(), Value::Float(1.0));
            r.insert("__prob_complement_risky".into(), Value::Float(0.3));
            r
        }];
        multiply_prob_factors_rows(&mut rows, "safety", &["__prob_complement_risky".into()]);

        assert!(
            !rows[0].contains_key("__prob_complement_risky"),
            "complement col should be removed"
        );
        let safety = rows[0].get("safety").unwrap().as_f64().unwrap();
        assert!((safety - 0.3).abs() < 1e-10, "expected 0.3, got {safety}");
    }

    #[test]
    fn test_anti_join_unchanged_without_prob() {
        // Boolean IS NOT: n=1 in flagged → excluded, n=2 not → kept
        let base = vec![row_int(&[("n", 1)]), row_int(&[("n", 2)])];
        let derived = vec![row_int(&[("n", 1)])];
        let is_ref = make_is_ref("n", "flagged");
        let schema = vec!["n".to_string()];

        let result = anti_join_is_ref(&base, &derived, &is_ref, &schema);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].get("n").unwrap().as_i64(), Some(2));
    }
}
