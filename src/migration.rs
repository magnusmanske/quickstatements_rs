//! One-off migration of the oversized `command` table (see README, "Archive").
//!
//! Old DONE/STOP batches are archived to files, leaving `command` untouched;
//! all other batches are copied to `command_new`, which then replaces
//! `command`. Copy passes run while the bot is running and can be repeated:
//! a batch is copied again if it changed since (newer `ts_last_change`, or
//! different per-status counts, which catch status-only changes by the PHP
//! frontend). Only the swap needs the bot and the web service stopped.
//!
//! Remove this module once `command_old` has been dropped.

use crate::archive::{self, batch_status_counts, is_finished, Archive, COLUMNS};
use crate::error::{QsError, QsResult};
use crate::qs_config::{db_timestamp, QuickStatements};
use chrono::{Duration, Utc};
use std::collections::HashMap;
use wikimisc::mysql_async as my;
use wikimisc::mysql_async::prelude::*;

/// Like `command`, without the redundant `batch_id` index and with a compressed `json` column
const CREATE_NEW_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS `command_new` (
  `id` int(11) unsigned NOT NULL AUTO_INCREMENT,
  `batch_id` int(11) NOT NULL,
  `num` int(11) NOT NULL,
  `json` mediumtext COMPRESSED NOT NULL,
  `status` varchar(8) NOT NULL DEFAULT '',
  `message` tinytext NOT NULL,
  `ts_change` varchar(14) NOT NULL DEFAULT '',
  PRIMARY KEY (`id`),
  KEY `batch_id_2` (`batch_id`,`status`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci";

const CREATE_PROGRESS_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS `command_migration` (
  `batch_id` int(11) NOT NULL,
  `ts_copied` varchar(14) NOT NULL,
  PRIMARY KEY (`batch_id`)
) ENGINE=InnoDB";

/// Changes stamped by clocks up to this far behind ours still trigger a re-copy
const CLOCK_MARGIN_MINUTES: i64 = 5;

/// Batches per query when comparing or verifying
const CHUNK_SIZE: usize = 1000;

/// (id, status, ts_last_change, ts_archived, ts_copied)
type BatchState = (i64, String, String, Option<String>, Option<String>);

/// Copies all batches that are new or changed since their last copy, and
/// archives old batches. Returns the number of batches copied, and of batches
/// that failed to copy or archive.
pub async fn copy_pass(qs: &QuickStatements, archive: &Archive) -> QsResult<(usize, usize)> {
    let mut conn = qs.get_db_conn().await?;
    for sql in [
        archive::CREATE_TABLE_SQL,
        CREATE_NEW_TABLE_SQL,
        CREATE_PROGRESS_TABLE_SQL,
    ] {
        conn.query_drop(sql).await?;
    }
    let cutoff = archive::cutoff_timestamp(qs.archive_months());
    let batches: Vec<BatchState> = conn
        .query("SELECT b.id,b.`status`,b.ts_last_change,a.ts_archived,m.ts_copied FROM batch b LEFT JOIN command_archive a ON a.batch_id=b.id LEFT JOIN command_migration m ON m.batch_id=b.id ORDER BY b.id")
        .await?;

    let mut to_copy = vec![];
    let mut to_compare = vec![];
    let mut failed = 0;
    for (batch_id, status, ts_last_change, ts_archived, ts_copied) in batches {
        let finished = is_finished(&status);
        match ts_copied {
            // Once copied, a batch stays in `command_new`
            Some(ts) if ts_last_change < ts => to_compare.push(batch_id),
            Some(_) => to_copy.push(batch_id),
            None if finished && ts_archived.is_some_and(|ts| ts_last_change < ts) => {}
            None if finished && ts_last_change < cutoff => {
                if let Err(e) = archive.archive_batch(qs, batch_id, true).await {
                    log::warn!("Batch #{batch_id} not archived: {e}");
                    failed += 1;
                }
            }
            None => to_copy.push(batch_id),
        }
    }
    to_copy.extend(changed_batches(&mut conn, &to_compare).await?);

    let mut copied = 0;
    for batch_id in to_copy {
        match copy_batch(&mut conn, batch_id).await {
            Ok(()) => copied += 1,
            Err(e) => {
                log::warn!("Batch #{batch_id} not copied: {e}");
                failed += 1;
            }
        }
    }
    Ok((copied, failed))
}

/// Batches whose per-status counts differ between `command` and `command_new`
async fn changed_batches(conn: &mut my::Conn, batch_ids: &[i64]) -> QsResult<Vec<i64>> {
    let mut ret = vec![];
    for chunk in batch_ids.chunks(CHUNK_SIZE) {
        let old = batch_status_counts(conn, "command", chunk).await?;
        let new = batch_status_counts(conn, "command_new", chunk).await?;
        ret.extend(chunk.iter().filter(|id| old.get(id) != new.get(id)));
    }
    Ok(ret)
}

async fn copy_batch(conn: &mut my::Conn, batch_id: i64) -> QsResult<()> {
    let ts_copied = db_timestamp(Utc::now() - Duration::minutes(CLOCK_MARGIN_MINUTES));
    let mut tx = conn.start_transaction(my::TxOpts::default()).await?;
    tx.exec_drop(
        "DELETE FROM command_new WHERE batch_id=:batch_id",
        params! {batch_id},
    )
    .await?;
    tx.exec_drop(
        format!("INSERT INTO command_new ({COLUMNS}) SELECT {COLUMNS} FROM command WHERE batch_id=:batch_id"),
        params! {batch_id},
    )
    .await?;
    tx.exec_drop(
        "REPLACE INTO command_migration (batch_id,ts_copied) VALUES (:batch_id,:ts_copied)",
        params! {batch_id, ts_copied},
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Final copy pass, then replaces `command` with `command_new`; the old table
/// is kept as `command_old`. The bot and the web service must be stopped.
pub async fn swap(qs: &QuickStatements, archive: &Archive) -> QsResult<()> {
    let (copied, failed) = copy_pass(qs, archive).await?;
    log::info!("Final pass copied {copied} batches");
    if failed > 0 {
        return Err(QsError::StringError(format!(
            "{failed} batches failed to copy or archive, see log"
        )));
    }

    let mut conn = qs.get_db_conn().await?;
    let copied_ids: Vec<i64> = conn.query("SELECT batch_id FROM command_migration").await?;
    let changed = changed_batches(&mut conn, &copied_ids).await?;
    if !changed.is_empty() {
        return Err(QsError::StringError(format!(
            "{} batches still changing (e.g. #{}); are the bot and the web service stopped?",
            changed.len(),
            changed[0]
        )));
    }

    let max_id: Option<Option<i64>> = conn.query_first("SELECT MAX(id) FROM command").await?;
    let next_id = max_id.flatten().unwrap_or(0) + 1;
    conn.query_drop(format!("ALTER TABLE command_new AUTO_INCREMENT={next_id}"))
        .await?;
    conn.query_drop("RENAME TABLE command TO command_old, command_new TO command")
        .await?;
    Ok(())
}

/// Checks that every batch in `command_old` is either live in `command` or
/// archived, with the same number of commands. Returns the number of problems.
pub async fn verify(qs: &QuickStatements, archive: &Archive) -> QsResult<usize> {
    let mut conn = qs.get_db_conn().await?;
    let (min, max): (Option<i64>, Option<i64>) = conn
        .query_first("SELECT MIN(batch_id),MAX(batch_id) FROM command_old")
        .await?
        .unwrap_or_default();
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(0);
    };

    let (mut problems, mut total) = (0, 0);
    for start in (min..=max).step_by(CHUNK_SIZE) {
        let range = format!(
            "batch_id BETWEEN {start} AND {}",
            start + CHUNK_SIZE as i64 - 1
        );
        let count_sql = |table: &str| {
            format!("SELECT batch_id,COUNT(*) FROM {table} WHERE {range} GROUP BY batch_id")
        };
        let old: Vec<(i64, u64)> = conn.query(count_sql("command_old")).await?;
        let live: HashMap<i64, u64> = conn
            .query(count_sql("command"))
            .await?
            .into_iter()
            .collect();
        let archived: HashMap<i64, u64> = conn
            .query(format!(
                "SELECT batch_id,num_commands FROM command_archive WHERE {range}"
            ))
            .await?
            .into_iter()
            .collect();
        for (batch_id, count) in old {
            total += count;
            let ok = match live.get(&batch_id) {
                Some(live_count) => *live_count == count,
                None => archived.get(&batch_id) == Some(&count) && archive.path(batch_id).exists(),
            };
            if !ok {
                problems += 1;
                log::error!(
                    "Batch #{batch_id}: {count} commands in command_old, live: {:?}, archived: {:?}",
                    live.get(&batch_id),
                    archived.get(&batch_id)
                );
            }
        }
    }
    log::info!("{total} commands in command_old, {problems} batches with problems");
    Ok(problems)
}
