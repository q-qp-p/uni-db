# Finding silent wrong answers by class, not by customer report

**Date:** 2026-09-28 · **Status:** W1 done (all confirmed defects fixed, plus four found on the way); W2 done (found the OPTIONAL MATCH clause-close class); W3 done (query-rewrite relations, topology fixture, wide tier; eleven engine defects found); W4 done (Locy↔Cypher relations and Locy through the fork, pinned and flush levers); W5 done (oracle covers ALONG, recursive FOLD, BEST BY and PROB); W6 done (fail-open evaluation sites made loud, plus a debug invariant) · **Trigger:** issues #293, #294

## Why

Four silent-wrong-answer bugs were fixed on 2026-09-28 (`d8964d1a4`,
`b863c7b79`, `c4d24b15f`, `0993d4f68`). Each one was cheap to catch *after*
the fact with an oracle that needs no reference implementation — the engine
compared with itself under a transformation that must not change the answer:

| Bug | Class | Oracle that catches it | Input shape it needs |
|---|---|---|---|
| #294 recursive FOLD kept one of two parallel edges | identity key omitted a component | recursive form ≡ non-recursive form | parallel edges, diamonds |
| anonymous VLP returned one row per endpoint | optimization with an unchecked precondition | named relationship ≡ anonymous relationship | ≥ 2 equal-length paths to one endpoint |
| chunked scan dropped unflushed rows past a vid gap | decision made from a partial view (L1 only) | before flush ≡ after flush | > 8192 rows of a label, a vid gap, then L0 rows |
| seeded FOLD type mismatch | — (loud) | recursive ≡ non-recursive | seed + fold clauses with differing literal types |

None of the existing harnesses could have caught any of them, for structural
reasons (measured, see §4): the DQP fixture is a bipartite one-hop graph with
no parallel edges, diamonds or cycles; its flush lever runs only on a
1000-row tier smaller than one scan slice; `querygen` never emits
variable-length paths, named relationships, `OPTIONAL MATCH`, `DISTINCT`,
`EXISTS` or `UNWIND`; no generated Locy program ever passes through a lever;
and the naive Locy oracle's generator has three fixed templates. The tests
were written by people who know the idioms, over data shaped the way they
expected.

## 1. The three classes

1. **Identity** — a key built for dedup / merge / group / "seen" omits a
   component, so two distinct things collapse, often keep-last and therefore
   nondeterministically.
2. **Partial view** — a read-path decision consults one of {L1, L0 current,
   L0 transaction, L0 pending-flush, fork branch, pinned snapshot} when the
   answer depends on their union.
3. **Unchecked precondition** — a rewrite or fast path is valid only if the
   consumer cannot observe something (multiplicity, a constraint, a NULL row),
   and that is assumed rather than checked.

## 2. Audit results (2026-09-28)

Four read-only audits enumerated candidates per class; the top suspects were
then probed differentially (`crates/uni/tests/common/bugs/class_audit_probes.rs`,
each probe = suspect query vs a differently-formulated control). **Confirmed**
means measured; everything else is a hypothesis.

| # | Class | Confirmed defect | Control that exposed it |
|---|---|---|---|
| 1 | precondition | Vectorized pattern predicate / pattern comprehension drops the anchor label, relationship property maps and relationship uniqueness (`WHERE (n:Person)-[:R]->()` also matches a Robot) | same pattern inside `EXISTS { MATCH … }` / `COUNT { MATCH … }` |
| 2 | identity | `OptionalFilterExec` groups by VID only: `UNWIND [2,3] AS x MATCH (a) OPTIONAL MATCH (a)-->(b) WHERE b.v = x` drops the `(3, null)` row | expected bag |
| 3 | identity | Locy: facts of a multi-rule stratum are stored under each rule's name, so `QUERY even` over mutually recursive `even`/`odd` returns odd's rows too | expected bag |
| 4 | identity | Locy fixpoint rounds every Float64 fact to 1e-12: an ALONG product of 1e-14 becomes 0.0 | expected value |
| 5 | partial view | FTS returns a stale L1 hit after an unflushed `SET` | expected bag |
| 6 | partial view | Vertex UNIQUE rejects re-creating a key after an unflushed delete | Cypher semantics |
| 7 | partial view | Vertex UNIQUE rejects reusing a key after a flushed change (append-mode rows, no MVCC dedup in the probe) | Cypher semantics |
| 8 | partial view | Non-DETACH `DELETE` of a node with an edge created in the same transaction succeeds | Cypher semantics |
| 9 | partial view | Adding a NOT NULL property to an all-L0 label is accepted, then **every flush fails** | flush |
| 10 | (loud) | `EXISTS { MATCH (n:Label)-… }` with `n` bound fails: `No field named "n._labels"` | — |
| 11 | (loud) | Unlabelled `MATCH (n {ext_id:'x'}) RETURN n.name` fails with a schema error | `count(n)` works |

Found and fixed during W1, beyond the table: label disjunction on a traversal
target matched nothing; inline element `WHERE` was ignored everywhere; vector
search had the FTS stale-version defect; a vertex `UNIQUE` key moved by an
unflushed `SET` stayed taken. The invariants are recorded in the Black Book,
Appendix B2.

Re-probed on the W3 topology fixture (2026-10-01): of the "not reproduced"
suspects below, the reachability trail check was real (fixed in W3);
"WHERE pushdown through a shadowing WITH" was real — a rename onto a used
name failed to plan, and a swap read the old binding's columns, silently;
`count(DISTINCT m)` over `_id` maps was right, but `collect(DISTINCT m)`,
`=` and `IN` merged any map with an `_id`/`_vid`/`vid`/`_eid` key; the
left-of-anchor hop was ruled out (8 shapes, both layouts). All fixed.

Not reproduced on the probe graphs (weak evidence only): the reachability
BFS first-predecessor trail check, `WHERE` pushdown through a shadowing
`WITH`, `count(DISTINCT m)` over maps carrying `_id`. Remaining unprobed
candidates are listed in the audit notes and should be probed before they are
dismissed.

## 3. The harness: metamorphic relations the engine must satisfy

Each relation is an oracle that needs no reference implementation. Every one
is justified by a bug it would have caught.

| Relation | Catches | Status |
|---|---|---|
| named relationship ≡ anonymous relationship | anonymous VLP multiplicity | hand test exists (`vlp_anonymous_path_multiplicity`) |
| pattern predicate ≡ `EXISTS { MATCH … }`; pattern comprehension size ≡ `COUNT { MATCH … }` | #1 | new |
| recursive rule ≡ non-recursive rule on acyclic data of depth ≤ k (unroll) | #294, seed typing | hand tests exist |
| Locy non-recursive rule ≡ equivalent Cypher `MATCH … RETURN key, agg(...)` | FOLD grouping, #293 | new |
| before flush ≡ after flush, **on fixtures with vid gaps and > 1 slice per label** | scan L0 gap, FTS stale, UNIQUE | lever exists; fixture does not |
| inside transaction ≡ after commit | non-DETACH delete | new |
| `target_partitions` ∈ {1, 2, 8}, batch size ∈ {1, 8192}, fresh session × N | #294's nondeterminism, keep-last merges; OPTIONAL null-fill per batch | done for the TCKs (W2) |
| `MATCH (a) OPTIONAL MATCH p` ≡ `MATCH (a) MATCH p` ⊎ `MATCH (a) WHERE NOT EXISTS { MATCH p }` padded with NULL | OPTIONAL clause close (W2) | hand test (`bugs::optional_match_clause_close`); a lever for W3 |
| adding a parallel edge with value v changes MSUM by exactly v; splitting a node does not change totals | identity keys | new |

## 4. Work items, in order

**W1 — fix the confirmed defects** (§2), each with its probe promoted to a
regression test that fails on the old code. Suggested order by blast radius:
9 (flush wedge), 8, 2, 1, 6/7, 5, 3, 4, then 10/11.

**W2 — make batch size and partition count settable.** `UniConfig.batch_size`
reaches only the cursor (`impl_query.rs:797`); `UniConfig.parallelism` has no
reader. Sessions build `SessionConfig::new()` at `api/mod.rs:2161` and
`executor/read.rs:548/552/557`. Wire both through, then add a Tier-3 lever and
a determinism check that runs the Locy TCK and the Cypher TCK under
`target_partitions ∈ {1, 8}` and `batch_size ∈ {1, 8192}`. This reuses ~4450
existing hand-written expectations.

*W2 result.* `UniConfig.parallelism` now sets DataFusion `target_partitions`,
and a new `UniConfig.execution_batch_size` (default `None` = DataFusion's 8192)
sets the engine batch size; `batch_size` is documented as what it is, the
cursor page size. Both TCK harnesses read `UNI_TCK_PARALLELISM` /
`UNI_TCK_EXECUTION_BATCH_SIZE` (a malformed value fails every scenario, so a
run cannot silently fall back to the defaults). Swept at (1, 1), (8, 7) and
(2, 8192) against the default: Locy 528/528 everywhere; Cypher 3925/3925
except `Graph6[6]` at batch size 1 — a leading `OPTIONAL MATCH` emitted one
NULL row per batch. Mapping it with the OPTIONAL oracle above found the class,
most of it wrong **at default settings**: every dead end of a multi-step
OPTIONAL pattern emitted its own NULL row, a comma-separated second path
extended rows the first had failed (`[6, 7]` where `[6, NULL]` was due), and a
leading clause with a labelled start emitted a NULL row per start vertex. Fixed
by closing every multi-step OPTIONAL clause with one evidence-aware
`OptionalFilterExec` (Black Book B2). `parallelism` moved no counter the DQP
Tier-3 probe observes; `execution_batch_size` does, and is now its observable
knob.

**W3 — widen the DQP fixture and generator.**
- Fixture: add a second relationship type over one label with parallel edges,
  diamonds, self-loops and a cycle; add a label with > 8192 rows and an
  interleaved second label so vids have gaps; keep a slice-sized L0 delta.
- `querygen`: emit variable-length relationships (named and anonymous),
  `OPTIONAL MATCH`, `DISTINCT`, `EXISTS { }` and pattern predicates, `UNWIND`
  over a literal list, and min/max/avg/collect.
- New Tier-2 levers that rewrite the *query* (the driver currently passes the
  same query to both sides, `driver.rs:390`): named↔anonymous,
  predicate↔`EXISTS`, comprehension↔`COUNT`.

*W3 result (in part).* `metamorphic::dqp::topo` holds data and engine fixed
and compares each generated query with equivalent formulations — six relations
(named ↔ anonymous relationship; `OPTIONAL MATCH` ↔ `MATCH` ⊎ `NOT EXISTS`;
`EXISTS` ↔ `COUNT > 0` ↔ pattern predicate; comprehension ↔ `COUNT {}`;
`*i..j` ↔ ⊎ₖ `*k..k` ↔ ⊎ₖ k fixed hops; `DISTINCT` ↔ grouping) — over a
fixture with chains, a diamond, parallel and identical-parallel edges,
self-loops, 2- and 3-cycles, fan-in/out, a second label and a dense cluster,
flushed and half in L0 at a two-row execution batch. Every reference query
also runs twice. Activation = the relation applies *and* the reference returns
rows (floor 30%; measured 47–85% per relation). The first runs found four
silent wrong answers, all at default settings, each with a regression test in
`bugs/`: a hop after a variable-length relationship reused its edges (221 rows
for 104); the reachability BFS dropped endpoints in an order-dependent way
(95–103 of 103 rows, run to run); a chunked OPTIONAL traversal emitted a NULL
row per chunk; and an unbound OPTIONAL entity was a non-null struct of NULLs
(`labels()` failed, `keys()` gave `[]` or the declared property names, a list
or map holding it became NULL).

*W3 second round.* Three more relations — aggregates against `reduce` over
`collect`, `UNWIND` of a list against the sum over its elements, `UNWIND
collect(x)` against the non-null rows — and a wide DQP tier (`Tier::Wide`:
> 8192 `Person` rows with vid gaps and a 65 536-row filler block) under the
flush lever, which fails on its first case with the scan-range fix reversed.
They found: `reduce` truncating float elements to the accumulator's integer
type, and panicking in Arrow with a `null` start; comprehensions, quantifiers,
`reduce` and pattern comprehensions failing — or, for a pattern comprehension,
returning empty — inside a `CASE` branch; mixed Int/Float arithmetic and
comparison failing when an operand holds a custom expression; a repeated
equality on a variable-length target making Lance reject a duplicate column;
and `elementId` failing to plan on every MATCH-bound variable. Each has a
regression test that fails with its fix reversed.

Decided 2026-10-01: `sum` over no non-null value is 0, as in Neo4j, not
NULL as in SQL. It had been NULL from DataFusion's `sum` and the Cypher-value
`sum`, but 0 from the row executor's accumulator; all three now agree on 0
(`bugs::sum_of_nothing_is_zero`), and the `aggregate` relation folds from 0.
`sum(...) OVER` windows follow the same rule; making them do so exposed that
a window `sum` cast every argument to an integer, so float window sums were
truncated (fixed in the same change).

**W4 — Locy through the levers.** A program-text case type and generator
(FOLD with composite keys, seeds, recursion, ALONG, parallel edges), an
`observe_locy` on `session.locy()`, a bag over derived facts, and the
recursion-unrolling and Locy↔Cypher relations. Verify first that
`LocyResult::metrics()` counters actually move, or the activation floor is
vacuous.

*W4 result (in part).* Five Locy relations in `metamorphic::dqp::topo`, each
against an independent Cypher formulation over the topology fixture (flushed,
and half in L0 at a two-row batch): a rule's facts ≡ `DISTINCT` of its body;
`FOLD COUNT/SUM/MIN/MAX` ≡ the grouped Cypher aggregate; recursive
reachability ≡ `-[*1..]->` with `DISTINCT`; `IS NOT` ≡ `NOT EXISTS`; a
`QUERY ... WHERE` filter ≡ the same filter in the body. Every Locy reference
also runs twice. They found: non-recursive rules did not deduplicate facts
(one per parallel edge); `QUERY ... WHERE` used two-valued logic with NULL;
`MIN`/`MAX`/`COLLECT` and properties of unlabelled nodes or relationships
came back as Float64; the W3 variable-length uniqueness fix had not reached
Locy rule bodies (a second planning entry point); and, decided 2026-10-01,
Locy `SUM`/`MSUM` of nothing now matches Cypher's 0. Loud, not fixed:
`count(*)` in a `QUERY ... RETURN` ("unsupported expression: Wildcard"), and
`x AND NOT a IS r TO b` is a parse error (`IS NOT` alone works). Not done
from W4: a program generator with composite keys, seeds, ALONG and recursive
FOLD (W5's multiset oracle is the reference those need).

**W5 — extend the naive Locy oracle** to FOLD and monotonic aggregates over
**multisets** (so duplicate contributions are not collapsed), with a random
program generator in place of the three templates.

*W5 result (in part).* The naive oracle (`uni-locy-oracle`) evaluates
non-recursive FOLD over the **bag** of a rule's rows (`COUNT(*)`, `COUNT`,
`SUM`/`MSUM`, `MIN`/`MMIN`, `MAX`/`MMAX`, `MCOUNT`), and
`random_program_strategy` replaces the three templates as the main source of
cases: a random multigraph (self-loops, parallel and identical edges, two
relationship types, integer weights) under random strata of base relations,
recursive reachability, stratified negation and three FOLD rules (over edges,
over the closure, over an edge joined with the closure). Every relation is
compared, values included, with types checked strictly (only sums may be
floats). Each relation must derive facts in at least a fifth of the cases.
It catches the oracle made to use set semantics, and the W4 dedup fix
reverted, each at its minimal shape. Soaked at 2 000 programs; PR lane 96;
nightly 5 000 via the existing oracle soak filter. It found one more defect:
in a schemaless graph a FOLD over a target bound by `IS ... TO` was still cast
to Float64 (fixed with the W4 type fix). Out of scope still: recursive FOLD,
whose self-reference contributes the target's folded value, ALONG and BEST BY.

*W4/W5 completion (2026-10-02).* `metamorphic::dqp::locy_levers` runs seven
Locy programs (plain, FOLD, recursion with negation, a variable-length body,
ALONG under a FOLD, ALONG with BEST BY over cycles, a recursive FOLD) through
the fork, pinned and flush levers, every program held to the lever's counter
witness; the pinned lever also writes after its snapshot and checks the pinned
side still gives the pre-write answer while the live side moves. Plan cache
does not apply (Locy has no plan cache) and delete's subset law fails under
`IS NOT`. Locy's counters were already harvested (the "always 0" note was
stale); the pinned witness exposed that `snapshot_reads` was never counted for
an edge or unlabelled-vertex scan, now counted. The oracle evaluates every
rule as a bag and gained per-path ALONG, recursive FOLD over folded values,
BEST BY (ASC over cycles, DESC over the acyclic part) and PROB (MNOR, MPROD,
and the probabilistic `IS NOT` complement), each with hand-computed unit
tests; all fourteen relations derive facts in a large share of cases. It found
three defects:
- an integer ALONG accumulation came back as Float64 (every ALONG column was
  typed Float64); now unified over the rule's clauses, and a dynamic value
  for a schemaless property, which a downstream SUM now reads;
- **silent wrong answer**: a derivation was identified by its clause and its
  own nodes and edges but not by the fact it extended, so two paths sharing
  the first hop and diverging below it with equal values were one fact (a
  downstream SUM read 4 for 7) — #159's class one level down. A hidden
  `__deriv_ref_*` column now hashes the referenced row. Consequence: an ALONG
  rule over a zero-weight cycle reports the iteration limit;
- the pinned-snapshot counter gap above.

The loud defects listed under W6 are closed except one: `count(*)` and every
other aggregate in a `QUERY ... RETURN` (grouped as Cypher's RETURN, checked
against Cypher), `LIMIT $n` there (was silently ignored), `x AND NOT a IS r TO
b` (the condition is the `AND` chain up to the reference; an `OR` before it is
still refused rather than regrouped), `WHERE n:A|B`, Cypher `1 <= {k: 1}`
(NULL, `=` false, `<>` true), and Locy `x / 0.0` / `toInteger('abc')` (checked
expression by expression against Cypher). An inline `WHERE` on a
variable-length relationship stays refused, as Neo4j refuses it.

**W6 — fail loud by default.** Each class member found so far was silent.
Debug-build invariant checks at merge/dedup sites (as `merge_fold_contributions`
now has), and compile-time rejection of programs that would otherwise be
silently degraded (as #293 now does), turn the next member into a failing test
instead of a wrong answer.

*W6 result.* An audit of `unwrap_or(false)` / `unwrap_or(Null)` /
`.ok()` sites on the evaluation path found eight confirmed fail-open sites and
four suspects. All but one are fixed, each with a test that fails with only
its fix reverted, except where noted:
- Locy `QUERY`, `DERIVE`, `EXPLAIN RULE` and `ABDUCE` `WHERE`, and the SLG
  path's target-dependent rule conditions, now share `eval_condition`. An
  evaluation error or a non-boolean was "drop the row". `ABDUCE`'s own two
  sites are reached in tests only through the `EXPLAIN` it runs first, which
  raises the same error.
- `QUERY ... ORDER BY` a non-returned expression sorted on NULL keys: a no-op.
- Locy `<`/`>` between incomparable types was false, not NULL, so under `NOT`
  every such row was kept. `^` of a non-number was `0.0`.
- `DERIVE`'s reported edge properties: an unevaluable map became no
  properties.
- Cypher `_cv_to_bool`: a non-boolean was `false`, so `NOT n.b` kept a row
  with `b = 1`. An encoded NULL was `false` too; no query found reaches that
  arm with an encoded NULL (they arrive as Arrow nulls), so it is unit-tested.
  Fixing it exposed that a bare `WHERE n.b`, and `n.b AND ...`, on a
  CypherValue failed to plan. Both are now read through `_cv_to_bool`.
- A list slice bound that is not an integer was 0 or the length.
- The SLG goal binding compared by `==`, so `QUERY idx WHERE k = 3.0`
  returned nothing. It also injected `k = 3` into a body where `k` is a YIELD
  alias, failing with "Variable 'k' not defined".
- A struct property's blob lookup that failed to plan compiled to a NULL
  literal. This is hardening without a repro.

Debug builds now assert that every rule's facts entering the derived store are
distinct rows (`debug_assert_facts_are_rows`). Its first run fired on an ALONG
rule, and that exposed a W4 regression: an ALONG rule's facts are per path
(#159), but the W4 dedup collapsed a non-recursive ALONG rule's equal-valued
paths, so the same rule's downstream SUM changed when a recursive clause was
added. ALONG rules are now exempt from both. The `locy_query_where` topology
relation applied to about one case in eight, so its activation check
failed at random in the small lane. It now also moves the anchor predicate. Open, and loud rather than
silent: Cypher `1 <= {k: 1}` raises an Arrow "Nested comparison" error where
openCypher gives NULL; Locy raises on `x / 0.0` and `toInteger('abc')` where
Cypher does not; `count(*)` in a `QUERY RETURN`, `x AND NOT a IS r TO b`,
`WHERE m:A|B` and an inline WHERE on a variable-length relationship are
refused. Not done: the fixpoint classifier's provenance fallback, which only
affects provenance.

## 5. What would have caught today's bugs earliest

W2's determinism check (for #294) and W3's fixture with parallel edges and vid
gaps (for #294, the VLP bug and the scan gap) are the highest-yield items: both
reuse existing expectations or existing levers, and neither needs a reference
implementation.
