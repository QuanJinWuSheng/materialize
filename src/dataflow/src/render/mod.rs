// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Renders a plan into a timely/differential dataflow computation.
//!
//! ## Error handling
//!
//! Timely and differential have no idioms for computations that can error. The
//! philosophy is, reasonably, to define the semantics of the computation such
//! that errors are unnecessary: e.g., by using wrap-around semantics for
//! integer overflow.
//!
//! Unfortunately, SQL semantics are not nearly so elegant, and require errors
//! in myriad cases. The classic example is a division by zero, but invalid
//! input for casts, overflowing integer operations, and dozens of other
//! functions need the ability to produce errors ar runtime.
//!
//! At the moment, only *scalar* expression evaluation can fail, so only
//! operators that evaluate scalar expressions can fail. At the time of writing,
//! that includes map, filter, reduce, and join operators.
//!
//! The approach taken is to build two parallel trees of computation: one for
//! the rows that have been successfully evaluated (the "oks tree"), and one for
//! the errors that have been generated (the "errs tree"). For example:
//!
//! ```text
//!    oks1  errs1       oks2  errs2
//!      |     |           |     |
//!      |     |           |     |
//!   project  |           |     |
//!      |     |           |     |
//!      |     |           |     |
//!     map    |           |     |
//!      |\    |           |     |
//!      | \   |           |     |
//!      |  \  |           |     |
//!      |   \ |           |     |
//!      |    \|           |     |
//!   project  +           +     +
//!      |     |          /     /
//!      |     |         /     /
//!    join ------------+     /
//!      |     |             /
//!      |     | +----------+
//!      |     |/
//!     oks   errs
//! ```
//!
//! The project operation cannot fail, so errors from errs1 are propagated
//! directly. Map operators are fallible and so can inject additional errors
//! into the stream. Join operators combine the errors from each of their
//! inputs.
//!
//! The semantics of the error stream are minimal. From the perspective of SQL,
//! a dataflow is considered to be in an error state if there is at least one
//! element in the final errs collection. The error value returned to the user
//! is selected arbitrarily; SQL only makes provisions to return one error to
//! the user at a time. There are plans to make the err collection accessible to
//! end users, so they can see all errors at once.
//!
//! To make errors transient, simply ensure that the operator can retract any
//! produced errors when corrected data arrives. To make errors permanent, write
//! the operator such that it never retracts the errors it produced. Future work
//! will likely want to introduce some sort of sort order for errors, so that
//! permanent errors are returned to the user ahead of transient errors—probably
//! by introducing a new error type a la:
//!
//! ```no_run
//! # struct EvalError;
//! # struct SourceError;
//! enum DataflowError {
//!     Transient(EvalError),
//!     Permanent(SourceError),
//! }
//! ```
//!
//! If the error stream is empty, the oks stream must be correct. If the error
//! stream is non-empty, then there are no semantics for the oks stream. This is
//! sufficient to support SQL in its current form, but is likely to be
//! unsatisfactory long term. We suspect that we can continue to imbue the oks
//! stream with semantics if we are very careful in describing what data should
//! and should not be produced upon encountering an error. Roughly speaking, the
//! oks stream could represent the correct result of the computation where all
//! rows that caused an error have been pruned from the stream. There are
//! strange and confusing questions here around foreign keys, though: what if
//! the optimizer proves that a particular key must exist in a collection, but
//! the key gets pruned away because its row participated in a scalar expression
//! evaluation that errored?
//!
//! In the meantime, it is probably wise for operators to keep the oks stream
//! roughly "as correct as possible" even when errors are present in the errs
//! stream. This reduces the amount of recomputation that must be performed
//! if/when the errors are retracted.

use std::collections::{HashMap, HashSet};
use std::iter;
use std::rc::Rc;
use std::rc::Weak;
use std::{any::Any, cell::RefCell};

use differential_dataflow::hashable::Hashable;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::arrange::arrangement::{Arrange, ArrangeByKey};
use differential_dataflow::operators::arrange::upsert::arrange_from_upsert;
use differential_dataflow::operators::Consolidate;
use differential_dataflow::{AsCollection, Collection};
use futures::executor::block_on;
use timely::communication::Allocate;
use timely::dataflow::operators::exchange::Exchange;
use timely::dataflow::operators::to_stream::ToStream;
use timely::dataflow::operators::unordered_input::UnorderedInput;
use timely::dataflow::operators::Map;
use timely::dataflow::scopes::Child;
use timely::dataflow::Scope;
use timely::worker::Worker as TimelyWorker;

use dataflow_types::*;
use expr::{GlobalId, Id, MapFilterProject, RelationExpr, ScalarExpr, SourceInstanceId};
use interchange::envelopes::{combine_at_timestamp, dbz_format, upsert_format};
use mz_avro::types::Value;
use mz_avro::Schema;
use ore::cast::CastFrom;
use ore::collections::CollectionExt as _;
use ore::iter::IteratorExt;
use repr::adt::decimal::Significand;
use repr::{Datum, RelationType, Row, RowArena, RowPacker, Timestamp};

use crate::decode::{decode_avro_values, decode_values};
use crate::operator::{CollectionExt, StreamExt};
use crate::render::context::{ArrangementFlavor, Context};
use crate::server::{CacheMessage, LocalInput, TimestampDataUpdates, TimestampMetadataUpdates};
use crate::sink;
use crate::source::{self, FileSourceInfo, KafkaSourceInfo, KinesisSourceInfo, S3SourceInfo};
use crate::source::{SourceConfig, SourceToken};
use crate::{
    arrangement::manager::{TraceBundle, TraceManager},
    logging::materialized::Logger,
};

mod arrange_by;
mod context;
mod flat_map;
mod join;
mod reduce;
mod threshold;
mod top_k;
mod upsert;

/// Worker-local state used during rendering.
pub struct RenderState {
    /// The traces available for sharing across dataflows.
    pub traces: TraceManager,
    /// Handles to local inputs, keyed by ID.
    pub local_inputs: HashMap<GlobalId, LocalInput>,
    /// Handles to external sources, keyed by ID.
    pub ts_source_mapping: HashMap<SourceInstanceId, Weak<Option<SourceToken>>>,
    /// Timestamp data updates for each source.
    pub ts_histories: TimestampDataUpdates,
    /// Communication channel for enabling/disabling timestamping on new/dropped
    /// sources.
    pub ts_source_updates: TimestampMetadataUpdates,
    /// Tokens that should be dropped when a dataflow is dropped to clean up
    /// associated state.
    pub dataflow_tokens: HashMap<GlobalId, Box<dyn Any>>,
    /// Sender to give data to be cahed.
    pub caching_tx: Option<comm::mpsc::Sender<CacheMessage>>,
}

pub fn build_dataflow<A: Allocate>(
    timely_worker: &mut TimelyWorker<A>,
    render_state: &mut RenderState,
    dataflow: DataflowDesc,
) {
    let worker_logging = timely_worker.log_register().get("timely");
    let name = format!("Dataflow: {}", &dataflow.debug_name);
    let materialized_logging = timely_worker.log_register().get("materialized");

    timely_worker.dataflow_core(&name, worker_logging, Box::new(()), |_, scope| {
        // The scope.clone() occurs to allow import in the region.
        // We build a region here to establish a pattern of a scope inside the dataflow,
        // so that other similar uses (e.g. with iterative scopes) do not require weird
        // alternate type signatures.
        scope.clone().region(|region| {
            let mut context = Context::for_dataflow(&dataflow, scope.addr().into_element());

            assert!(
                !dataflow
                    .source_imports
                    .iter()
                    .map(|(id, _src)| id)
                    .has_duplicates(),
                "computation of unique IDs assumes a source appears no more than once per dataflow"
            );

            // Import declared sources into the rendering context.
            for (src_id, src) in dataflow.source_imports.clone() {
                context.import_source(
                    render_state,
                    region,
                    materialized_logging.clone(),
                    src_id,
                    src,
                );
            }

            // Import declared indexes into the rendering context.
            for (idx_id, idx) in &dataflow.index_imports {
                context.import_index(render_state, scope, region, *idx_id, idx);
            }

            // Build declared objects.
            for object in &dataflow.objects_to_build {
                context.build_object(region, object);
            }

            // Export declared indexes.
            for (idx_id, idx, typ) in &dataflow.index_exports {
                let imports = dataflow.get_imports(&idx.on_id);
                context.export_index(render_state, imports, *idx_id, idx, typ);
            }

            // Export declared sinks.
            for (sink_id, sink) in &dataflow.sink_exports {
                let imports = dataflow.get_imports(&sink.from);
                context.export_sink(render_state, imports, *sink_id, sink);
            }
        });
    })
}

impl<'g, G> Context<Child<'g, G, G::Timestamp>, RelationExpr, Row, Timestamp>
where
    G: Scope<Timestamp = Timestamp>,
{
    fn import_source(
        &mut self,
        render_state: &mut RenderState,
        scope: &mut Child<'g, G, G::Timestamp>,
        materialized_logging: Option<Logger>,
        src_id: GlobalId,
        mut src: SourceDesc,
    ) {
        if let Some(operator) = &src.operators {
            if operator.is_trivial(src.arity()) {
                src.operators = None;
            }
        }

        match src.connector {
            SourceConnector::Local => {
                let ((handle, capability), stream) = scope.new_unordered_input();
                render_state
                    .local_inputs
                    .insert(src_id, LocalInput { handle, capability });
                let err_collection = Collection::empty(scope);
                self.collections.insert(
                    RelationExpr::global_get(src_id, src.desc.typ().clone()),
                    (stream.as_collection(), err_collection),
                );
            }

            SourceConnector::External {
                connector,
                encoding,
                envelope,
                consistency,
                ts_frequency,
            } => {
                // TODO(benesch): this match arm is hard to follow. Refactor.

                let get_expr = RelationExpr::global_get(src_id, src.desc.typ().clone());

                // This uid must be unique across all different instantiations of a source
                let uid = SourceInstanceId {
                    source_id: src_id,
                    dataflow_id: self.dataflow_id,
                };

                // TODO(benesch): we force all sources to have an empty
                // error stream. Likely we will want to plumb this
                // collection into the source connector so that sources
                // can produce errors.
                let mut err_collection = Collection::empty(scope);

                let fast_forwarded = match &connector {
                    ExternalSourceConnector::Kafka(KafkaSourceConnector {
                        start_offsets, ..
                    }) => start_offsets.values().any(|&val| val > 0),
                    _ => false,
                };

                // All workers are responsible for reading in Kafka sources. Other sources
                // support single-threaded ingestion only
                let active_read_worker = if let ExternalSourceConnector::Kafka(_) = connector {
                    true
                } else {
                    (usize::cast_from(uid.hashed()) % scope.peers()) == scope.index()
                };

                let caching_tx = if let (true, Some(caching_tx)) =
                    (connector.caching_enabled(), render_state.caching_tx.clone())
                {
                    Some(block_on(caching_tx.connect()).expect("failed to connect caching tx"))
                } else {
                    None
                };

                let source_config = SourceConfig {
                    name: format!("{}-{}", connector.name(), uid),
                    id: uid,
                    scope,
                    // Distribute read responsibility among workers.
                    active: active_read_worker,
                    timestamp_histories: render_state.ts_histories.clone(),
                    timestamp_tx: render_state.ts_source_updates.clone(),
                    consistency,
                    timestamp_frequency: ts_frequency,
                    worker_id: scope.index(),
                    worker_count: scope.peers(),
                    logger: materialized_logging,
                    encoding: encoding.clone(),
                    caching_tx,
                };

                let capability = if let SourceEnvelope::Upsert(key_encoding) = envelope {
                    match connector {
                        ExternalSourceConnector::Kafka(_) => {
                            let (source, capability) = source::create_source::<_, KafkaSourceInfo, _>(
                                source_config,
                                connector,
                            );

                            let (transformed, new_err_collection) =
                                upsert::pre_arrange_from_upsert_transforms(
                                    &source.0,
                                    encoding,
                                    key_encoding,
                                    &self.debug_name,
                                    scope.index(),
                                    self.as_of_frontier.clone(),
                                    &mut src.operators,
                                    src.desc.typ(),
                                );

                            let arranged = arrange_from_upsert(
                                &transformed,
                                &format!("UpsertArrange: {}", src_id.to_string()),
                            );

                            err_collection.concat(
                                &new_err_collection
                                    .pass_through("upsert-linear-errors")
                                    .as_collection(),
                            );

                            let keys = src.desc.typ().keys[0]
                                .iter()
                                .map(|k| ScalarExpr::Column(*k))
                                .collect::<Vec<_>>();
                            self.set_local(&get_expr, &keys, (arranged, err_collection.arrange()));
                            capability
                        }
                        _ => unreachable!("Upsert envelope unsupported for non-Kafka sources"),
                    }
                } else {
                    let (stream, capability) = if let ExternalSourceConnector::AvroOcf(_) =
                        connector
                    {
                        let ((source, err_source), capability) =
                            source::create_source::<_, FileSourceInfo<Value>, Value>(
                                source_config,
                                connector,
                            );
                        err_collection = err_collection.concat(
                            &err_source
                                .map(DataflowError::SourceError)
                                .pass_through("AvroOCF-errors")
                                .as_collection(),
                        );
                        let reader_schema = match &encoding {
                            DataEncoding::AvroOcf(AvroOcfEncoding { reader_schema }) => {
                                reader_schema
                            }
                            _ => unreachable!(
                                "Internal error: \
                                     Avro OCF schema should have already been resolved.\n\
                                    Encoding is: {:?}",
                                encoding
                            ),
                        };

                        let reader_schema = Schema::parse_str(reader_schema).unwrap();
                        (
                            decode_avro_values(&source, &envelope, reader_schema, &self.debug_name),
                            capability,
                        )
                    } else {
                        let ((ok_source, err_source), capability) = match connector {
                            ExternalSourceConnector::Kafka(_) => {
                                source::create_source::<_, KafkaSourceInfo, _>(
                                    source_config,
                                    connector,
                                )
                            }
                            ExternalSourceConnector::Kinesis(_) => {
                                source::create_source::<_, KinesisSourceInfo, _>(
                                    source_config,
                                    connector,
                                )
                            }
                            ExternalSourceConnector::S3(_) => {
                                source::create_source::<_, S3SourceInfo, _>(
                                    source_config,
                                    connector,
                                )
                            }
                            ExternalSourceConnector::File(_) => {
                                source::create_source::<_, FileSourceInfo<Vec<u8>>, Vec<u8>>(
                                    source_config,
                                    connector,
                                )
                            }
                            ExternalSourceConnector::AvroOcf(_) => unreachable!(),
                        };
                        err_collection = err_collection.concat(
                            &err_source
                                .map(DataflowError::SourceError)
                                .pass_through("source-errors")
                                .as_collection(),
                        );

                        // TODO(brennan) -- this should just be a RelationExpr::FlatMap using regexp_extract, csv_extract,
                        // a hypothetical future avro_extract, protobuf_extract, etc.
                        let (stream, extra_token) = decode_values(
                            &ok_source,
                            encoding,
                            &self.debug_name,
                            &envelope,
                            &mut src.operators,
                            fast_forwarded,
                        );
                        if let Some(tok) = extra_token {
                            self.additional_tokens
                                .entry(src_id)
                                .or_insert_with(Vec::new)
                                .push(Rc::new(tok));
                        }

                        (stream, capability)
                    };

                    let mut collection = match envelope {
                        SourceEnvelope::None | SourceEnvelope::CdcV2 => stream.as_collection(),
                        SourceEnvelope::Debezium(_) =>
                        // TODO(btv) -- this should just be a RelationExpr::Explode (name TBD)
                        {
                            stream.as_collection().explode({
                                let mut row_packer = repr::RowPacker::new();
                                move |row| {
                                    let mut datums = row.unpack();
                                    let diff = datums.pop().unwrap().unwrap_int64() as isize;
                                    Some((row_packer.pack(datums.into_iter()), diff))
                                }
                            })
                        }
                        SourceEnvelope::Upsert(_) => unreachable!(),
                    };

                    // Implement source filtering and projection.
                    // At the moment this is strictly optional, but we perform it anyhow
                    // to demonstrate the intended use.
                    if let Some(operators) = src.operators.clone() {
                        // Determine replacement values for unused columns.
                        let source_type = src.desc.typ();
                        let position_or = (0..source_type.arity())
                            .map(|col| {
                                if operators.projection.contains(&col) {
                                    Some(col)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>();

                        // Evaluate the predicate on each record, noting potential errors that might result.
                        let (collection2, errors) = collection.flat_map_fallible({
                            let mut row_packer = repr::RowPacker::new();
                            move |input_row| {
                                let temp_storage = RowArena::new();
                                let datums = input_row.unpack();
                                let pred_eval = operators
                                    .predicates
                                    .iter()
                                    .map(|predicate| predicate.eval(&datums, &temp_storage))
                                    .find(|result| result != &Ok(Datum::True));
                                match pred_eval {
                                    None => Some(Ok(row_packer.pack(position_or.iter().map(
                                        |pos_or| match pos_or {
                                            Some(index) => datums[*index],
                                            None => Datum::Dummy,
                                        },
                                    )))),
                                    Some(Ok(Datum::False)) => None,
                                    Some(Ok(Datum::Null)) => None,
                                    Some(Ok(x)) => {
                                        panic!("Predicate evaluated to invalid value: {:?}", x)
                                    }
                                    Some(Err(x)) => Some(Err(x.into())),
                                }
                            }
                        });

                        collection = collection2;
                        err_collection = err_collection.concat(&errors);
                    }

                    // Apply `as_of` to each timestamp.
                    let as_of_frontier1 = self.as_of_frontier.clone();
                    collection = collection
                        .inner
                        .map_in_place(move |(_, time, _)| time.advance_by(as_of_frontier1.borrow()))
                        .as_collection();

                    let as_of_frontier2 = self.as_of_frontier.clone();
                    err_collection = err_collection
                        .inner
                        .map_in_place(move |(_, time, _)| time.advance_by(as_of_frontier2.borrow()))
                        .as_collection();

                    // Introduce the stream by name, as an unarranged collection.
                    self.collections.insert(
                        RelationExpr::global_get(src_id, src.desc.typ().clone()),
                        (collection, err_collection),
                    );
                    capability
                };
                let token = Rc::new(capability);
                self.source_tokens.insert(src_id, token.clone());

                // We also need to keep track of this mapping globally to activate sources
                // on timestamp advancement queries
                let prev = render_state
                    .ts_source_mapping
                    .insert(uid, Rc::downgrade(&token));
                assert!(prev.is_none());
            }
        }
    }

    fn import_index(
        &mut self,
        render_state: &mut RenderState,
        scope: &mut G,
        region: &mut Child<'g, G, G::Timestamp>,
        idx_id: GlobalId,
        (idx, typ): &(IndexDesc, RelationType),
    ) {
        if let Some(traces) = render_state.traces.get_mut(&idx_id) {
            let token = traces.to_drop().clone();
            let (ok_arranged, ok_button) = traces.oks_mut().import_frontier_core(
                scope,
                &format!("Index({}, {:?})", idx.on_id, idx.keys),
                self.as_of_frontier.clone(),
            );
            let (err_arranged, err_button) = traces.errs_mut().import_frontier_core(
                scope,
                &format!("ErrIndex({}, {:?})", idx.on_id, idx.keys),
                self.as_of_frontier.clone(),
            );
            let ok_arranged = ok_arranged.enter(region);
            let err_arranged = err_arranged.enter(region);
            let get_expr = RelationExpr::global_get(idx.on_id, typ.clone());
            self.set_trace(idx_id, &get_expr, &idx.keys, (ok_arranged, err_arranged));
            self.additional_tokens
                .entry(idx_id)
                .or_insert_with(Vec::new)
                .push(Rc::new((
                    ok_button.press_on_drop(),
                    err_button.press_on_drop(),
                    token,
                )));
        } else {
            panic!(
                "import of index {} failed while building dataflow {}",
                idx_id, self.dataflow_id
            );
        }
    }

    fn build_object(&mut self, scope: &mut Child<'g, G, G::Timestamp>, object: &BuildDesc) {
        self.ensure_rendered(object.relation_expr.as_ref(), scope, scope.index());
        if let Some(typ) = &object.typ {
            self.clone_from_to(
                &object.relation_expr.as_ref(),
                &RelationExpr::global_get(object.id, typ.clone()),
            );
        } else {
            self.render_arrangeby(&object.relation_expr.as_ref(), Some(&object.id.to_string()));
            // Under the premise that this is always an arrange_by aroung a global get,
            // this will leave behind the arrangements bound to the global get, so that
            // we will not tidy them up in the next pass.
        }

        // After building each object, we want to tear down all other cached collections
        // and arrangements to avoid accidentally providing hits on local identifiers.
        // We could relax this if we better understood which expressions are dangerous
        // (e.g. expressions containing gets of local identifiers not covered by a let).
        //
        // TODO: Improve collection and arrangement re-use.
        self.collections.retain(|e, _| {
            matches!(e, RelationExpr::Get {
                id: Id::Global(_),
                typ: _,
            })
        });
        self.local.retain(|e, _| {
            matches!(e, RelationExpr::Get {
                id: Id::Global(_),
                typ: _,
            })
        });
        // We do not install in `context.trace`, and can skip deleting things from it.
    }

    fn export_index(
        &mut self,
        render_state: &mut RenderState,
        import_ids: HashSet<GlobalId>,
        idx_id: GlobalId,
        idx: &IndexDesc,
        typ: &RelationType,
    ) {
        // put together tokens that belong to the export
        let mut needed_source_tokens = Vec::new();
        let mut needed_additional_tokens = Vec::new();
        for import_id in import_ids {
            if let Some(addls) = self.additional_tokens.get(&import_id) {
                needed_additional_tokens.extend_from_slice(addls);
            }
            if let Some(source_token) = self.source_tokens.get(&import_id) {
                needed_source_tokens.push(source_token.clone());
            }
        }
        let tokens = Rc::new((needed_source_tokens, needed_additional_tokens));
        let get_expr = RelationExpr::global_get(idx.on_id, typ.clone());
        match self.arrangement(&get_expr, &idx.keys) {
            Some(ArrangementFlavor::Local(oks, errs)) => {
                render_state.traces.set(
                    idx_id,
                    TraceBundle::new(oks.trace, errs.trace).with_drop(tokens),
                );
            }
            Some(ArrangementFlavor::Trace(gid, _, _)) => {
                // Duplicate of existing arrangement with id `gid`, so
                // just create another handle to that arrangement.
                let trace = render_state.traces.get(&gid).unwrap().clone();
                render_state.traces.set(idx_id, trace);
            }
            None => {
                panic!("Arrangement alarmingly absent!");
            }
        };
    }

    fn export_sink(
        &mut self,
        render_state: &mut RenderState,
        import_ids: HashSet<GlobalId>,
        sink_id: GlobalId,
        sink: &SinkDesc,
    ) {
        // put together tokens that belong to the export
        let mut needed_source_tokens = Vec::new();
        let mut needed_additional_tokens = Vec::new();
        let mut needed_sink_tokens = Vec::new();
        for import_id in import_ids {
            if let Some(addls) = self.additional_tokens.get(&import_id) {
                needed_additional_tokens.extend_from_slice(addls);
            }
            if let Some(source_token) = self.source_tokens.get(&import_id) {
                needed_source_tokens.push(source_token.clone());
            }
        }
        let (collection, _err_collection) = self
            .collection(&RelationExpr::global_get(
                sink.from,
                sink.from_desc.typ().clone(),
            ))
            .expect("Sink source collection not loaded");

        // Some connectors support keys - extract them.
        let key_indices = sink
            .connector
            .get_key_indices()
            .map(|key_indices| key_indices.to_vec());
        let keyed = collection.map(move |row| {
            let key = key_indices.as_ref().map(|key_indices| {
                // TODO[perf] (btv) - is there a way to avoid unpacking and repacking every row and cloning the datums?
                // Does it matter?
                let datums = row.unpack();
                Row::pack(key_indices.iter().map(|&idx| datums[idx].clone()))
            });
            (key, row)
        });

        // Each partition needs to be handled by its own worker, so that we can write messages in order.
        // For now, we only support single-partition sinks.
        let keyed = keyed
            .inner
            .exchange(move |_| sink_id.hashed())
            .as_collection();

        // Apply the envelope.
        // * "Debezium" consolidates the stream, sorts it by time, and produces DiffPairs from it.
        //   It then renders those as Avro.
        // * Upsert" does the same, except at the last step, it renders the diff pair in upsert format.
        //   (As part of doing so, it asserts that there are not multiple conflicting values at the same timestamp)
        // * "Tail" writes some metadata.
        let collection = match sink.envelope {
            SinkEnvelope::Debezium => {
                let combined = combine_at_timestamp(keyed.arrange_by_key().stream);
                // This has to be an `Rc<RefCell<...>>` because the inner closure (passed to `Iterator::map`) references it, and it might outlive the outer closure.
                let rp = Rc::new(RefCell::new(RowPacker::new()));
                let collection = combined.flat_map(move |(mut k, v)| {
                    let max_idx = v.len() - 1;
                    let rp = rp.clone();
                    v.into_iter().enumerate().map(move |(idx, dp)| {
                        let k = if idx == max_idx { k.take() } else { k.clone() };
                        (k, Some(dbz_format(&mut *rp.borrow_mut(), dp)))
                    })
                });
                collection
            }
            SinkEnvelope::Upsert => {
                let combined = combine_at_timestamp(keyed.arrange_by_key().stream);

                let collection = combined.map(|(k, v)| {
                    let v = upsert_format(v);
                    (k, v)
                });
                collection
            }
            SinkEnvelope::Tail { emit_progress } => keyed
                .consolidate()
                .inner
                .map({
                    let mut rp = RowPacker::new();
                    move |((k, v), time, diff)| {
                        rp.push(Datum::Decimal(Significand::new(i128::from(time))));
                        if emit_progress {
                            rp.push(Datum::False);
                        }
                        rp.push(Datum::Int64(i64::cast_from(diff)));
                        rp.extend_by_row(&v);
                        let v = rp.finish_and_reuse();
                        ((k, Some(v)), time, 1)
                    }
                })
                .as_collection(),
        };

        // Some sinks require that the timestamp be appended to the end of the value.
        let append_timestamp = match &sink.connector {
            SinkConnector::Kafka(c) => c.consistency.is_some(),
            SinkConnector::Tail(_) => false,
            SinkConnector::AvroOcf(_) => false,
        };
        let collection = if append_timestamp {
            collection
                .inner
                .map(|((k, v), t, diff)| {
                    let v = v.map(|v| {
                        let mut rp = RowPacker::new();
                        rp.extend_by_row(&v);
                        let t = t.to_string();
                        rp.push_list_with(|rp| {
                            rp.push(Datum::String(&t));
                        });
                        rp.finish()
                    });
                    ((k, v), t, diff)
                })
                .as_collection()
        } else {
            collection
        };

        // TODO(benesch): errors should stream out through the sink,
        // if we figure out a protocol for that.

        match sink.connector.clone() {
            SinkConnector::Kafka(c) => {
                let token = sink::kafka(
                    collection,
                    sink_id,
                    c,
                    sink.key_desc.clone(),
                    sink.value_desc.clone(),
                );
                needed_sink_tokens.push(token);
            }
            SinkConnector::Tail(c) => {
                let batches = collection
                    .map(move |(k, v)| {
                        assert!(k.is_none(), "tail does not support keys");
                        let v = v.expect("tail must have values");
                        (sink_id, v)
                    })
                    .arrange_by_key()
                    .stream;
                sink::tail(batches, sink_id, c);
            }
            SinkConnector::AvroOcf(c) => {
                sink::avro_ocf(collection, sink_id, c, sink.value_desc.clone());
            }
        };

        let tokens = Rc::new((
            needed_sink_tokens,
            needed_source_tokens,
            needed_additional_tokens,
        ));
        render_state
            .dataflow_tokens
            .insert(sink_id, Box::new(tokens));
    }
}

impl<G> Context<G, RelationExpr, Row, Timestamp>
where
    G: Scope<Timestamp = Timestamp>,
{
    /// Attempt to render a chain of map/filter/project operators on top of another operator.
    ///
    /// Returns true if it was successful, and false otherwise. If this method returns false,
    /// we should continue with the traditional individual render implementations of each
    /// operator.
    fn try_render_map_filter_project(
        &mut self,
        relation_expr: &RelationExpr,
        scope: &mut G,
        worker_index: usize,
    ) -> bool {
        // Extract a MapFilterProject and residual from `relation_expr`.
        let (mfp, input) = MapFilterProject::extract_from_expression(relation_expr);
        match input {
            RelationExpr::Get { .. } => {
                // TODO: determine if `mfp` is no-op to simplify implementation.
                let mfp2 = mfp.clone();
                self.ensure_rendered(&input, scope, worker_index);
                let (ok_collection, mut err_collection) = self
                    .flat_map_ref(&input, move |exprs| mfp2.literal_constraints(exprs), {
                        let mut row_packer = repr::RowPacker::new();
                        let mut datums = vec![];
                        move |row| {
                            let pack_result = {
                                let temp_storage = RowArena::new();
                                let mut datums_local = std::mem::take(&mut datums);
                                datums_local.extend(row.iter());
                                // Temporary assignment looks weird, but seems needed to convince
                                // Rust that the lifetime of `evaluate_iter` does not escape.
                                let result =
                                    match mfp.evaluate_iter(&mut datums_local, &temp_storage) {
                                        Ok(Some(iter)) => {
                                            row_packer.clear();
                                            row_packer.extend(iter);
                                            Some(Ok(()))
                                        }
                                        Ok(None) => None,
                                        Err(e) => Some(Err(e.into())),
                                    };
                                datums = ore::vec::repurpose_allocation(datums_local);
                                result
                            };

                            // Re-use the input `row` if at all possible.
                            pack_result.map(|res| {
                                res.map(|()| match row {
                                    timely::communication::message::RefOrMut::Ref(_) => {
                                        row_packer.finish_and_reuse()
                                    }
                                    timely::communication::message::RefOrMut::Mut(r) => {
                                        row_packer.finish_into(r);
                                        std::mem::take(r)
                                    }
                                })
                            })
                        }
                    })
                    .unwrap();

                use timely::dataflow::operators::ok_err::OkErr;
                let (oks, errors) = ok_collection.inner.ok_err(|(x, t, d)| match x {
                    Ok(x) => Ok((x, t, d)),
                    Err(x) => Err((x, t, d)),
                });
                err_collection = err_collection.concat(&errors.as_collection());

                self.collections
                    .insert(relation_expr.clone(), (oks.as_collection(), err_collection));
                true
            }
            RelationExpr::FlatMap { input: input2, .. } => {
                self.ensure_rendered(&input2, scope, worker_index);
                let (oks, err) = self.render_flat_map(input, Some(mfp));
                self.collections.insert(relation_expr.clone(), (oks, err));
                true
            }

            RelationExpr::Join {
                inputs,
                implementation,
                ..
            } => {
                for input in inputs {
                    self.ensure_rendered(input, scope, worker_index);
                }
                match implementation {
                    expr::JoinImplementation::Differential(_start, _order) => {
                        let collection = self.render_join(input, mfp, scope);
                        self.collections.insert(relation_expr.clone(), collection);
                    }
                    expr::JoinImplementation::DeltaQuery(_orders) => {
                        let collection =
                            self.render_delta_join(input, mfp, scope, worker_index, |t| {
                                t.saturating_sub(1)
                            });
                        self.collections.insert(relation_expr.clone(), collection);
                    }
                    expr::JoinImplementation::Unimplemented => {
                        panic!("Attempt to render unimplemented join");
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Ensures the context contains an entry for `relation_expr`.
    ///
    /// This method may construct new dataflow elements and register then in the context,
    /// and is only obliged to ensure that a call to `self.collection(relation_expr)` will
    /// result in a non-`None` result. This may be a raw collection or an arrangement by
    /// any set of keys.
    ///
    /// The rough structure of the logic for each expression is to ensure that any input
    /// collections are rendered,
    pub fn ensure_rendered(
        &mut self,
        relation_expr: &RelationExpr,
        scope: &mut G,
        worker_index: usize,
    ) {
        if !self.has_collection(relation_expr) {
            // Each of the `RelationExpr` variants have logic to render themselves to either
            // a collection or an arrangement. In either case, we associate the result with
            // the `relation_expr` argument in the context.
            match relation_expr {
                // The constant collection is instantiated only on worker zero.
                RelationExpr::Constant { rows, .. } => {
                    let rows = if worker_index == 0 {
                        rows.clone()
                    } else {
                        vec![]
                    };

                    let collection = rows
                        .to_stream(scope)
                        .map(|(x, diff)| (x, timely::progress::Timestamp::minimum(), diff))
                        .as_collection();

                    let err_collection = Collection::empty(scope);

                    self.collections
                        .insert(relation_expr.clone(), (collection, err_collection));
                }

                // A get should have been loaded into the context, and it is surprising to
                // reach this point given the `has_collection()` guard at the top of the method.
                RelationExpr::Get { id, typ: _ } => {
                    // TODO: something more tasteful.
                    // perhaps load an empty collection, warn?
                    panic!("Collection {} not pre-loaded", id);
                }

                RelationExpr::Let { id, value, body } => {
                    let typ = value.typ();
                    let bind = RelationExpr::Get {
                        id: Id::Local(*id),
                        typ,
                    };
                    if self.has_collection(&bind) {
                        panic!("Inappropriate to re-bind name: {:?}", bind);
                    } else {
                        self.ensure_rendered(value, scope, worker_index);
                        self.clone_from_to(value, &bind);
                        self.ensure_rendered(body, scope, worker_index);
                        self.clone_from_to(body, relation_expr);
                    }
                }

                RelationExpr::Project { input, outputs } => {
                    if !self.try_render_map_filter_project(relation_expr, scope, worker_index) {
                        self.ensure_rendered(input, scope, worker_index);
                        let outputs = outputs.clone();
                        let (ok_collection, err_collection) = self.collection(input).unwrap();
                        let ok_collection = ok_collection.map({
                            let mut row_packer = repr::RowPacker::new();
                            move |row| {
                                let datums = row.unpack();
                                row_packer.pack(outputs.iter().map(|i| datums[*i]))
                            }
                        });

                        self.collections
                            .insert(relation_expr.clone(), (ok_collection, err_collection));
                    }
                }

                RelationExpr::Map { input, scalars } => {
                    if !self.try_render_map_filter_project(relation_expr, scope, worker_index) {
                        self.ensure_rendered(input, scope, worker_index);
                        let scalars = scalars.clone();
                        let (ok_collection, err_collection) = self.collection(input).unwrap();
                        let (ok_collection, new_err_collection) = ok_collection.map_fallible({
                            let mut row_packer = repr::RowPacker::new();
                            move |input_row| {
                                let mut datums = input_row.unpack();
                                let temp_storage = RowArena::new();
                                for scalar in &scalars {
                                    let datum = scalar.eval(&datums, &temp_storage)?;
                                    // Scalar is allowed to see the outputs of previous scalars.
                                    // To avoid repeatedly unpacking input_row, we just push the outputs into datums so later scalars can see them.
                                    // Note that this doesn't mutate input_row.
                                    datums.push(datum);
                                }
                                Ok::<_, DataflowError>(row_packer.pack(&*datums))
                            }
                        });
                        let err_collection = err_collection.concat(&new_err_collection);
                        self.collections
                            .insert(relation_expr.clone(), (ok_collection, err_collection));
                    }
                }

                RelationExpr::FlatMap { input, .. } => {
                    self.ensure_rendered(input, scope, worker_index);
                    let (oks, err) = self.render_flat_map(relation_expr, None);
                    self.collections.insert(relation_expr.clone(), (oks, err));
                }

                RelationExpr::Filter { input, predicates } => {
                    if !self.try_render_map_filter_project(relation_expr, scope, worker_index) {
                        self.ensure_rendered(input, scope, worker_index);
                        let temp_storage = RowArena::new();
                        let predicates = predicates.clone();
                        let (ok_collection, err_collection) = self.collection(input).unwrap();
                        let (ok_collection, new_err_collection) =
                            ok_collection.filter_fallible(move |input_row| {
                                let datums = input_row.unpack();
                                for p in &predicates {
                                    if p.eval(&datums, &temp_storage)? != Datum::True {
                                        return Ok(false);
                                    }
                                }
                                Ok::<_, DataflowError>(true)
                            });
                        let err_collection = err_collection.concat(&new_err_collection);
                        let collections = (ok_collection, err_collection);
                        self.collections.insert(relation_expr.clone(), collections);
                    }
                }

                RelationExpr::Join {
                    inputs,
                    implementation,
                    ..
                } => {
                    for input in inputs {
                        self.ensure_rendered(input, scope, worker_index);
                    }
                    let input_mapper = expr::JoinInputMapper::new(inputs);
                    let mfp = MapFilterProject::new(input_mapper.total_columns());
                    match implementation {
                        expr::JoinImplementation::Differential(_start, _order) => {
                            let collection = self.render_join(relation_expr, mfp, scope);
                            self.collections.insert(relation_expr.clone(), collection);
                        }
                        expr::JoinImplementation::DeltaQuery(_orders) => {
                            let collection = self.render_delta_join(
                                relation_expr,
                                mfp,
                                scope,
                                worker_index,
                                |t| t.saturating_sub(1),
                            );
                            self.collections.insert(relation_expr.clone(), collection);
                        }
                        expr::JoinImplementation::Unimplemented => {
                            panic!("Attempt to render unimplemented join");
                        }
                    }
                }

                RelationExpr::Reduce { input, .. } => {
                    self.ensure_rendered(input, scope, worker_index);
                    self.render_reduce(relation_expr, scope);
                }

                RelationExpr::TopK { input, .. } => {
                    self.ensure_rendered(input, scope, worker_index);
                    self.render_topk(relation_expr);
                }

                RelationExpr::Negate { input } => {
                    self.ensure_rendered(input, scope, worker_index);
                    let (ok_collection, err_collection) = self.collection(input).unwrap();
                    let ok_collection = ok_collection.negate();
                    self.collections
                        .insert(relation_expr.clone(), (ok_collection, err_collection));
                }

                RelationExpr::Threshold { input } => {
                    self.ensure_rendered(input, scope, worker_index);
                    self.render_threshold(relation_expr);
                }

                RelationExpr::Union { base, inputs } => {
                    let (oks, errs): (Vec<_>, Vec<_>) = iter::once(&**base)
                        .chain(inputs)
                        .map(|input| {
                            self.ensure_rendered(input, scope, worker_index);
                            self.collection(input).unwrap()
                        })
                        .unzip();

                    let ok = differential_dataflow::collection::concatenate(scope, oks);
                    let err = differential_dataflow::collection::concatenate(scope, errs);

                    self.collections.insert(relation_expr.clone(), (ok, err));
                }

                RelationExpr::ArrangeBy { input, keys } => {
                    // We can avoid rendering if we have all arrangements present,
                    // and there is at least one of them (to ensure the collection
                    // is available independent of arrangements).
                    if keys.is_empty()
                        || keys
                            .iter()
                            .any(|key| self.arrangement(&input, key).is_none())
                    {
                        self.ensure_rendered(input, scope, worker_index);
                    }
                    self.render_arrangeby(relation_expr, None);
                }
            };
        }
    }
}

/// A re-useable vector of `Datum` with varying lifetimes.
///
/// This type is meant to allow us to recycle an underlying allocation with
/// a specific lifetime, under the condition that the vector is emptied before
/// this happens (to prevent leaking of invalid references).
///
/// It uses `ore::vec::repurpose_allocation` to accomplish this, which contains
/// unsafe code.
pub mod datum_vec {

    use repr::{Datum, Row};

    /// A re-useable vector of `Datum` with no particular lifetime.
    pub struct DatumVec {
        outer: Vec<Datum<'static>>,
    }

    impl DatumVec {
        /// Allocate a new instance.
        pub fn new() -> Self {
            Self { outer: Vec::new() }
        }
        /// Borrow an instance with a specific lifetime.
        ///
        /// When the result is dropped, its allocation will be returned to `self`.
        pub fn borrow<'a>(&'a mut self) -> DatumVecBorrow<'a> {
            let inner = std::mem::take(&mut self.outer);
            DatumVecBorrow {
                outer: &mut self.outer,
                inner,
            }
        }
        /// Borrow an instance with a specific lifetime, and pre-populate with a `Row`.
        pub fn borrow_with<'a>(&'a mut self, row: &'a Row) -> DatumVecBorrow<'a> {
            let mut borrow = self.borrow();
            borrow.extend(row.iter());
            borrow
        }
    }

    /// A borrowed allocation of `Datum` with a specific lifetime.
    ///
    /// When an instance is dropped, its allocation is returned to the vector from
    /// which it was extracted.
    pub struct DatumVecBorrow<'outer> {
        outer: &'outer mut Vec<Datum<'static>>,
        inner: Vec<Datum<'outer>>,
    }

    impl<'outer> Drop for DatumVecBorrow<'outer> {
        fn drop(&mut self) {
            *self.outer = ore::vec::repurpose_allocation(std::mem::take(&mut self.inner));
        }
    }

    impl<'outer> std::ops::Deref for DatumVecBorrow<'outer> {
        type Target = Vec<Datum<'outer>>;
        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }

    impl<'outer> std::ops::DerefMut for DatumVecBorrow<'outer> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }
}
