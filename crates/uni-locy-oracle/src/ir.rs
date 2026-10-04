//! Intermediate representation the oracle evaluates over.
//!
//! These types are the oracle's *own* minimal IR — deliberately independent of
//! the engine's AST/compiled forms — so the oracle and engine share no
//! evaluation code. The [`generator`](crate::generator) emits this IR alongside
//! the equivalent Locy program text from a single source of truth.
//!
//! A fact is a tuple of `i64`s (seeded node `id`s and integer values). A plain
//! rule's relation is a set of such tuples; a `FOLD` rule's is one row per
//! group; an `ALONG` rule's is a bag, one row per derivation path; a `BEST BY`
//! rule's is one row per key. A rule is a union of clauses, each a relational
//! join over base tuples plus `IS` / `IS NOT` references to other relations.

// Rust guideline compliant

use std::collections::HashMap;

/// One derived or base fact: a tuple of `i64` keys (seeded node `id`s).
///
/// The oracle works entirely in `i64` space; the differential harness recovers
/// these ids from the engine's whole-node `YIELD` output via `properties["id"]`.
pub type Tuple = Vec<i64>;

/// A reference from a clause body to another relation: `subjects IS rule [TO target]`.
///
/// For a positive reference the [`target`](IsRef::target) binds a new variable to
/// the referenced relation's trailing column. For a negated reference (`IS NOT`)
/// all subject variables are already bound and the clause keeps only bindings
/// whose subject tuple is *absent* from the referenced relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsRef {
    /// Name of the referenced relation (rule).
    pub rule: String,
    /// Local variable names supplying the subject (lookup-key) columns, in order.
    pub subjects: Vec<String>,
    /// Local variable bound to the reference's `TO` target column, if any.
    ///
    /// Always `None` for a negated reference.
    pub target: Option<String>,
    /// Local variables bound to the referenced fact's remaining columns, in
    /// order, after the subjects and the target: an `ALONG` value read as
    /// `prev.q`, or a recursive `FOLD`'s folded value. Empty for a negated
    /// reference.
    pub values: Vec<String>,
}

/// A variable a clause computes after its references are joined:
/// `constant + factor * sum of vars` — an `ALONG` step (`prev.q + e.w`), a fold
/// input (`s + e.w`), a constant seed (`1 AS s`), or a probability in
/// [`PROB_SCALE`] units (`e.w / 10.0` is `factor = PROB_SCALE / 10`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Computed {
    /// The variable bound.
    pub name: String,
    /// Variables summed.
    pub vars: Vec<String>,
    /// Multiplies the sum.
    pub factor: i64,
    /// Added to the product.
    pub constant: i64,
}

/// A probability `p` is held as the integer `round(p * PROB_SCALE)`.
pub const PROB_SCALE: i64 = 1_000_000_000;

/// A probabilistic `IS NOT` (#PROB): when both the referenced rule and this
/// one carry a PROB column, the reference is not an anti-join but a factor:
/// the referenced facts matching the subjects are combined by noisy-OR into
/// `q`, and the clause's probability column is multiplied by `1 - q` (by 1
/// when none match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbComplement {
    /// The reference (`negated` form; no target).
    pub reference: IsRef,
    /// The referenced rule's PROB column.
    pub prob_column: usize,
    /// This clause's PROB variable, multiplied in place.
    pub prob_var: String,
}

/// One clause (a single `CREATE RULE ... AS` definition) in relational-skeleton form.
///
/// Evaluation of a clause is: start from [`base`](OracleClause::base) bindings,
/// join in each positive reference, anti-join each negated reference, then project
/// to [`yield_vars`](OracleClause::yield_vars).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleClause {
    /// Base tuples the clause's `MATCH` contributes, known by construction from
    /// the generated graph. Each tuple is indexed by [`var_cols`](Self::var_cols).
    pub base: Vec<Tuple>,
    /// Column index, within each [`base`](Self::base) tuple, of each local variable.
    pub var_cols: HashMap<String, usize>,
    /// Positive `IS` references, applied as joins in order.
    pub pos_refs: Vec<IsRef>,
    /// Negated `IS NOT` references, applied as anti-joins.
    pub neg_refs: Vec<IsRef>,
    /// Local variables projected to the `YIELD KEY` columns, in output order.
    pub yield_vars: Vec<String>,
    /// Variables computed after the references, in order.
    pub computed: Vec<Computed>,
    /// Probabilistic `IS NOT` references, applied after `computed`.
    pub prob_complements: Vec<ProbComplement>,
}

/// A rule: a named relation defined as the union of its [`clauses`](OracleRule::clauses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleRule {
    /// The relation name (matches the engine's derived-relation key).
    pub name: String,
    /// Clauses whose results are unioned into this relation.
    pub clauses: Vec<OracleClause>,
    /// A `FOLD` over the rule's rows, when the rule aggregates. A clause may
    /// reference the rule itself: it then reads the **folded** relation (one
    /// row per key, from the previous iteration), and the rule is recomputed
    /// until no folded value moves.
    pub fold: Option<OracleFold>,
    /// The rule's facts are one row per derivation path (an `ALONG` rule,
    /// #159): a **bag**, recomputed from scratch each iteration, in which two
    /// paths with equal values are two facts. Terminates only on acyclic input.
    pub per_path: bool,
    /// `BEST BY`: one row per key, the one with the best criterion.
    pub best: Option<OracleBest>,
}

/// `BEST BY <column> ASC|DESC` over a rule's rows, grouped by its leading keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleBest {
    /// Leading key columns of each row.
    pub key_count: usize,
    /// The criterion's column.
    pub criterion: usize,
    /// `ASC` keeps the least criterion, `DESC` the greatest.
    pub ascending: bool,
}

/// A FOLD aggregate the oracle evaluates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    /// `COUNT(*)`: rows in the group.
    CountStar,
    /// `COUNT(x)`: non-null inputs in the group (inputs are never null here).
    Count,
    /// `SUM(x)` / `MSUM(x)`.
    Sum,
    /// `MIN(x)` / `MMIN(x)`.
    Min,
    /// `MAX(x)` / `MMAX(x)`.
    Max,
    /// `MNOR(x)`: `1 - prod(1 - x)`, inputs and output in [`PROB_SCALE`] units.
    NoisyOr,
    /// `MPROD(x)`: `prod(x)`, inputs and output in [`PROB_SCALE`] units.
    Product,
}

/// One aggregate of a [`OracleFold`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldAgg {
    /// The aggregate.
    pub agg: Agg,
    /// Column of the clause's projected row holding the input; `None` for
    /// [`Agg::CountStar`].
    pub input: Option<usize>,
}

/// A `FOLD` over a rule's rows: `YIELD KEY k1..kn, agg1, agg2, ...`.
///
/// Each clause projects its `yield_vars` as the `key_count` key columns
/// followed by the aggregates' input columns. Unlike a plain rule, which is a
/// **set**, the fold reads the **bag** of those rows — every binding counts,
/// so parallel edges contribute once each — grouped by the key columns. The
/// fact for a group is its key followed by each aggregate's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleFold {
    /// Leading key columns of each projected row.
    pub key_count: usize,
    /// The aggregates, in output order.
    pub aggs: Vec<FoldAgg>,
}

/// A stratified program: rules grouped into dependency-ordered strata.
///
/// The generator owns stratum order (it controls program shape); negated
/// references only target rules in strictly earlier strata, so each stratum can
/// be driven to a least fixpoint before the next begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleProgram {
    /// Strata in dependency order; each inner `Vec` is one stratum's rules.
    pub strata: Vec<Vec<OracleRule>>,
}

/// The single-source-of-truth triple emitted by a generator builder.
///
/// The same parameters produce all three faces — the engine consumes
/// [`base_graph_cypher`](Generated::base_graph_cypher) + [`program_text`](Generated::program_text),
/// while the oracle consumes [`oracle_rules`](Generated::oracle_rules) — so the
/// two sides share inputs but no evaluation code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// Cypher that seeds the base graph (nodes carry an integer `id` property).
    pub base_graph_cypher: String,
    /// The Locy program the engine evaluates.
    pub program_text: String,
    /// The oracle IR equivalent of `program_text`.
    pub oracle_rules: OracleProgram,
    /// `YIELD KEY` column names per derived relation; drives `FactRow` extraction.
    pub key_schema: HashMap<String, Vec<String>>,
}
