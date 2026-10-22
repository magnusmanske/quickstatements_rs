//! Archive for the commands of old batches, to keep the `command` table small.
//!
//! An archived batch's commands are stored as gzipped JSON lines, one file per
//! batch at `<archive_dir>/<batch_id / 1000>/<batch_id>.jsonl.gz`, and the
//! batch gets a row in `command_archive`. Rows in `command` always take
//! precedence: a batch with live rows is read from the table, even if an older
//! archive file exists. The PHP frontend reads the same files, so keep the
//! format in sync with it.

use crate::error::{QsError, QsResult};
use crate::qs_config::{CommandRow, QuickStatements};
use chrono::{DateTime, Months, Utc};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use wikimisc::mysql_async as my;
use wikimisc::mysql_async::prelude::*;
use wikimisc::timestamp::TimeStamp;

/// Columns of the `command` table, in `StoredCommand` order
pub(crate) const COLUMNS: &str = "id,batch_id,num,json,`status`,message,ts_change";

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS `command_archive` (
  `batch_id` int(11) NOT NULL,
  `num_commands` int(11) unsigned NOT NULL,
  `status_counts` varchar(255) NOT NULL,
  `ts_archived` varchar(14) NOT NULL,
  PRIMARY KEY (`batch_id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci";

/// Commands per multi-row INSERT when restoring
const RESTORE_CHUNK_SIZE: usize = 500;

/// Number of commands per status, e.g. {"DONE": 10, "ERROR": 2}
pub type StatusCounts = BTreeMap<String, u64>;

/// One row of the `command` table, as stored in the database or an archive file
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCommand {
    pub id: i64,
    pub batch_id: i64,
    pub num: i64,
    pub json: String,
    pub status: String,
    pub message: String,
    pub ts_change: String,
}

impl From<CommandRow> for StoredCommand {
    fn from((id, batch_id, num, json, status, message, ts_change): CommandRow) -> Self {
        Self {
            id,
            batch_id,
            num,
            json,
            status,
            message,
            ts_change,
        }
    }
}

/// DONE/STOP batches last changed before this time are due for archiving
fn cutoff_timestamp(months: u32) -> String {
    let cutoff = Utc::now()
        .checked_sub_months(Months::new(months))
        .unwrap_or(DateTime::<Utc>::MIN_UTC); // Archive nothing rather than everything
    TimeStamp::datetime(&cutoff)
}

fn is_finished(status: &str) -> bool {
    status == "DONE" || status == "STOP"
}

/// Per-status counts of a batch's rows in `command`
async fn live_status_counts(q: &mut impl Queryable, batch_id: i64) -> QsResult<StatusCounts> {
    let sql = "SELECT `status`,COUNT(*) FROM command WHERE batch_id=:batch_id GROUP BY `status`";
    let rows: Vec<(String, u64)> = q.exec(sql, params! {batch_id}).await?;
    Ok(rows.into_iter().collect())
}

pub(crate) async fn has_live_rows(q: &mut impl Queryable, batch_id: i64) -> QsResult<bool> {
    let sql = "SELECT id FROM command WHERE batch_id=:batch_id LIMIT 1";
    let row: Option<i64> = q.exec_first(sql, params! {batch_id}).await?;
    Ok(row.is_some())
}

async fn insert_commands(q: &mut impl Queryable, commands: &[StoredCommand]) -> QsResult<()> {
    if commands.is_empty() {
        return Ok(());
    }
    let placeholders = vec!["(?,?,?,?,?,?,?)"; commands.len()].join(",");
    let sql = format!("INSERT INTO command ({COLUMNS}) VALUES {placeholders}");
    let params: Vec<my::Value> = commands
        .iter()
        .flat_map(|c| {
            [
                c.id.into(),
                c.batch_id.into(),
                c.num.into(),
                c.json.as_str().into(),
                c.status.as_str().into(),
                c.message.as_str().into(),
                c.ts_change.as_str().into(),
            ]
        })
        .collect();
    q.exec_drop(sql, params).await?;
    Ok(())
}

/// Reads the commands of an archive file, one per line
fn read_file(path: &Path) -> io::Result<impl Iterator<Item = QsResult<StoredCommand>>> {
    let lines = BufReader::new(GzDecoder::new(File::open(path)?)).lines();
    Ok(lines.map(|line| Ok(serde_json::from_str(&line?)?)))
}

fn count_statuses(
    commands: impl Iterator<Item = QsResult<StoredCommand>>,
) -> QsResult<StatusCounts> {
    let mut counts = StatusCounts::new();
    for command in commands {
        *counts.entry(command?.status).or_default() += 1;
    }
    Ok(counts)
}

/// Writes an archive file under a temporary name; `finish` checks it and
/// moves it into place.
struct ArchiveWriter {
    path: PathBuf,
    tmp: PathBuf,
    gz: GzEncoder<BufWriter<File>>,
    counts: StatusCounts,
}

impl ArchiveWriter {
    fn create(path: PathBuf) -> QsResult<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let gz = GzEncoder::new(BufWriter::new(File::create(&tmp)?), Compression::default());
        Ok(Self {
            path,
            tmp,
            gz,
            counts: StatusCounts::new(),
        })
    }

    fn push(&mut self, command: &StoredCommand) -> QsResult<()> {
        serde_json::to_writer(&mut self.gz, command)?;
        self.gz.write_all(b"\n")?;
        *self.counts.entry(command.status.clone()).or_default() += 1;
        Ok(())
    }

    /// Returns the per-status counts. An empty archive is discarded.
    fn finish(self) -> QsResult<StatusCounts> {
        let file = self.gz.finish()?.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        if self.counts.is_empty() {
            fs::remove_file(&self.tmp)?;
        } else if count_statuses(read_file(&self.tmp)?)? != self.counts {
            return Err(QsError::StringError(format!(
                "{} does not read back correctly",
                self.tmp.display()
            )));
        } else {
            fs::rename(&self.tmp, &self.path)?;
        }
        Ok(self.counts)
    }
}

#[derive(Debug, Clone)]
pub struct Archive {
    dir: PathBuf,
}

impl Archive {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self, batch_id: i64) -> PathBuf {
        self.dir
            .join((batch_id / 1000).to_string())
            .join(format!("{batch_id}.jsonl.gz"))
    }

    /// The archived commands of a batch, or `None` if it has no archive file
    pub fn read(&self, batch_id: i64) -> QsResult<Option<Vec<StoredCommand>>> {
        self.read_filtered(batch_id, |_| true)
    }

    /// Like `read`, but only the commands that `keep` accepts
    pub fn read_filtered(
        &self,
        batch_id: i64,
        mut keep: impl FnMut(&StoredCommand) -> bool,
    ) -> QsResult<Option<Vec<StoredCommand>>> {
        match read_file(&self.path(batch_id)) {
            Ok(commands) => Ok(Some(
                commands
                    .filter(|c| c.as_ref().map_or(true, &mut keep))
                    .collect::<QsResult<_>>()?,
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Per-status counts of an archived batch, from `command_archive`
    pub async fn status_counts(
        &self,
        qs: &QuickStatements,
        batch_id: i64,
    ) -> QsResult<Option<StatusCounts>> {
        let sql = "SELECT status_counts FROM command_archive WHERE batch_id=:batch_id";
        let json: Option<String> = qs
            .get_db_conn()
            .await?
            .exec_first(sql, params! {batch_id})
            .await?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    /// Writes the batch's live commands to its archive file, replacing any
    /// previous one.
    async fn write_from_db(&self, conn: &mut my::Conn, batch_id: i64) -> QsResult<StatusCounts> {
        let mut writer = ArchiveWriter::create(self.path(batch_id))?;
        let tmp = writer.tmp.clone();
        let mut error = None;
        let sql = format!("SELECT {COLUMNS} FROM command WHERE batch_id=:batch_id ORDER BY id");
        let result = async {
            conn.exec_iter(sql, params! {batch_id})
                .await?
                .for_each(|row| {
                    if error.is_none() {
                        let command = StoredCommand::from(my::from_row::<CommandRow>(row));
                        error = writer.push(&command).err();
                    }
                })
                .await?;
            match error {
                Some(e) => Err(e),
                None => writer.finish(),
            }
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }

    /// Archives the commands of a batch and returns their number. The rows are
    /// then removed from `command`, provided the batch is still DONE/STOP and
    /// its commands are unchanged.
    pub async fn archive_batch(&self, qs: &QuickStatements, batch_id: i64) -> QsResult<u64> {
        // Taken before reading, so any later change to the batch is newer
        let ts_archived = qs.timestamp();
        let mut conn = qs.get_db_conn().await?;
        let counts = self.write_from_db(&mut conn, batch_id).await?;
        let num_commands: u64 = counts.values().sum();
        if num_commands == 0 {
            return Ok(0);
        }

        // Dropping the transaction early rolls it back
        let mut tx = conn.start_transaction(my::TxOpts::default()).await?;
        let sql = "SELECT `status` FROM batch WHERE id=:batch_id FOR UPDATE";
        let status: Option<String> = tx.exec_first(sql, params! {batch_id}).await?;
        if !status.as_deref().is_some_and(is_finished)
            || live_status_counts(&mut tx, batch_id).await? != counts
        {
            return Err(QsError::StringError(format!(
                "Batch #{batch_id} changed while archiving, kept live"
            )));
        }
        let status_counts = serde_json::to_string(&counts)?;
        tx.exec_drop(
            "REPLACE INTO command_archive (batch_id,num_commands,status_counts,ts_archived) VALUES (:batch_id,:num_commands,:status_counts,:ts_archived)",
            params! {batch_id, num_commands, status_counts, ts_archived},
        )
        .await?;
        tx.exec_drop(
            "DELETE FROM command WHERE batch_id=:batch_id",
            params! {batch_id},
        )
        .await?;
        tx.commit().await?;
        Ok(num_commands)
    }

    /// Moves an archived batch back into `command`, unless it has live rows
    /// already. Returns true if the batch was restored.
    pub async fn restore_batch(&self, qs: &QuickStatements, batch_id: i64) -> QsResult<bool> {
        let mut conn = qs.get_db_conn().await?;
        if has_live_rows(&mut conn, batch_id).await? {
            return Ok(false);
        }
        let commands = match read_file(&self.path(batch_id)) {
            Ok(commands) => commands,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut tx = conn.start_transaction(my::TxOpts::default()).await?;
        let mut chunk = Vec::with_capacity(RESTORE_CHUNK_SIZE);
        for command in commands {
            chunk.push(command?);
            if chunk.len() == RESTORE_CHUNK_SIZE {
                insert_commands(&mut tx, &chunk).await?;
                chunk.clear();
            }
        }
        insert_commands(&mut tx, &chunk).await?;
        tx.commit().await?;
        log::info!("Restored archived batch #{batch_id}");
        Ok(true)
    }

    /// Archives all batches that have been DONE/STOP for `archive_months` and
    /// still have rows in `command`, then removes those rows.
    /// Returns the number of batches and commands archived.
    pub async fn sweep(&self, qs: &QuickStatements) -> QsResult<(usize, u64)> {
        let mut conn = qs.get_db_conn().await?;
        conn.query_drop(CREATE_TABLE_SQL).await?;
        let cutoff = cutoff_timestamp(qs.archive_months());
        let sql = "SELECT id FROM batch WHERE `status` IN ('DONE','STOP') AND ts_last_change<:cutoff AND EXISTS (SELECT 1 FROM command WHERE command.batch_id=batch.id) ORDER BY id";
        let batch_ids: Vec<i64> = conn.exec(sql, params! {cutoff}).await?;
        drop(conn);

        let (mut batches, mut commands) = (0, 0);
        for batch_id in batch_ids {
            match self.archive_batch(qs, batch_id).await {
                Ok(n) => {
                    batches += 1;
                    commands += n;
                }
                Err(e) => log::warn!("Batch #{batch_id} not archived: {e}"),
            }
        }
        Ok((batches, commands))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(id: i64, status: &str) -> StoredCommand {
        StoredCommand {
            id,
            batch_id: 1234,
            num: id - 100,
            json: r#"{"action":"add","item":"Q42","text":"ünïcödé\n\"quoted\""}"#.to_string(),
            status: status.to_string(),
            message: String::new(),
            ts_change: "20260101000000".to_string(),
        }
    }

    #[test]
    fn path_is_sharded_by_thousands() {
        let archive = Archive::new("/a");
        assert_eq!(
            archive.path(260144),
            PathBuf::from("/a/260/260144.jsonl.gz")
        );
        assert_eq!(archive.path(7), PathBuf::from("/a/0/7.jsonl.gz"));
    }

    #[test]
    fn write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        let commands = vec![
            command(100, "DONE"),
            command(101, "ERROR"),
            command(102, "DONE"),
        ];

        let mut writer = ArchiveWriter::create(archive.path(1234)).unwrap();
        commands.iter().for_each(|c| writer.push(c).unwrap());
        let counts = writer.finish().unwrap();

        assert_eq!(
            counts,
            StatusCounts::from([("DONE".into(), 2), ("ERROR".into(), 1)])
        );
        assert_eq!(archive.read(1234).unwrap(), Some(commands));
        assert!(!archive.path(1234).with_extension("tmp").exists());
    }

    #[test]
    fn empty_archive_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        let writer = ArchiveWriter::create(archive.path(5)).unwrap();
        assert!(writer.finish().unwrap().is_empty());
        assert_eq!(archive.read(5).unwrap(), None);
        assert!(!archive.path(5).with_extension("tmp").exists());
    }

    #[test]
    fn missing_archive_reads_as_none() {
        let archive = Archive::new("/nonexistent/qs_archive");
        assert_eq!(archive.read(1).unwrap(), None);
    }

    #[test]
    fn cutoff_is_in_the_past() {
        let now = TimeStamp::now();
        let cutoff = cutoff_timestamp(3);
        assert_eq!(cutoff.len(), 14);
        assert!(cutoff < now);
    }
}
