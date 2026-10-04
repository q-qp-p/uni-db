// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! End-to-end test for the `LocyGenerator` plugin surface (deferred P2 item D3):
//! a fixed-arity, non-recursive *generator* predicate that binds new variables
//! (1:N), the Datomic/Souffle-functor use case. Registers a third-party
//! `myplugin.range(n) -> (i)` generator and uses it inside a Locy rule body,
//! proving the emitted tuples explode the rule's rows and bind the output var.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use arrow_array::{Array, ArrayRef, Int64Array};
use datafusion::logical_expr::{ColumnarValue, Volatility};
use uni_db::{DataType, Uni};
use uni_plugin::traits::locy::{GenSignature, GeneratorOutput, LocyGenerator};
use uni_plugin::traits::scalar::ArgType;
use uni_plugin::{
    AbiRange, Capability, CapabilitySet, Determinism, FnError, Plugin, PluginError, PluginManifest,
    PluginRegistrar, ProvidedSurfaces, QName, Scope, SideEffects,
};

/// `myplugin.range(n) -> (i)` — for input `n`, emits one row per `i` in `0..n`.
#[derive(Debug)]
struct RangeGen {
    sig: GenSignature,
}

impl RangeGen {
    fn new() -> Self {
        Self {
            sig: GenSignature {
                args: vec![ArgType::Primitive(arrow_schema::DataType::Int64)],
                outputs: vec![ArgType::Primitive(arrow_schema::DataType::Int64)],
                volatility: Volatility::Immutable,
            },
        }
    }
}

impl LocyGenerator for RangeGen {
    fn signature(&self) -> &GenSignature {
        &self.sig
    }

    fn generate(&self, args: &[ColumnarValue], rows: usize) -> Result<GeneratorOutput, FnError> {
        let ColumnarValue::Array(a) = &args[0] else {
            return Err(FnError::new(0, "range expects an array arg"));
        };
        let n_arr = a
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| FnError::new(0, "range expects an Int64"))?;

        let mut row_map: Vec<u32> = Vec::new();
        let mut vals: Vec<i64> = Vec::new();
        for r in 0..rows {
            if n_arr.is_null(r) {
                continue;
            }
            for i in 0..n_arr.value(r).max(0) {
                row_map.push(r as u32);
                vals.push(i);
            }
        }
        let col: ArrayRef = Arc::new(Int64Array::from(vals));
        Ok(GeneratorOutput {
            row_map,
            columns: vec![col],
        })
    }
}

struct GeneratorPlugin {
    manifest: OnceLock<PluginManifest>,
}

impl Plugin for GeneratorPlugin {
    fn manifest(&self) -> &PluginManifest {
        self.manifest.get_or_init(|| PluginManifest {
            id: uni_plugin::PluginId::new("myplugin"),
            version: "0.1.0".parse().expect("static version"),
            abi: AbiRange::parse("^1").expect("static abi"),
            depends_on: vec![],
            capabilities: CapabilitySet::from_iter_of([Capability::LocyGenerator]),
            determinism: Determinism::Pure,
            side_effects: SideEffects::ReadOnly,
            scope: Scope::Instance,
            hash: None,
            signature: None,
            provides: ProvidedSurfaces::default(),
            docs: "third-party Locy generator predicate (test)".to_owned(),
            metadata: BTreeMap::new(),
        })
    }

    fn register(&self, r: &mut PluginRegistrar<'_>) -> Result<(), PluginError> {
        r.locy_generator(
            QName::new("myplugin", "range"),
            RangeGen::new().sig,
            Arc::new(RangeGen::new()),
        )?;
        Ok(())
    }
}

/// A registered Locy generator explodes a rule's rows and binds its output var.
#[tokio::test]
async fn locy_generator_binds_and_explodes_in_rule_body() -> anyhow::Result<()> {
    let db = Uni::in_memory().build().await?;
    db.add_plugin(GeneratorPlugin {
        manifest: OnceLock::new(),
    })?;

    db.schema()
        .label("Cnt")
        .property("k", DataType::Int64)
        .apply()
        .await?;

    let session = db.session();
    let tx = session.tx().await?;
    // Two source rows: k=3 (emits 0,1,2) and k=2 (emits 0,1).
    tx.execute("CREATE (:Cnt {k: 3})").await?;
    tx.execute("CREATE (:Cnt {k: 2})").await?;
    tx.commit().await?;

    let program = "CREATE RULE idx AS \
         MATCH (n:Cnt) WHERE myplugin.range(n.k) -> (i) YIELD KEY n.k AS k, KEY i AS i\n\
         QUERY idx RETURN k, i";
    let result = session.locy(program).await?;
    let empty = vec![];
    let rows = result.rows().unwrap_or(&empty);

    let mut pairs: Vec<(i64, i64)> = rows
        .iter()
        .filter_map(|r| {
            Some((
                r.get("k").and_then(uni_db::Value::as_i64)?,
                r.get("i").and_then(uni_db::Value::as_i64)?,
            ))
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![(2, 0), (2, 1), (3, 0), (3, 1), (3, 2)],
        "range(k) should emit i in 0..k, exploding each source row"
    );
    Ok(())
}

/// A `QUERY ... WHERE key = literal` over a generator rule (resolved by the SLG
/// path) keeps the rows Locy's `=` keeps. The goal binding compared the key by
/// structural `==`, so `k = 3.0` against an integer `k` matched nothing and the
/// query silently returned no rows (W6).
#[tokio::test]
async fn locy_generator_goal_binding_uses_value_equality() -> anyhow::Result<()> {
    let db = Uni::in_memory().build().await?;
    db.add_plugin(GeneratorPlugin {
        manifest: OnceLock::new(),
    })?;
    db.schema()
        .label("Cnt")
        .property("k", DataType::Int64)
        .apply()
        .await?;
    let session = db.session();
    let tx = session.tx().await?;
    tx.execute("CREATE (:Cnt {k: 3}), (:Cnt {k: 2})").await?;
    tx.commit().await?;

    for filter in ["k = 3", "k = 3.0", "3.0 = k"] {
        let program = format!(
            "CREATE RULE idx AS \
             MATCH (n:Cnt) WHERE myplugin.range(n.k) -> (i) YIELD KEY n.k AS k, KEY i AS i\n\
             QUERY idx WHERE {filter} RETURN k, i"
        );
        let result = session.locy(&program).await?;
        let empty = vec![];
        let mut is: Vec<i64> = result
            .rows()
            .unwrap_or(&empty)
            .iter()
            .filter_map(|r| r.get("i").and_then(uni_db::Value::as_i64))
            .collect();
        is.sort();
        assert_eq!(is, vec![0, 1, 2], "{filter}");
    }
    Ok(())
}
