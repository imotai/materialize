// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Reclocking compatibility code until the whole ingestion pipeline is transformed to native
//! timestamps

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use differential_dataflow::lattice::Lattice;
use fail::fail_point;
use futures::StreamExt;
use futures::stream::LocalBoxStream;
use mz_ore::soft_panic_or_log;
use mz_ore::vec::VecExt;
use mz_persist_client::Diagnostics;
use mz_persist_client::cache::PersistClientCache;
use mz_persist_client::error::UpperMismatch;
use mz_persist_client::read::ListenEvent;
use mz_persist_client::write::WriteHandle;
use mz_persist_types::Codec64;
use mz_persist_types::codec_impls::UnitSchema;
use mz_repr::{Diff, GlobalId, RelationDesc};
use mz_storage_client::util::remap_handle::{RemapHandle, RemapHandleReader};
use mz_storage_types::StorageDiff;
use mz_storage_types::controller::CollectionMetadata;
use mz_storage_types::sources::{SourceData, SourceTimestamp};
use timely::order::{PartialOrder, TotalOrder};
use timely::progress::Timestamp;
use timely::progress::frontier::Antichain;
use tokio::sync::watch;

/// A handle to a persist shard that stores remap bindings
pub struct PersistHandle<FromTime: SourceTimestamp, IntoTime: Timestamp + Lattice + Codec64> {
    events: LocalBoxStream<
        'static,
        ListenEvent<
            IntoTime,
            (
                (Result<SourceData, String>, Result<(), String>),
                IntoTime,
                StorageDiff,
            ),
        >,
    >,
    write_handle: WriteHandle<SourceData, (), IntoTime, StorageDiff>,
    /// Whether or not this handle is in read-only mode.
    read_only_rx: watch::Receiver<bool>,
    pending_batch: Vec<(FromTime, IntoTime, Diff)>,
    // Reports `self`'s write frontier.
    shared_write_frontier: Rc<RefCell<Antichain<IntoTime>>>,
}

impl<FromTime: Timestamp, IntoTime: Timestamp + Sync> PersistHandle<FromTime, IntoTime>
where
    FromTime: SourceTimestamp,
    IntoTime: Timestamp + TotalOrder + Lattice + Codec64,
{
    pub async fn new(
        persist_clients: Arc<PersistClientCache>,
        read_only_rx: watch::Receiver<bool>,
        metadata: CollectionMetadata,
        as_of: Antichain<IntoTime>,
        shared_write_frontier: Rc<RefCell<Antichain<IntoTime>>>,
        // additional information to improve logging
        id: GlobalId,
        operator: &str,
        worker_id: usize,
        worker_count: usize,
        // Must match the `FromTime`. Ideally we would be able to discover this
        // from `SourceTimestamp`, but each source would need a specific `SourceTimestamp`
        // implementation, as they do not share remap `RelationDesc`'s (columns names
        // are different).
        //
        // TODO(guswynn): use the type-system to prevent misuse here.
        remap_relation_desc: RelationDesc,
        remap_collection_id: GlobalId,
    ) -> anyhow::Result<Self> {
        let remap_shard = if let Some(remap_shard) = metadata.remap_shard {
            remap_shard
        } else {
            panic!(
                "cannot create remap PersistHandle for collection without remap shard: {id}, metadata: {:?}",
                metadata
            );
        };

        let persist_client = persist_clients
            .open(metadata.persist_location.clone())
            .await
            .context("error creating persist client")?;

        let (write_handle, mut read_handle) = persist_client
            .open(
                remap_shard,
                Arc::new(remap_relation_desc),
                Arc::new(UnitSchema),
                Diagnostics {
                    shard_name: remap_collection_id.to_string(),
                    handle_purpose: format!("reclock for {}", id),
                },
                false,
            )
            .await
            .expect("invalid usage");

        let upper = write_handle.upper();
        // We want a leased reader because elsewhere in the code the `as_of`
        // time may also be determined by another `ReadHandle`, and the pair of
        // them offer the invariant that we need (that the `as_of` if <= this
        // `since`). Using a `SinceHandle` here does not offer the same
        // invariant when paired with a `ReadHandle`.
        let since = read_handle.since();

        // Allow manually simulating the scenario where the since of the remap
        // shard has advanced too far.
        fail_point!("invalid_remap_as_of");

        if since.is_empty() {
            // This can happen when, say, a source is being dropped but we on
            // the cluster are busy and notice that only later. In those cases
            // it can happen that we still try to render an ingestion that is
            // not valid anymore and where the shards it uses are not valid to
            // use anymore.
            //
            // This is a rare race condition and something that is expected to
            // happen every now and then. It's not a bug in the current way of
            // how things work.
            tracing::info!(
                source_id = %id,
                %worker_id,
                "since of remap shard is the empty antichain, suspending...");

            // We wait 5 hours to give the commands a chance to arrive at this
            // replica and for it to drop our dataflow.
            tokio::time::sleep(Duration::from_secs(5 * 60 * 60)).await;

            // If we're still here after 5 hours, something has gone wrong and
            // we complain.
            soft_panic_or_log!(
                "since of remap shard is the empty antichain, source_id = {id}, worker_id = {worker_id}"
            );
        }

        if !PartialOrder::less_equal(since, &as_of) {
            anyhow::bail!(
                "invalid as_of: as_of({as_of:?}) < since({since:?}), \
                source {id}, \
                remap_shard: {:?}",
                metadata.remap_shard
            );
        }

        assert!(
            as_of.elements() == [IntoTime::minimum()] || PartialOrder::less_than(&as_of, upper),
            "invalid as_of: upper({upper:?}) <= as_of({as_of:?})",
        );

        tracing::info!(
            ?since,
            ?as_of,
            ?upper,
            "{operator}({id}) {worker_id}/{worker_count} initializing PersistHandle"
        );

        use futures::stream;
        let events = stream::once(async move {
            let updates = read_handle
                .snapshot_and_fetch(as_of.clone())
                .await
                .expect("since <= as_of asserted");
            let snapshot = stream::once(std::future::ready(ListenEvent::Updates(updates)));

            let listener = read_handle
                .listen(as_of.clone())
                .await
                .expect("since <= as_of asserted");

            let listen_stream = stream::unfold(listener, |mut listener| async move {
                let events = stream::iter(listener.fetch_next().await);
                Some((events, listener))
            })
            .flatten();

            snapshot.chain(listen_stream)
        })
        .flatten()
        .boxed_local();

        Ok(Self {
            events,
            write_handle,
            read_only_rx,
            pending_batch: vec![],
            shared_write_frontier,
        })
    }
}

#[async_trait::async_trait(?Send)]
impl<FromTime, IntoTime> RemapHandleReader for PersistHandle<FromTime, IntoTime>
where
    FromTime: SourceTimestamp,
    IntoTime: Timestamp + Lattice + Codec64,
{
    type FromTime = FromTime;
    type IntoTime = IntoTime;

    async fn next(
        &mut self,
    ) -> Option<(
        Vec<(Self::FromTime, Self::IntoTime, Diff)>,
        Antichain<Self::IntoTime>,
    )> {
        while let Some(event) = self.events.next().await {
            match event {
                ListenEvent::Progress(new_upper) => {
                    // Peel off a batch of pending data
                    let batch = self
                        .pending_batch
                        .drain_filter_swapping(|(_, ts, _)| !new_upper.less_equal(ts))
                        .collect();
                    return Some((batch, new_upper));
                }
                ListenEvent::Updates(msgs) => {
                    for ((update, _), into_ts, diff) in msgs {
                        let from_ts = FromTime::decode_row(
                            &update.expect("invalid row").0.expect("invalid row"),
                        );
                        self.pending_batch.push((from_ts, into_ts, diff.into()));
                    }
                }
            }
        }
        None
    }
}

#[async_trait::async_trait(?Send)]
impl<FromTime, IntoTime> RemapHandle for PersistHandle<FromTime, IntoTime>
where
    FromTime: SourceTimestamp,
    IntoTime: Timestamp + TotalOrder + Lattice + Codec64 + Sync,
{
    async fn compare_and_append(
        &mut self,
        updates: Vec<(Self::FromTime, Self::IntoTime, Diff)>,
        upper: Antichain<Self::IntoTime>,
        new_upper: Antichain<Self::IntoTime>,
    ) -> Result<(), UpperMismatch<Self::IntoTime>> {
        if *self.read_only_rx.borrow() {
            // We have to wait for either us coming out of read-only mode or
            // someone else advancing the upper. If we just returned an
            // `UpperMismatch` while in read-only mode, we would go into a busy
            // loop because we'd be called over and over again. One presumes.

            loop {
                tracing::trace!(
                    ?upper,
                    ?new_upper,
                    persist_upper = ?self.write_handle.upper(),
                    "persist remap handle is in read-only mode, waiting until we come out of it or the shard upper advances");

                // We don't try to be too smart here, and for example use
                // `wait_for_upper_past()`. We'd have to use a select!, which
                // would require cancel safety of `wait_for_upper_past()`, which
                // it doesn't advertise.
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), self.read_only_rx.changed()).await;

                if !*self.read_only_rx.borrow() {
                    tracing::trace!(
                        ?upper,
                        ?new_upper,
                        persist_upper = ?self.write_handle.upper(),
                        "persist remap handle has come out of read-only mode"
                    );

                    // It's okay to write now.
                    break;
                }

                let current_upper = self.write_handle.fetch_recent_upper().await;

                if PartialOrder::less_than(&upper, current_upper) {
                    tracing::trace!(
                        ?upper,
                        ?new_upper,
                        persist_upper = ?current_upper,
                        "someone else advanced the upper, aborting write"
                    );

                    return Err(UpperMismatch {
                        current: current_upper.clone(),
                        expected: upper,
                    });
                }
            }
        }

        let row_updates = updates.into_iter().map(|(from_ts, into_ts, diff)| {
            (
                (SourceData(Ok(from_ts.encode_row())), ()),
                into_ts,
                diff.into_inner(),
            )
        });

        match self
            .write_handle
            .compare_and_append(row_updates, upper, new_upper.clone())
            .await
        {
            Ok(result) => {
                *self.shared_write_frontier.borrow_mut() = new_upper;
                return result;
            }
            Err(invalid_use) => panic!("compare_and_append failed: {invalid_use}"),
        }
    }

    fn upper(&self) -> &Antichain<Self::IntoTime> {
        self.write_handle.upper()
    }
}
