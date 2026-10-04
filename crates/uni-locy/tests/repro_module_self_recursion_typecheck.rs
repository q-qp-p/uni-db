//! Repro for compiler/typecheck.rs:128 — typecheck compares the RAW
//! (unresolved) IS-ref rule name against module-QUALIFIED SCC names. Inside a
//! `MODULE foo`, rule `r` is catalogued/stratified as `foo.r`, but the
//! self-IS test `scc_rules.contains(&is_ref.rule_name.to_string())` tests raw
//! "r" against {"foo.r"} → false → `has_self_is=false` → `check_prev_in_base_case`
//! fires, wrongly rejecting a legal self-recursive `prev` in ALONG.

use uni_cypher::parse_locy;
use uni_locy::compile;

// The exact program body below compiles cleanly WITHOUT a module (this is the
// existing passing `phase_b_f1_suppressed_when_along_present` shape).
const BODY: &str = "CREATE RULE r AS MATCH (a)-[e:E]->(b) ALONG total = e.weight \
     YIELD KEY a, KEY b, total \
     CREATE RULE r AS MATCH (a)-[e:E]->(mid) WHERE mid IS r TO b \
     ALONG total = prev.total + e.weight \
     FOLD total = MSUM(total) YIELD KEY a, KEY b, total";

#[test]
fn module_qualified_self_recursion_wrongly_rejected() {
    // Control: no MODULE → compiles fine.
    let prog_plain = parse_locy(BODY).unwrap();
    assert!(
        compile(&prog_plain).is_ok(),
        "control: self-recursive prev-in-ALONG must compile without a module"
    );

    // Same program wrapped in `MODULE foo`.
    let with_module = format!("MODULE foo\n{BODY}");
    let prog_mod = parse_locy(&with_module).unwrap();
    let result = compile(&prog_mod);

    // FIXED (typecheck.rs): the IS-ref name is module-qualified before the SCC
    // membership test, so `mid IS r` is recognized as a self-recursive ref and
    // `prev.total` in ALONG is legal — the module-wrapped program compiles like
    // the plain control.
    assert!(
        result.is_ok(),
        "module-qualified self-recursion must compile, got {result:?}"
    );
}
