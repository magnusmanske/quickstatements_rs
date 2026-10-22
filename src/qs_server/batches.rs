//! Database access of the web API: batches, their commands, and API tokens.
//! All `command` queries are scoped by batch, as the table is huge.

use super::random_string;
use crate::archive::{has_live_rows, StoredCommand, COLUMNS};
use crate::error::QsResult;
use crate::qs_config::{CommandRow, QuickStatements};
use serde_json::{json, Map, Value};
use wikimisc::mysql_async::prelude::*;
use wikimisc::mysql_async::{self as my, TxOpts};

/// Commands per multi-row INSERT/UPDATE
const CHUNK_SIZE: usize = 500;

/// (id, name, user, site, status, message, last_item, ts_created, ts_last_change, user name)
type BatchRow = (
    i64,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

/// A batch and its owner
pub struct Batch {
    pub id: i64,
    pub user: i64,
}

pub async fn batch(qs: &QuickStatements, id: i64) -> QsResult<Option<Batch>> {
    let mut conn = qs.get_db_conn().await?;
    let user: Option<i64> = conn
        .exec_first("SELECT user FROM batch WHERE id=:id", params! {id})
        .await?;
    Ok(user.map(|user| Batch { id, user }))
}

/// Batches with their number of commands per status, keyed by batch ID
/// (PHP's `getBatchStatus`)
pub async fn batch_status(qs: &QuickStatements, ids: &[i64]) -> QsResult<Map<String, Value>> {
    let mut ret = Map::new();
    if ids.is_empty() {
        return Ok(ret);
    }
    // IDs are integers, safe to put into SQL
    let ids = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut conn = qs.get_db_conn().await?;
    let sql = format!(
        "SELECT b.id,b.name,b.user,b.site,b.status,b.message,b.last_item,b.ts_created,b.ts_last_change,u.name FROM batch b LEFT JOIN `{}`.user u ON u.id=b.user WHERE b.id IN ({ids})",
        qs.auth_db()
    );
    let rows: Vec<BatchRow> = conn.query(sql).await?;
    for row in rows {
        let batch = json!({
            "id": row.0, "name": row.1, "user": row.2, "site": row.3, "status": row.4,
            "message": row.5, "last_item": row.6, "ts_created": row.7, "ts_last_change": row.8,
            "user_name": row.9.unwrap_or_default(),
        });
        ret.insert(row.0.to_string(), json!({"batch": batch, "commands": {}}));
    }

    let sql = format!("SELECT batch_id,`status`,COUNT(*) FROM command WHERE batch_id IN ({ids}) GROUP BY batch_id,`status`");
    let counts: Vec<(i64, String, u64)> = conn.query(sql).await?;
    for (id, status, count) in counts {
        if let Some(batch) = ret.get_mut(&id.to_string()) {
            batch["commands"][status] = json!(count);
        }
    }

    // Archived batches have no rows in `command`
    if qs.archive().is_some() {
        let sql =
            format!("SELECT batch_id,status_counts FROM command_archive WHERE batch_id IN ({ids})");
        let archived: Vec<(i64, String)> = conn.query(sql).await?;
        for (id, counts) in archived {
            if let Some(batch) = ret.get_mut(&id.to_string()) {
                if batch["commands"].as_object().is_some_and(Map::is_empty) {
                    batch["commands"] = serde_json::from_str(&counts)?;
                }
            }
        }
    }
    Ok(ret)
}

/// The most recently changed batches, of one user if `user_name` is given
pub async fn latest_batch_ids(
    qs: &QuickStatements,
    user_name: &str,
    limit: u64,
    offset: u64,
) -> QsResult<Vec<i64>> {
    let mut conn = qs.get_db_conn().await?;
    let user_name = user_name.replace('_', " ").trim().to_string();
    if user_name.is_empty() {
        let sql = "SELECT id FROM batch ORDER BY ts_last_change DESC LIMIT :limit OFFSET :offset";
        return Ok(conn.exec(sql, params! {limit, offset}).await?);
    }
    let sql = format!(
        "SELECT b.id FROM batch b JOIN `{}`.user u ON u.id=b.user WHERE u.name=:user_name ORDER BY b.ts_last_change DESC LIMIT :limit OFFSET :offset",
        qs.auth_db()
    );
    Ok(conn.exec(sql, params! {user_name, limit, offset}).await?)
}

/// The commands of a batch from number `start` on, by number; only those with
/// one of `statuses`, if any. Read from the archive if the batch has no rows.
pub async fn commands(
    qs: &QuickStatements,
    batch_id: i64,
    start: i64,
    limit: Option<u64>,
    statuses: &[String],
) -> QsResult<Vec<StoredCommand>> {
    let mut sql = format!("SELECT {COLUMNS} FROM command WHERE batch_id=? AND num>=?");
    let mut params: Vec<my::Value> = vec![batch_id.into(), start.into()];
    if !statuses.is_empty() {
        let placeholders = vec!["?"; statuses.len()].join(",");
        sql += &format!(" AND `status` IN ({placeholders})");
        params.extend(statuses.iter().map(|s| s.as_str().into()));
    }
    sql += " ORDER BY num";
    if let Some(limit) = limit {
        sql += " LIMIT ?";
        params.push(limit.into());
    }
    let mut conn = qs.get_db_conn().await?;
    let rows: Vec<CommandRow> = conn.exec(sql, params).await?;
    if !rows.is_empty() {
        return Ok(rows.into_iter().map(StoredCommand::from).collect());
    }
    // Only for batches without rows: a restored batch keeps its archive file
    let Some(archive) = qs.archive() else {
        return Ok(vec![]);
    };
    if has_live_rows(&mut conn, batch_id).await? {
        return Ok(vec![]);
    }
    let statuses = statuses.to_vec();
    let archived = tokio::task::spawn_blocking(move || {
        archive.read_filtered(batch_id, |c| in_page(c, start, &statuses))
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(first_by_number(archived.unwrap_or_default(), limit))
}

/// Whether the command is selected by the SQL query in `commands`
fn in_page(command: &StoredCommand, start: i64, statuses: &[String]) -> bool {
    command.num >= start && (statuses.is_empty() || statuses.contains(&command.status))
}

fn first_by_number(mut commands: Vec<StoredCommand>, limit: Option<u64>) -> Vec<StoredCommand> {
    commands.sort_by_key(|c| c.num);
    commands.truncate(limit.map_or(usize::MAX, |l| l as usize));
    commands
}

/// A command as the API returns it, with its JSON decoded
pub fn command_json(command: &StoredCommand) -> Value {
    json!({
        "id": command.id,
        "batch_id": command.batch_id,
        "num": command.num,
        "json": serde_json::from_str::<Value>(&command.json).unwrap_or(json!({})),
        "status": command.status,
        "message": command.message,
        "ts_change": command.ts_change,
    })
}

/// Stores a new batch with its commands and the OAuth credentials the bot
/// edits with, and returns its ID. Commands keep a `status` they already
/// have (e.g. from running in the browser).
pub async fn create_batch(
    qs: &QuickStatements,
    commands: &[Value],
    user_id: i64,
    name: &str,
    site: &str,
    credentials: &Value,
) -> QsResult<i64> {
    let ts = qs.timestamp();
    let mut conn = qs.get_db_conn().await?;
    // The bot cannot see the batch until it is complete
    let mut tx = conn.start_transaction(TxOpts::default()).await?;
    tx.exec_drop(
        "INSERT INTO batch (name,user,site,ts_created,ts_last_change,status,message) VALUES (:name,:user_id,:site,:ts,:ts,'INIT','')",
        params! {name, user_id, site, "ts" => ts.as_str()},
    )
    .await?;
    let batch_id = tx.last_insert_id().ok_or("No ID for the new batch")? as i64;
    let sql = format!(
        "INSERT INTO `{}`.batch_oauth (batch_id,serialized,serialized_json) VALUES (:batch_id,'',:json)",
        qs.auth_db()
    );
    tx.exec_drop(sql, params! {batch_id, "json" => credentials.to_string()})
        .await?;
    for (chunk_number, chunk) in commands.chunks(CHUNK_SIZE).enumerate() {
        let placeholders = vec!["(?,?,?,?,?,'')"; chunk.len()].join(",");
        let sql = format!("INSERT INTO command (batch_id,num,json,`status`,ts_change,message) VALUES {placeholders}");
        let params: Vec<my::Value> = chunk
            .iter()
            .enumerate()
            .flat_map(|(i, command)| {
                let num = (chunk_number * CHUNK_SIZE + i) as i64;
                [
                    batch_id.into(),
                    num.into(),
                    command.to_string().into(),
                    initial_status(command).into(),
                    ts.as_str().into(),
                ]
            })
            .collect();
        tx.exec_drop(sql, params).await?;
    }
    tx.commit().await?;
    Ok(batch_id)
}

fn initial_status(command: &Value) -> String {
    match command["status"].as_str().map(str::trim) {
        Some(status) if !status.is_empty() => status.to_uppercase(),
        _ => "INIT".to_string(),
    }
}

/// Puts the failed commands of a batch back into INIT, except those that
/// cannot succeed on a retry, and returns their number
pub async fn reset_errors(qs: &QuickStatements, batch_id: i64) -> QsResult<usize> {
    let mut conn = qs.get_db_conn().await?;
    let sql = "SELECT id,json,message FROM command WHERE batch_id=:batch_id AND `status`='ERROR'";
    let rows: Vec<(i64, String, String)> = conn.exec(sql, params! {batch_id}).await?;
    let ids: Vec<String> = rows
        .into_iter()
        .filter(|(_, json, message)| is_retryable(json, message))
        .map(|(id, ..)| id.to_string())
        .collect();
    for chunk in ids.chunks(CHUNK_SIZE) {
        let sql = format!(
            "UPDATE command SET `status`='INIT',message='' WHERE batch_id=:batch_id AND id IN ({})",
            chunk.join(",")
        );
        conn.exec_drop(sql, params! {batch_id}).await?;
    }
    Ok(ids.len())
}

fn is_retryable(json: &str, message: &str) -> bool {
    if message.contains("no-such-entity") {
        return false; // The entity does not exist
    }
    match serde_json::from_str::<Value>(json) {
        // With LAST, it is unknown which item the command was for
        Ok(command) => command.is_object() && command["item"] != "LAST",
        Err(_) => false,
    }
}

/// Adds the user, or updates their name after a rename. A changed name
/// resets the API token: PHP gave out IDs that may now be another user's.
/// (Assignments run in order, so `api_hash` is reset before the name changes.)
pub async fn save_user(qs: &QuickStatements, id: i64, name: &str) -> QsResult<()> {
    let sql = format!(
        "INSERT INTO `{}`.user (id,name) VALUES (:id,:name) ON DUPLICATE KEY UPDATE api_hash=IF(name=VALUES(name),api_hash,''),name=VALUES(name)",
        qs.auth_db()
    );
    qs.get_db_conn()
        .await?
        .exec_drop(sql, params! {id, name})
        .await?;
    Ok(())
}

/// The user's token for submitting batches through the API; a new one if
/// they have none yet, or `renew` is set
pub async fn api_token(qs: &QuickStatements, user_id: i64, renew: bool) -> QsResult<String> {
    let mut conn = qs.get_db_conn().await?;
    if !renew {
        let sql = format!(
            "SELECT api_hash FROM `{}`.user WHERE id=:user_id",
            qs.auth_db()
        );
        let token: Option<String> = conn.exec_first(sql, params! {user_id}).await?;
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            return Ok(token);
        }
    }
    let token = random_string(48);
    let sql = format!(
        "UPDATE `{}`.user SET api_hash=:token WHERE id=:user_id",
        qs.auth_db()
    );
    conn.exec_drop(sql, params! {"token" => token.as_str(), user_id})
        .await?;
    Ok(token)
}

/// The user with that name and API token
pub async fn user_with_token(
    qs: &QuickStatements,
    name: &str,
    token: &str,
) -> QsResult<Option<i64>> {
    if token.is_empty() {
        return Ok(None);
    }
    let name = name.replace('_', " ").trim().to_string();
    let sql = format!(
        "SELECT id FROM `{}`.user WHERE name=:name AND api_hash=:token LIMIT 1",
        qs.auth_db()
    );
    let mut conn = qs.get_db_conn().await?;
    Ok(conn.exec_first(sql, params! {name, token}).await?)
}

/// The OAuth credentials of the user's latest batch, to submit a batch with
/// an API token (PHP's `fillOA`)
pub async fn latest_credentials(qs: &QuickStatements, user_id: i64) -> QsResult<Option<Value>> {
    let sql = format!(
        "SELECT serialized_json FROM `{}`.batch_oauth WHERE batch_id=(SELECT MAX(id) FROM batch WHERE user=:user_id) ORDER BY id DESC LIMIT 1",
        qs.auth_db()
    );
    let json: Option<String> = qs
        .get_db_conn()
        .await?
        .exec_first(sql, params! {user_id})
        .await?;
    Ok(json
        .and_then(|j| serde_json::from_str::<Value>(&j).ok())
        .filter(|j| j["gTokenKey"].is_string() && j["gTokenSecret"].is_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(num: i64, status: &str) -> StoredCommand {
        StoredCommand {
            id: num + 100,
            batch_id: 1,
            num,
            json: "{}".to_string(),
            status: status.to_string(),
            message: String::new(),
            ts_change: String::new(),
        }
    }

    #[test]
    fn archived_pages_filter_sort_and_limit() {
        let commands = [
            command(3, "DONE"),
            command(0, "ERROR"),
            command(2, "ERROR"),
            command(1, "DONE"),
        ];
        let page = |start, limit, statuses: &[String]| {
            let selected = commands
                .iter()
                .filter(|c| in_page(c, start, statuses))
                .cloned()
                .collect();
            first_by_number(selected, limit)
                .iter()
                .map(|c| c.num)
                .collect::<Vec<_>>()
        };
        assert_eq!(page(0, None, &[]), [0, 1, 2, 3]);
        assert_eq!(page(1, Some(2), &[]), [1, 2]);
        assert_eq!(page(2, None, &["DONE".to_string()]), [3]);
    }

    #[test]
    fn initial_status_keeps_browser_status() {
        assert_eq!(initial_status(&json!({"status":"done"})), "DONE");
        assert_eq!(initial_status(&json!({"status":" "})), "INIT");
        assert_eq!(initial_status(&json!({})), "INIT");
    }

    #[test]
    fn retryable_errors() {
        assert!(is_retryable(r#"{"item":"Q1"}"#, "some error"));
        assert!(!is_retryable(r#"{"item":"Q1"}"#, "no-such-entity"));
        assert!(!is_retryable(r#"{"item":"LAST"}"#, ""));
        assert!(!is_retryable("not json", ""));
    }

    #[test]
    fn command_json_decodes_json() {
        let mut c = command(1, "DONE");
        c.json = r#"{"action":"create"}"#.to_string();
        assert_eq!(command_json(&c)["json"]["action"], "create");
    }
}
