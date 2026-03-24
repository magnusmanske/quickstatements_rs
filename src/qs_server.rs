use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::services::ServeDir;

use crate::qs_config::QuickStatements;
use crate::qs_parser::QuickStatementsParser;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<QuickStatements>,
}

/// Main entry point: build and return the axum Router.
pub fn build_router(config: Arc<QuickStatements>) -> Router {
    let state = AppState { config };

    // API routes
    let api = Router::new()
        .route("/api.php", get(api_handler).post(api_handler_post))
        .route("/config.json", get(serve_config));

    // Combine: API first, then static file serving for everything else
    Router::new()
        .merge(api)
        .fallback_service(ServeDir::new("public_html"))
        .with_state(state)
}

// ---- Query parameter structs ----

#[derive(Deserialize, Default, Debug)]
#[allow(dead_code)]
struct ApiParams {
    action: Option<String>,
    // get_batch_info / start_batch / stop_batch
    batch: Option<String>,
    // get_batches_info
    user: Option<String>,
    limit: Option<String>,
    offset: Option<String>,
    // get_commands_from_batch
    start: Option<String>,
    filter: Option<String>,
    // import
    format: Option<String>,
    data: Option<String>,
    compress: Option<String>,
    persistent: Option<String>,
    temporary: Option<String>,
    site: Option<String>,
    batchname: Option<String>,
    // run_single_command
    command: Option<String>,
    last_item: Option<String>,
    // run_batch
    name: Option<String>,
    commands: Option<String>,
    // get_token
    force_generate: Option<String>,
    // reset_errors
    batch_id: Option<String>,
    // get_batch (by temp id)
    id: Option<String>,
    // JSONP
    callback: Option<String>,
}

/// Serve config.json dynamically from the loaded config
async fn serve_config(State(state): State<AppState>) -> Json<Value> {
    Json(state.config.frontend_config().clone())
}

// GET handler
async fn api_handler(
    State(state): State<AppState>,
    Query(params): Query<ApiParams>,
) -> Response {
    handle_api(state, params).await
}

// POST handler — axum can parse Form or Query; we merge both.
async fn api_handler_post(
    State(state): State<AppState>,
    axum::extract::Form(params): axum::extract::Form<ApiParams>,
) -> Response {
    handle_api(state, params).await
}

async fn handle_api(state: AppState, params: ApiParams) -> Response {
    let action = params.action.as_deref().unwrap_or("");
    let result = match action {
        "is_logged_in" => action_is_logged_in(&params),
        "get_batch_info" => action_get_batch_info(&state, &params).await,
        "get_batches_info" => action_get_batches_info(&state, &params).await,
        "get_commands_from_batch" => action_get_commands_from_batch(&state, &params).await,
        "start_batch" => action_start_batch(&state, &params).await,
        "stop_batch" => action_stop_batch(&state, &params).await,
        "import" => action_import(&params).await,
        "run_batch" => action_run_batch(&state, &params).await,
        "run_single_command" => action_run_single_command(&state, &params).await,
        "get_token" => action_get_token(),
        "reset_errors" => action_reset_errors(&state, &params).await,
        "get_batch" => action_get_batch(&params),
        "oauth_redirect" => return Redirect::to("/").into_response(),
        _ => json!({"status": format!("ERROR: Unknown action '{}'", action)}),
    };

    // JSONP support (only for is_logged_in in practice)
    if let Some(cb) = &params.callback {
        let body = format!("{}({})", cb, result);
        return (
            [(axum::http::header::CONTENT_TYPE, "application/javascript")],
            body,
        )
            .into_response();
    }

    Json(result).into_response()
}

// ---- API action implementations ----

/// `action=is_logged_in`
/// In standalone mode there is no OAuth session, so we report not logged in.
/// The frontend gracefully degrades — batch viewing still works.
fn action_is_logged_in(_params: &ApiParams) -> Value {
    json!({
        "status": "OK",
        "data": {
            "is_logged_in": false
        }
    })
}

/// `action=get_batch_info`
async fn action_get_batch_info(state: &AppState, params: &ApiParams) -> Value {
    let batch_id: i64 = match params.batch.as_deref().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return json!({"status": "ERROR: batch parameter required"}),
    };

    let pool = &state.config;
    // Get batch row
    let batch_row = match get_batch_row(pool, batch_id).await {
        Some(row) => row,
        None => return json!({"status": format!("ERROR: batch {} not found", batch_id)}),
    };

    // Get command status counts
    let counts = get_command_counts(pool, batch_id).await;

    let mut data = HashMap::new();
    let batch_key = batch_id.to_string();
    data.insert(
        batch_key,
        json!({
            "batch": batch_row,
            "commands": counts,
        }),
    );

    json!({"status": "OK", "data": data})
}

/// `action=get_batches_info`
async fn action_get_batches_info(state: &AppState, params: &ApiParams) -> Value {
    let limit: i64 = params
        .limit
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let offset: i64 = params
        .offset
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let pool = &state.config;

    let user_filter = params.user.as_deref().unwrap_or("");

    let batches = get_batches(pool, user_filter, limit, offset).await;

    json!({
        "status": "OK",
        "data": batches,
    })
}

/// `action=get_commands_from_batch`
async fn action_get_commands_from_batch(state: &AppState, params: &ApiParams) -> Value {
    let batch_id: i64 = match params.batch.as_deref().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return json!({"status": "ERROR: batch parameter required"}),
    };
    let start: i64 = params
        .start
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit: i64 = params
        .limit
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let filter = params.filter.as_deref().unwrap_or("");

    let commands = get_commands(
        &state.config,
        batch_id,
        start,
        limit,
        filter,
    )
    .await;

    json!({"status": "OK", "data": commands})
}

/// `action=start_batch`
async fn action_start_batch(state: &AppState, params: &ApiParams) -> Value {
    let batch_id: i64 = match params.batch.as_deref().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return json!({"status": "ERROR: batch parameter required"}),
    };

    match set_batch_status_simple(&state.config, batch_id, "INIT").await {
        true => json!({"status": "OK"}),
        false => json!({"status": "ERROR: Could not start batch"}),
    }
}

/// `action=stop_batch`
async fn action_stop_batch(state: &AppState, params: &ApiParams) -> Value {
    let batch_id: i64 = match params.batch.as_deref().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return json!({"status": "ERROR: batch parameter required"}),
    };

    match set_batch_status_simple(&state.config, batch_id, "STOP").await {
        true => json!({"status": "OK"}),
        false => json!({"status": "ERROR: Could not stop batch"}),
    }
}

/// `action=import`
async fn action_import(params: &ApiParams) -> Value {
    let data = match params.data.as_deref() {
        Some(d) if !d.is_empty() => d,
        _ => return json!({"status": "ERROR: no data provided"}),
    };
    let format = params.format.as_deref().unwrap_or("v1");
    let compress = params.compress.as_deref().unwrap_or("1") != "0";

    if format == "v1" {
        import_v1(data, compress).await
    } else if format == "csv" {
        // CSV import is a pass-through to V1 after converting
        import_csv(data, compress).await
    } else {
        json!({"status": format!("ERROR: Unknown format {}", format)})
    }
}

/// `action=run_batch`
async fn action_run_batch(state: &AppState, params: &ApiParams) -> Value {
    let name = params.name.as_deref().unwrap_or("");
    let site = params
        .site
        .as_deref()
        .unwrap_or(state.config.default_site().unwrap_or("wikidata"));
    let commands_str = match params.commands.as_deref() {
        Some(c) => c,
        None => return json!({"status": "ERROR: commands parameter required"}),
    };
    let commands: Vec<Value> = match serde_json::from_str(commands_str) {
        Ok(c) => c,
        Err(e) => return json!({"status": format!("ERROR: Cannot parse commands JSON: {}", e)}),
    };

    match create_batch(&state.config, name, site, &commands).await {
        Some(batch_id) => json!({"status": "OK", "batch_id": batch_id}),
        None => json!({"status": "ERROR: Could not create batch"}),
    }
}

/// `action=run_single_command`
async fn action_run_single_command(_state: &AppState, _params: &ApiParams) -> Value {
    // This would need OAuth to actually run.
    // For standalone mode, we return an error suggesting background mode.
    json!({"status": "ERROR: Direct command execution requires OAuth authentication. Use background mode instead."})
}

/// `action=get_token`
fn action_get_token() -> Value {
    // No OAuth in standalone mode
    json!({
        "status": "OK",
        "data": {
            "token": "",
            "is_logged_in": false
        }
    })
}

/// `action=reset_errors`
async fn action_reset_errors(state: &AppState, params: &ApiParams) -> Value {
    let batch_id: i64 = match params
        .batch_id
        .as_deref()
        .and_then(|s| s.parse().ok())
    {
        Some(id) => id,
        None => return json!({"status": "ERROR: batch_id parameter required"}),
    };

    let count = reset_error_commands(&state.config, batch_id).await;
    json!({"status": "OK", "init": count})
}

/// `action=get_batch` — load a temporary batch by ID
fn action_get_batch(params: &ApiParams) -> Value {
    let id = params.id.as_deref().unwrap_or("");
    if id.is_empty() {
        return json!({"status": "ERROR: id parameter required"});
    }
    // Try to read the temp file
    let path = format!("public_html/tmp/{}", id);
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(data) => json!({"status": "OK", "id": id, "data": data}),
            Err(_) => json!({"status": "ERROR: invalid tmp file content"}),
        },
        Err(_) => json!({"status": format!("ERROR: tmp file {} not found", id)}),
    }
}

// ---- Database helper functions ----

use mysql_async as my;
use mysql_async::prelude::*;

/// Get a single batch's metadata
async fn get_batch_row(qs: &QuickStatements, batch_id: i64) -> Option<Value> {
    let sql = "SELECT id, `name`, `user`, site, `status`, message, last_item, ts_last_change FROM batch WHERE id=:batch_id";
    let mut conn = qs.get_db_conn().await.ok()?;
    let rows: Vec<(i64, String, i64, String, String, String, String, String)> = conn
        .exec(sql, my::params! {batch_id})
        .await
        .ok()?;
    let row = rows.first()?;

    let user_name = qs.get_user_name(row.2).await.unwrap_or_default();

    Some(json!({
        "id": row.0,
        "name": row.1,
        "user": user_name,
        "site": row.3,
        "status": row.4,
        "message": row.5,
        "last_item": row.6,
        "ts_last_change": row.7,
    }))
}

/// Get command status counts for a batch
async fn get_command_counts(qs: &QuickStatements, batch_id: i64) -> Value {
    let sql = "SELECT `status`, COUNT(*) AS cnt FROM command WHERE batch_id=:batch_id GROUP BY `status`";
    let mut counts = json!({
        "INIT": 0, "RUN": 0, "DONE": 0, "ERROR": 0, "BLOCKED": 0, "STOP": 0
    });

    if let Ok(mut conn) = qs.get_db_conn().await {
        if let Ok(rows) = conn
            .exec::<(String, i64), _, _>(sql, my::params! {batch_id})
            .await
        {
            for (status, cnt) in rows {
                counts[&status] = json!(cnt);
            }
        }
    }
    counts
}

/// Get list of batches, optionally filtered by user
async fn get_batches(
    qs: &QuickStatements,
    user_filter: &str,
    limit: i64,
    offset: i64,
) -> Value {
    let mut conn = match qs.get_db_conn().await {
        Ok(c) => c,
        Err(_) => return json!({}),
    };

    let (sql, query_params) = if user_filter.is_empty() {
        let sql = "SELECT b.id, b.`name`, b.`user`, b.site, b.`status`, b.message, b.last_item, b.ts_last_change FROM batch b ORDER BY b.id DESC LIMIT :limit OFFSET :offset";
        (sql.to_string(), my::params! { limit, offset })
    } else {
        // Resolve user name to user id
        let auth_db = "s53220__quickstatements_auth";
        let sql = format!(
            "SELECT b.id, b.`name`, b.`user`, b.site, b.`status`, b.message, b.last_item, b.ts_last_change FROM batch b WHERE b.`user` IN (SELECT user_id FROM {}.user WHERE name=:user_filter) ORDER BY b.id DESC LIMIT :limit OFFSET :offset",
            auth_db
        );
        (sql, my::params! { user_filter, limit, offset })
    };

    let rows: Vec<(i64, String, i64, String, String, String, String, String)> =
        match conn.exec(&sql, query_params).await {
            Ok(r) => r,
            Err(_) => return json!({}),
        };

    let mut result = json!({});
    for row in &rows {
        let user_name = qs.get_user_name(row.2).await.unwrap_or_default();
        let batch_id = row.0;
        let counts = get_command_counts(qs, batch_id).await;

        result[batch_id.to_string()] = json!({
            "batch": {
                "id": row.0,
                "name": row.1,
                "user": user_name,
                "site": row.3,
                "status": row.4,
                "message": row.5,
                "last_item": row.6,
                "ts_last_change": row.7,
            },
            "commands": counts,
        });
    }
    result
}

/// Get commands from a batch with pagination and optional status filter
async fn get_commands(
    qs: &QuickStatements,
    batch_id: i64,
    start: i64,
    limit: i64,
    filter: &str,
) -> Value {
    let mut conn = match qs.get_db_conn().await {
        Ok(c) => c,
        Err(_) => return json!([]),
    };

    let filter_statuses: Vec<&str> = if filter.is_empty() {
        vec![]
    } else {
        filter.split(',').map(|s| s.trim()).collect()
    };

    let mut sql = format!(
        "SELECT id, batch_id, num, json, `status`, message, ts_change FROM command WHERE batch_id={}",
        batch_id
    );

    if !filter_statuses.is_empty() {
        let quoted: Vec<String> = filter_statuses
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "")))
            .collect();
        sql += &format!(" AND `status` IN ({})", quoted.join(","));
    }

    sql += " ORDER BY num";

    if limit > 0 {
        sql += &format!(" LIMIT {}", limit);
    }
    if start > 0 {
        sql += &format!(" OFFSET {}", start);
    }

    let rows: Vec<(i64, i64, i64, String, String, String, String)> =
        match conn.exec(&sql, ()).await {
            Ok(r) => r,
            Err(_) => return json!([]),
        };

    let commands: Vec<Value> = rows
        .iter()
        .map(|row| {
            let cmd_json: Value =
                serde_json::from_str(&row.3).unwrap_or(json!({}));
            json!({
                "id": row.0,
                "batch_id": row.1,
                "num": row.2,
                "json": cmd_json,
                "status": row.4,
                "message": row.5,
                "ts_change": row.6,
            })
        })
        .collect();

    json!(commands)
}

/// Set batch status (for start/stop)
async fn set_batch_status_simple(qs: &QuickStatements, batch_id: i64, status: &str) -> bool {
    let ts = qs.timestamp();
    let sql = r#"UPDATE `batch` SET `status`=:status, `ts_last_change`=:ts WHERE id=:batch_id"#;
    match qs.get_db_conn().await {
        Ok(mut conn) => conn
            .exec_drop(sql, my::params! {status, ts, batch_id})
            .await
            .is_ok(),
        Err(_) => false,
    }
}

/// Create a new batch in the database and insert commands
async fn create_batch(
    qs: &QuickStatements,
    name: &str,
    site: &str,
    commands: &[Value],
) -> Option<i64> {
    let mut conn = qs.get_db_conn().await.ok()?;
    let ts = qs.timestamp();
    let user: i64 = 0; // No OAuth user in standalone mode

    // Insert batch
    let ts_batch = ts.clone();
    conn.exec_drop(
        "INSERT INTO batch (`name`, `user`, site, `status`, ts_last_change) VALUES (:name, :user, :site, 'INIT', :ts_batch)",
        my::params! {name, user, site, ts_batch},
    )
    .await
    .ok()?;

    let batch_id: i64 = conn
        .exec_first("SELECT LAST_INSERT_ID()", ())
        .await
        .ok()??;

    // Insert commands
    for (num, cmd) in commands.iter().enumerate() {
        let json_str = serde_json::to_string(cmd).unwrap_or_else(|_| "{}".to_string());
        let num = num as i64;
        let ts_cmd = ts.clone();
        conn.exec_drop(
            "INSERT INTO command (batch_id, num, json, `status`, ts_change) VALUES (:batch_id, :num, :json_str, 'INIT', :ts_cmd)",
            my::params! {batch_id, num, json_str, ts_cmd},
        )
        .await
        .ok()?;
    }

    Some(batch_id)
}

/// Reset ERROR commands back to INIT
async fn reset_error_commands(qs: &QuickStatements, batch_id: i64) -> i64 {
    let ts = qs.timestamp();
    let sql = r#"UPDATE command SET `status`='INIT', message='', ts_change=:ts WHERE batch_id=:batch_id AND `status`='ERROR'"#;
    match qs.get_db_conn().await {
        Ok(mut conn) => {
            match conn.exec_iter(sql, my::params! {ts, batch_id}).await {
                Ok(result) => result.affected_rows() as i64,
                Err(_) => 0,
            }
        }
        Err(_) => 0,
    }
}

// ---- V1 / CSV import helpers ----

async fn import_v1(data: &str, compress: bool) -> Value {
    let lines: Vec<&str> = if data.contains('\n') {
        data.split('\n').collect()
    } else {
        data.split("||").collect()
    };

    let mut parsers = vec![];
    let mut errors = vec![];
    for line in &lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match QuickStatementsParser::new_from_line(line, None).await {
            Ok(p) => parsers.push(p),
            Err(e) => errors.push(json!({"error": e, "line": line})),
        }
    }

    if compress {
        QuickStatementsParser::compress(&mut parsers);
    }

    let commands: Vec<Value> = parsers
        .iter()
        .flat_map(|p| p.to_json().unwrap_or_default())
        .collect();

    let mut result = json!({
        "status": "OK",
        "data": {
            "commands": commands
        }
    });
    if !errors.is_empty() {
        result["errors"] = json!(errors);
    }
    result
}

async fn import_csv(data: &str, compress: bool) -> Value {
    // CSV: first line is header, remaining lines are data rows
    // Convert to V1 tab-separated format
    let mut lines: Vec<&str> = data.split('\n').collect();
    if lines.is_empty() {
        return json!({"status": "ERROR: Empty CSV data"});
    }

    let header_line = lines.remove(0);
    let _headers: Vec<&str> = header_line.split(',').map(|s| s.trim()).collect();

    let mut v1_lines = vec![];
    for line in &lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        let v1_line = cols.join("\t");
        v1_lines.push(v1_line);
    }

    import_v1(&v1_lines.join("\n"), compress).await
}
