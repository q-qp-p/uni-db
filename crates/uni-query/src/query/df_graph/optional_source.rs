// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Dragonscale Team

//! Row identity for the rows entering an `OPTIONAL MATCH`.
//!
//! An `OPTIONAL MATCH` must produce, for every row that enters it, either the
//! rows its pattern matched or exactly one row with the pattern's variables
//! NULL. Both operators that decide this — the optional traversal (for rows the
//! pattern did not match) and `OptionalFilterExec` (for rows a `WHERE` then
//! removed every match of) — see only rows the pattern has already fanned out,
//! and they recovered "which entering row was this" from the values of the
//! bound node ids. That merged distinct entering rows that bind the same node:
//! `MATCH (a) UNWIND [1, 2] AS x OPTIONAL MATCH (a)-->(b)` kept one of its two
//! NULL rows, `UNWIND [2, 3] AS x … OPTIONAL MATCH (a)-->(b) WHERE b.v = x`
//! dropped `(3, NULL)` once `x = 2` matched, and identical entering rows (a
//! bag) collapsed to one.
//!
//! [`OptionalSourceRowIdExec`] tags each row with a unique id as it enters the
//! clause; both operators group on the newest such id. The column is named
//! with the [`OPTIONAL_SOURCE_ROW_PREFIX`] prefix and a per-clause ordinal, so
//! nested clauses keep separate ids.

// Rust guideline compliant

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use datafusion::arrow::array::{ArrayRef, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::execution::TaskContext;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, ExecutionPlanProperties, Partitioning,
    PlanProperties, RecordBatchStream, SendableRecordBatchStream,
};
use futures::Stream;

/// Prefix of the row-id columns [`OptionalSourceRowIdExec`] appends.
pub(crate) const OPTIONAL_SOURCE_ROW_PREFIX: &str = "__optional_source_row_";

/// Appends a unique `UInt64` id to every row entering an `OPTIONAL MATCH`.
///
/// Ids are unique across partitions: the partition index occupies the top 16
/// bits and a per-partition counter the rest.
#[derive(Debug)]
pub(crate) struct OptionalSourceRowIdExec {
    input: Arc<dyn ExecutionPlan>,
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
}

impl OptionalSourceRowIdExec {
    /// Wraps `input`, naming the new column after the clauses already tagged.
    pub(crate) fn new(input: Arc<dyn ExecutionPlan>) -> Self {
        let input_schema = input.schema();
        let ordinal = input_schema
            .fields()
            .iter()
            .filter(|f| f.name().starts_with(OPTIONAL_SOURCE_ROW_PREFIX))
            .count();
        let mut fields: Vec<Arc<Field>> = input_schema.fields().iter().cloned().collect();
        fields.push(Arc::new(Field::new(
            format!("{OPTIONAL_SOURCE_ROW_PREFIX}{ordinal}"),
            DataType::UInt64,
            false,
        )));
        let schema: SchemaRef = Arc::new(Schema::new_with_metadata(
            fields,
            input_schema.metadata().clone(),
        ));
        // Keep the child's partition count: reporting fewer partitions than
        // the child produces would make the parent read only some of them.
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(Arc::clone(&schema)),
            Partitioning::UnknownPartitioning(input.output_partitioning().partition_count()),
            input.properties().emission_type,
            input.properties().boundedness,
        ));
        Self {
            input,
            schema,
            properties,
        }
    }
}

/// The newest optional-source row-id column of `schema` that `is_optional`
/// does not claim, if any.
///
/// "Newest" is the highest ordinal: the id of the innermost `OPTIONAL MATCH`
/// whose rows are being grouped. An outer clause's id is also present and also
/// unique per row, but it would merge the rows an inner clause fanned out.
pub(crate) fn newest_source_row_column(
    schema: &Schema,
    is_optional: impl Fn(&str) -> bool,
) -> Option<usize> {
    schema
        .fields()
        .iter()
        .enumerate()
        .filter_map(|(idx, f)| {
            let ordinal: usize = f
                .name()
                .strip_prefix(OPTIONAL_SOURCE_ROW_PREFIX)?
                .parse()
                .ok()?;
            (!is_optional(f.name())).then_some((ordinal, idx))
        })
        .max()
        .map(|(_, idx)| idx)
}

/// The schema of the rows that entered the clause whose row-id column is
/// `column`, row-id column included, found by locating the operator that
/// appended it in `plan`.
///
/// Everything else in a later schema was created inside the clause. The
/// row-id column counts as entering: it is the entering row's identity, and a
/// NULL row built for that entering row must keep it.
pub(crate) fn entering_schema(plan: &Arc<dyn ExecutionPlan>, column: &str) -> Option<SchemaRef> {
    if let Some(tagger) = plan.downcast_ref::<OptionalSourceRowIdExec>()
        && tagger
            .schema
            .fields()
            .last()
            .is_some_and(|f| f.name() == column)
    {
        return Some(Arc::clone(&tagger.schema));
    }
    plan.children()
        .into_iter()
        .find_map(|child| entering_schema(child, column))
}

impl DisplayAs for OptionalSourceRowIdExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "OptionalSourceRowIdExec")
    }
}

impl ExecutionPlan for OptionalSourceRowIdExec {
    fn name(&self) -> &str {
        "OptionalSourceRowIdExec"
    }

    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        let child = children.into_iter().next().ok_or_else(|| {
            DataFusionError::Internal("OptionalSourceRowIdExec needs one child".into())
        })?;
        Ok(Arc::new(Self::new(child)))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> DFResult<SendableRecordBatchStream> {
        Ok(Box::pin(RowIdStream {
            input: self.input.execute(partition, context)?,
            schema: Arc::clone(&self.schema),
            next: (partition as u64) << 48,
        }))
    }
}

struct RowIdStream {
    input: SendableRecordBatchStream,
    schema: SchemaRef,
    next: u64,
}

impl Stream for RowIdStream {
    type Item = DFResult<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.input).poll_next(cx) {
            Poll::Ready(Some(Ok(batch))) => {
                let start = self.next;
                let rows = batch.num_rows() as u64;
                self.next += rows;
                let ids: ArrayRef = Arc::new(UInt64Array::from_iter_values(start..start + rows));
                let mut columns = batch.columns().to_vec();
                columns.push(ids);
                Poll::Ready(Some(
                    RecordBatch::try_new(Arc::clone(&self.schema), columns)
                        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None)),
                ))
            }
            other => other,
        }
    }
}

impl RecordBatchStream for RowIdStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}
