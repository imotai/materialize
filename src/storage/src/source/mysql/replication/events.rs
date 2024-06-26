// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use mysql_common::binlog::events::{QueryEvent, RowsEventData};

use timely::progress::Timestamp;
use tracing::trace;

use mz_mysql_util::{pack_mysql_row, MySqlError};
use mz_repr::Row;
use mz_storage_types::sources::mysql::GtidPartition;

use super::super::schemas::verify_schemas;
use super::super::{DefiniteError, MySqlTableName, TransientError};
use super::context::ReplContext;

/// Returns the MySqlTableName for the given table name referenced in a
/// SQL statement, using the current schema if the table name is unqualified.
fn table_ident(name: &str, current_schema: &str) -> Result<MySqlTableName, TransientError> {
    let stripped = name.replace('`', "");
    let mut name_iter = stripped.split('.');
    match (name_iter.next(), name_iter.next()) {
        (Some(t_name), None) => Ok(MySqlTableName::new(current_schema, t_name)),
        (Some(schema_name), Some(t_name)) => Ok(MySqlTableName::new(schema_name, t_name)),
        _ => Err(TransientError::Generic(anyhow::anyhow!(
            "Invalid table name from QueryEvent: {}",
            name
        ))),
    }
}

/// Handles QueryEvents from the MySQL replication stream. Since we only use
/// row-based replication, we only expect to see QueryEvents for DDL changes.
///
/// From the MySQL docs: 'A Query_event is created for each query that modifies
/// the database, unless the query is logged row-based.' This means that we can
/// expect any DDL changes to be represented as QueryEvents, which we must parse
/// to figure out if any of the tables we care about have been affected.
///
/// This function returns a bool to represent whether the event that was handled
/// represents a 'complete' event that should cause the frontier to advance beyond
/// the current GTID.
pub(super) async fn handle_query_event(
    event: QueryEvent<'_>,
    ctx: &mut ReplContext<'_>,
    new_gtid: &GtidPartition,
) -> Result<bool, TransientError> {
    let (id, worker_id) = (ctx.config.id, ctx.config.worker_id);

    let query = event.query();
    let current_schema = event.schema();
    let mut is_complete_event = false;

    // MySQL does not permit transactional DDL, so luckily we don't need to
    // worry about tracking BEGIN/COMMIT query events. We only need to look
    // for DDL changes that affect the schema of the tables we care about.
    let mut query_iter = query.split_ascii_whitespace();
    let first = query_iter.next();
    let second = query_iter.next();
    match (
        first.map(str::to_ascii_lowercase).as_deref(),
        second.map(str::to_ascii_lowercase).as_deref(),
    ) {
        // Detect `ALTER TABLE <tbl>`, `RENAME TABLE <tbl>` statements
        (Some("alter") | Some("rename"), Some("table")) => {
            let table = table_ident(
                query_iter.next().ok_or_else(|| {
                    TransientError::Generic(anyhow::anyhow!("Invalid DDL query: {}", query))
                })?,
                &current_schema,
            )?;
            is_complete_event = true;
            if ctx.table_info.contains_key(&table) {
                trace!(%id, "timely-{worker_id} DDL change detected \
                       for {table:?}");
                let (output_index, table_desc) = &ctx.table_info[&table];
                let mut conn = ctx
                    .connection_config
                    .connect(
                        &format!("timely-{worker_id} MySQL "),
                        &ctx.config.config.connection_context.ssh_tunnel_manager,
                    )
                    .await?;
                if let Some((err_table, err)) = verify_schemas(
                    &mut *conn,
                    &[(&table, table_desc)],
                    ctx.text_columns,
                    ctx.ignore_columns,
                )
                .await?
                .into_iter()
                .next()
                {
                    assert_eq!(err_table, &table, "Unexpected table verification error");
                    trace!(%id, "timely-{worker_id} DDL change \
                           verification error for {table:?}: {err:?}");
                    let gtid_cap = ctx.data_cap_set.delayed(new_gtid);
                    ctx.data_output
                        .give(&gtid_cap, ((*output_index, Err(err)), new_gtid.clone(), 1));
                    ctx.errored_tables.insert(table.clone());
                }
            }
        }
        // Detect `DROP TABLE [IF EXISTS] <tbl>, <tbl>` statements. Since
        // this can drop multiple tables we just check all tables we care about
        (Some("drop"), Some("table")) => {
            let mut conn = ctx
                .connection_config
                .connect(
                    &format!("timely-{worker_id} MySQL "),
                    &ctx.config.config.connection_context.ssh_tunnel_manager,
                )
                .await?;
            let expected = ctx
                .table_info
                .iter()
                .filter(|(t, _)| !ctx.errored_tables.contains(t))
                .map(|(t, d)| (t, &d.1))
                .collect::<Vec<_>>();
            let schema_errors =
                verify_schemas(&mut *conn, &expected, ctx.text_columns, ctx.ignore_columns).await?;
            is_complete_event = true;
            for (dropped_table, err) in schema_errors {
                if ctx.table_info.contains_key(dropped_table)
                    && !ctx.errored_tables.contains(dropped_table)
                {
                    trace!(%id, "timely-{worker_id} DDL change \
                           dropped table: {dropped_table:?}: {err:?}");
                    if let Some((output_index, _)) = ctx.table_info.get(dropped_table) {
                        let gtid_cap = ctx.data_cap_set.delayed(new_gtid);
                        ctx.data_output
                            .give(&gtid_cap, ((*output_index, Err(err)), new_gtid.clone(), 1));
                        ctx.errored_tables.insert(dropped_table.clone());
                    }
                }
            }
        }
        // Detect `TRUNCATE [TABLE] <tbl>` statements
        (Some("truncate"), Some(_)) => {
            // We need the original un-lowercased version of 'second' since it might be a table ref
            let second = second.expect("known to be Some");
            let table = if second.eq_ignore_ascii_case("table") {
                table_ident(
                    query_iter.next().ok_or_else(|| {
                        TransientError::Generic(anyhow::anyhow!("Invalid DDL query: {}", query))
                    })?,
                    &current_schema,
                )?
            } else {
                table_ident(second, &current_schema)?
            };
            is_complete_event = true;
            if ctx.table_info.contains_key(&table) {
                trace!(%id, "timely-{worker_id} TRUNCATE detected \
                       for {table:?}");
                if let Some((output_index, _)) = ctx.table_info.get(&table) {
                    let gtid_cap = ctx.data_cap_set.delayed(new_gtid);
                    ctx.data_output.give(
                        &gtid_cap,
                        (
                            (
                                *output_index,
                                Err(DefiniteError::TableTruncated(table.to_string())),
                            ),
                            new_gtid.clone(),
                            1,
                        ),
                    );
                    ctx.errored_tables.insert(table);
                }
            }
        }
        // Detect `COMMIT` statements which signify the end of a transaction on non-XA capable
        // storage engines
        (Some("commit"), None) => {
            is_complete_event = true;
        }
        _ => {}
    }

    Ok(is_complete_event)
}

/// Handles RowsEvents from the MySQL replication stream. These events contain
/// insert/update/delete events for a single transaction or committed statement.
///
/// We use these events to update the dataflow with the new rows, and return a new
/// frontier with which to advance the dataflow's progress.
pub(super) fn handle_rows_event(
    event: RowsEventData<'_>,
    ctx: &mut ReplContext<'_>,
    new_gtid: &GtidPartition,
    event_buffer: &mut Vec<(
        (usize, Result<Row, DefiniteError>),
        GtidPartition,
        mz_repr::Diff,
    )>,
) -> Result<(), TransientError> {
    let (id, worker_id) = (ctx.config.id, ctx.config.worker_id);

    // Find the relevant table
    let binlog_table_id = event.table_id();
    let table_map_event = ctx
        .stream
        .get_ref()
        .get_tme(binlog_table_id)
        .ok_or_else(|| TransientError::Generic(anyhow::anyhow!("Table map event not found")))?;
    let table = MySqlTableName::new(
        &*table_map_event.database_name(),
        &*table_map_event.table_name(),
    );

    if ctx.errored_tables.contains(&table) {
        return Ok(());
    }

    let (output_index, table_desc) = match &ctx.table_info.get(&table) {
        Some((output_index, table_desc)) => (output_index, table_desc),
        None => {
            // We don't know about this table, so skip this event
            return Ok(());
        }
    };

    trace!(%id, "timely-{worker_id} handling RowsEvent for {table:?}");

    // Capability for this event.
    let gtid_cap = ctx.data_cap_set.delayed(new_gtid);

    // Iterate over the rows in this RowsEvent. Each row is a pair of 'before_row', 'after_row',
    // to accomodate for updates and deletes (which include a before_row),
    // and updates and inserts (which inclued an after row).
    let mut final_row = Row::default();
    let mut rows_iter = event.rows(table_map_event);
    let mut rewind_count = 0;
    let mut additions = 0;
    let mut retractions = 0;
    while let Some(Ok((before_row, after_row))) = rows_iter.next() {
        // Update metrics for updates/inserts/deletes
        match (&before_row, &after_row) {
            (None, None) => {}
            (Some(_), Some(_)) => {
                ctx.metrics.updates.inc();
            }
            (None, Some(_)) => {
                ctx.metrics.inserts.inc();
            }
            (Some(_), None) => {
                ctx.metrics.deletes.inc();
            }
        }

        let updates = [before_row.map(|r| (r, -1)), after_row.map(|r| (r, 1))];
        for (binlog_row, diff) in updates.into_iter().flatten() {
            let row = mysql_async::Row::try_from(binlog_row)?;
            let event = match pack_mysql_row(&mut final_row, row, table_desc) {
                Ok(row) => Ok(row),
                // Produce a DefiniteError in the stream for any rows that fail to decode
                Err(err @ MySqlError::ValueDecodeError { .. }) => {
                    Err(DefiniteError::ValueDecodeError(err.to_string()))
                }
                Err(err) => Err(err)?,
            };

            let data = (*output_index, event);

            // Rewind this update if it was already present in the snapshot
            if let Some((_rewind_data_cap, rewind_req)) = ctx.rewinds.get(&table) {
                if !rewind_req.snapshot_upper.less_equal(new_gtid) {
                    rewind_count += 1;
                    event_buffer.push((data.clone(), GtidPartition::minimum(), -diff));
                }
            }
            if diff > 0 {
                additions += 1;
            } else {
                retractions += 1;
            }
            ctx.data_output
                .give(&gtid_cap, (data, new_gtid.clone(), diff));
        }
    }

    // We want to emit data in individual pieces to allow timely to break large chunks of data into
    // containers. Naively interleaving new data and rewinds in the loop above defeats a timely
    // optimization that caches push buffers if the `.give()` time has not changed.
    //
    // Instead, we buffer rewind events into a reusable buffer, and emit all at once here at the end.

    if !event_buffer.is_empty() {
        let (rewind_data_cap, _) = ctx.rewinds.get(&table).unwrap();
        for d in event_buffer.drain(..) {
            ctx.data_output.give(rewind_data_cap, d);
        }
    }

    trace!(
        %id,
        "timely-{worker_id} sent updates for {new_gtid:?} \
            with {} updates ({} additions, {} retractions) and {} \
            rewinds",
        additions + retractions,
        additions,
        retractions,
        rewind_count,
    );

    Ok(())
}
