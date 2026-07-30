use crate::error::{QsError, QsResult};
use crate::qs_command::QuickStatementsCommand;
use chrono::prelude::Utc;
use config::*;
use log;
use mysql_async as my;
use mysql_async::from_row;
use mysql_async::prelude::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::sync::Arc;
use tokio::sync::RwLock;
use wikibase::mediawiki::Api;

/// Row layout of the `command` table: (id, batch_id, num, json, status, message, ts_change)
type CommandRow = (i64, i64, i64, String, String, String, String);

#[derive(Debug, Clone)]
pub struct QuickStatements {
    params: Value,
    pool: my::Pool,
    running_batch_ids: Arc<RwLock<HashSet<i64>>>,
    user_counter: Arc<RwLock<HashMap<i64, i64>>>,
    max_batches_per_user: i64,
    verbose: bool,
}

impl QuickStatements {
    pub fn new_from_config_json(filename: &str) -> Option<Self> {
        let file = File::open(filename).ok()?;
        let mut params: Value = serde_json::from_reader(file).ok()?;

        // Load the PHP/JS config into params as ["config"], or create empty object
        params["config"] = match params["config_file"].as_str() {
            Some(filename) => match File::open(filename) {
                Ok(file) => serde_json::from_reader(file).unwrap_or(json!({})),
                Err(_) => {
                    eprintln!(
                        "Warning: could not open config_file '{}', using empty config",
                        filename
                    );
                    json!({})
                }
            },
            None => json!({}),
        };

        let max_batches_per_user = params["max_batches_per_user"].as_i64().unwrap_or(2);
        let ret = Self {
            pool: Self::create_mysql_pool(&params),
            params,
            running_batch_ids: Arc::new(RwLock::new(HashSet::new())),
            user_counter: Arc::new(RwLock::new(HashMap::new())),
            max_batches_per_user,
            verbose: false,
        };
        Some(ret)
    }

    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    pub fn get_api_for_site(&self, site: &str) -> Option<&str> {
        self.params["config"]["sites"][site]["api"].as_str()
    }

    /// Returns the default site name from the config
    pub fn default_site(&self) -> Option<&str> {
        self.params["config"]["site"].as_str()
    }

    /// Returns the full config JSON (for serving config.json to the frontend)
    pub fn frontend_config(&self) -> &Value {
        &self.params["config"]
    }

    /// Get a database connection from the pool
    pub async fn get_db_conn(&self) -> QsResult<my::Conn> {
        Ok(self.pool.get_conn().await?)
    }

    /// Lightweight check: can we reach the database at all?
    pub async fn db_ping(&self) -> bool {
        match self.pool.get_conn().await {
            Ok(mut conn) => conn.query_drop("SELECT 1").await.is_ok(),
            Err(_) => false,
        }
    }

    pub fn edit_delay_ms(&self) -> Option<u64> {
        Some(self.params["edit_delay_ms"].as_u64().unwrap_or(1000))
    }

    pub fn maxlag_s(&self) -> Option<u64> {
        Some(self.params["set_maxlag"].as_u64().unwrap_or(5))
    }

    /// Returns the site of a batch, or `Ok(None)` if the batch has no (usable) site.
    /// DB errors are propagated, so callers can tell "no site" from "DB down".
    pub async fn get_site_from_batch(&self, batch_id: i64) -> QsResult<Option<String>> {
        let sql = r#"SELECT site FROM batch WHERE id=:batch_id"#;
        let rows = self
            .pool
            .get_conn()
            .await?
            .exec_iter(sql, params! {batch_id})
            .await?
            .map_and_drop(from_row::<Option<String>>)
            .await?;
        Ok(rows.first().cloned().flatten().filter(|s| !s.is_empty()))
    }

    pub async fn number_of_bots_running(&self) -> usize {
        self.running_batch_ids.read().await.len()
    }

    pub fn timestamp(&self) -> String {
        let now = Utc::now();
        now.format("%Y%m%d%H%M%S").to_string()
    }

    pub async fn restart_batch(&self, batch_id: i64) -> Option<()> {
        let mut conn = self.pool.get_conn().await.ok()?;
        let ts = self.timestamp();
        // Only (re)start batches that are still INIT or RUN, so a user STOP issued
        // between batch selection and this update is not overwritten.
        conn.exec_drop(r#"UPDATE `batch` SET `status`="RUN",`message`="",`ts_last_change`=:ts WHERE id=:batch_id AND `status` IN ("INIT","RUN")"#, params!{ts,batch_id}).await.ok()?;
        let ts = self.timestamp();
        conn.exec_drop(r#"UPDATE `command` SET `status`="INIT",`message`="",`ts_change`=:ts WHERE `status` IN ("RUN","BLOCKED") AND `batch_id`=:batch_id"#, params!{ts,batch_id}).await.ok()
    }

    pub async fn reset_all_running_batches(&self) -> QsResult<()> {
        let mut conn = self.pool.get_conn().await?;
        let ts = self.timestamp();
        conn.exec_drop(r#"UPDATE `batch` SET `status`="INIT",`message`="",`ts_last_change`=:ts WHERE `status`="RUN""#, params!{ts}).await?;
        Ok(())
    }

    pub async fn get_api_url(&self, batch_id: i64) -> Option<&str> {
        let site: String = match self.get_site_from_batch(batch_id).await {
            Ok(Some(site)) => site,
            // No/empty site set for this batch: use the configured default
            Ok(None) => match self.params["config"]["site"].as_str() {
                Some(s) => s.to_string(),
                None => return None,
            },
            // A DB error must not fall back to the default site — that could
            // run the batch against the wrong wiki
            Err(e) => {
                log::error!("get_api_url: cannot get site for batch #{}: {}", batch_id, e);
                return None;
            }
        };
        self.get_api_for_site(&site)
    }

    pub async fn is_user_blocked(mw_api: &mut Api, user_name: &str) -> QsResult<bool> {
        let params: HashMap<String, String> = [
            ("action", "query"),
            ("list", "users"),
            ("ususers", user_name),
            ("usprop", "blockinfo"),
            ("format", "json"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let res = mw_api.post_query_api_json_mut(&params).await?;
        Ok(res["query"]["users"][0]["blockid"].is_number())
    }

    fn create_mysql_pool(params: &Value) -> my::Pool {
        if !params["mysql"].is_object() {
            panic!("QuickStatementsConfig::create_mysql_pool: No mysql info in params");
        }
        let port = params["mysql"]["port"].as_u64().unwrap_or(3306) as u16;
        let host = params["mysql"]["host"].as_str().expect("No host");
        let schema = params["mysql"]["schema"].as_str().expect("No schema");
        let user = params["mysql"]["user"].as_str().expect("No user");
        let pass = params["mysql"]["pass"].as_str().expect("No pass");
        let opts = my::OptsBuilder::default()
            .ip_or_hostname(host)
            .db_name(Some(schema))
            .user(Some(user))
            .pass(Some(pass))
            .tcp_port(port);

        mysql_async::Pool::new(opts)
    }

    pub async fn get_last_item_from_batch(&self, batch_id: i64) -> Option<String> {
        let sql = r#"SELECT last_item FROM batch WHERE `id`=:batch_id"#;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_iter(sql, params! {batch_id})
            .await
            .ok()?
            .map_and_drop(from_row::<String>)
            .await
            .ok()?
            .first()
            .cloned()
    }

    /// Decode the raw DB value into a LastEntityState. Uses pipe-delimited format
    /// when LAST_FORM / LAST_SENSE are present, plain value otherwise (backward compatible).
    pub async fn get_last_state_from_batch(
        &self,
        batch_id: i64,
    ) -> crate::qs_command::LastEntityState {
        match self.get_last_item_from_batch(batch_id).await {
            Some(stored) => crate::qs_command::LastEntityState::decode(&stored),
            None => crate::qs_command::LastEntityState::default(),
        }
    }

    pub async fn get_next_batch(&self) -> Option<(i64, i64)> {
        let batches = self.get_next_batches().await;
        batches.into_iter().next()
    }

    /// Returns all batches that can be started right now, respecting per-user limits.
    pub async fn get_next_batches(&self) -> Vec<(i64, i64)> {
        let sql =
            "SELECT id,user FROM batch WHERE `status` IN ('INIT','RUN') ORDER BY `ts_last_change`";

        let results: Vec<(i64, i64)> = match self.pool.get_conn().await {
            Ok(mut conn) => match conn.exec_iter(sql, ()).await {
                Ok(result) => result
                    .map_and_drop(from_row::<(i64, i64)>)
                    .await
                    .unwrap_or_default(),
                Err(e) => {
                    log::error!("get_next_batches: query failed: {}", e);
                    return vec![];
                }
            },
            Err(e) => {
                log::error!("get_next_batches: DB connection failed: {}", e);
                return vec![];
            }
        };

        let running = self.running_batch_ids.read().await;
        let mut user_counts: HashMap<i64, i64> = self.user_counter.read().await.clone();
        let mut ret = vec![];
        for (id, user_id) in results {
            if running.contains(&id) {
                continue;
            }
            let cnt = user_counts.entry(user_id).or_insert(0);
            if *cnt >= self.max_batches_per_user {
                continue;
            }
            *cnt += 1;
            ret.push((id, user_id));
        }
        ret
    }

    pub async fn reinitialize_open_batches(&self) -> Option<()> {
        let sql = "UPDATE batch SET status='INIT' WHERE status='DONE' AND id IN (SELECT DISTINCT batch_id FROM command WHERE status='INIT' and batch_id>12000)" ;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_drop(sql, ())
            .await
            .ok()
    }

    pub async fn set_batch_running(&self, batch_id: i64, user_id: i64) {
        log::info!("Starting batch #{} for user {}", batch_id, user_id);

        if self.reinitialize_open_batches().await.is_none() {
            log::warn!(
                "Failed to reinitialize open batches for batch #{}",
                batch_id
            );
        }

        // Increase user batch counter. The read-modify-write must happen under a
        // single write lock, or concurrent (de)activations lose updates.
        self.running_batch_ids.write().await.insert(batch_id);
        *self
            .user_counter
            .write()
            .await
            .entry(user_id)
            .or_insert(0) += 1;

        log::info!(
            "Currently {} bots running",
            self.number_of_bots_running().await
        );
    }

    /// Removes a batch from the running set and frees its per-user slot.
    /// Idempotent: only adjusts the user counter if the batch was actually running,
    /// so multiple deactivations (e.g. BLOCKED then STOP) can't leak slots.
    pub async fn deactivate_batch_run(&self, batch_id: i64, user_id: i64) -> Option<()> {
        if !self.running_batch_ids.write().await.remove(&batch_id) {
            return Some(());
        }
        // Read-modify-write under a single write lock, or concurrent
        // deactivations lose updates and leak user slots.
        {
            let mut user_counter = self.user_counter.write().await;
            let count = user_counter.entry(user_id).or_insert(0);
            *count = (*count - 1).max(0);
        }
        log::info!(
            "Currently {} bots running",
            self.number_of_bots_running().await
        );
        Some(())
    }

    pub async fn set_batch_finished(&self, batch_id: i64, user_id: i64) -> Option<()> {
        log::info!("Batch #{} finished", batch_id);
        self.set_batch_status("DONE", "", batch_id, user_id).await
    }

    pub async fn check_batch_not_stopped(&self, batch_id: i64) -> QsResult<()> {
        let sql = r#"SELECT id FROM batch WHERE id=:batch_id AND `status` NOT IN ('RUN','INIT')"#;

        let results = self
            .pool
            .get_conn()
            .await?
            .exec_iter(sql, params! {batch_id})
            .await?
            .map_and_drop(from_row::<usize>)
            .await?;
        if results.is_empty() {
            Ok(())
        } else {
            Err(QsError::BatchStatusError(batch_id))
        }
    }

    pub async fn set_batch_status(
        &self,
        status: &str,
        message: &str,
        batch_id: i64,
        user_id: i64,
    ) -> Option<()> {
        // Free the in-memory slot first, so a failing DB update cannot leak it
        self.deactivate_batch_run(batch_id, user_id).await;
        let ts = self.timestamp();
        let sql = r#"UPDATE `batch` SET `status`=:status,`message`=:message,`ts_last_change`=:ts WHERE id=:batch_id"#;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_drop(sql, params! {status,message,ts,batch_id})
            .await
            .ok()
    }

    /// Runs a query against the `command` table and returns the first result.
    /// DB errors are propagated, so callers can tell "no command" from "DB down".
    async fn fetch_first_command(
        &self,
        sql: &str,
        params: my::Params,
    ) -> QsResult<Option<QuickStatementsCommand>> {
        let rows = self
            .pool
            .get_conn()
            .await?
            .exec_iter(sql, params)
            .await?
            .map_and_drop(from_row::<CommandRow>)
            .await?;
        Ok(rows.first().map(QuickStatementsCommand::from_row))
    }

    pub async fn get_command_by_id(
        &self,
        command_id: i64,
    ) -> QsResult<Option<QuickStatementsCommand>> {
        let sql = r#"SELECT id,batch_id,num,json,`status`,message,ts_change FROM command WHERE id=:command_id"#;
        self.fetch_first_command(sql, params! {command_id}).await
    }

    pub async fn get_next_command(
        &self,
        batch_id: i64,
    ) -> QsResult<Option<QuickStatementsCommand>> {
        let sql = r#"SELECT id,batch_id,num,json,`status`,message,ts_change FROM command WHERE batch_id=:batch_id AND status IN ('INIT') ORDER BY num LIMIT 1"#;
        self.fetch_first_command(sql, params! {batch_id}).await
    }

    pub async fn set_command_status(
        &self,
        command: &mut QuickStatementsCommand,
        new_status: &str,
        new_message: Option<String>,
    ) -> Option<()> {
        let status = new_status.trim().to_uppercase();
        let message = new_message.as_deref().unwrap_or("");

        // Keep the in-memory struct fields in sync with the JSON blob.
        command.status = status.clone();
        command.message = message.to_string();
        command.json["meta"]["status"] = json!(&status);
        command.json["meta"]["message"] = json!(message);

        let json = serde_json::to_string(&command.json).unwrap_or_else(|_| "{}".to_string());

        let command_id = command.id;
        let ts = self.timestamp();
        let sql = r#"UPDATE `command` SET `ts_change`=:ts,`json`=:json,`status`=:new_status,`message`=:message WHERE `id`=:command_id"#;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_drop(
                sql,
                params! {ts,json,"new_status"=>status,"message"=>message,"command_id"=>command_id},
            )
            .await
            .ok()
    }

    pub async fn set_last_item_for_batch(
        &self,
        batch_id: i64,
        last_item: &Option<String>,
    ) -> Option<()> {
        let last_item = last_item.as_deref().unwrap_or("");
        let ts = self.timestamp();
        let sql = r#"UPDATE `batch` SET `ts_last_change`=:ts,`last_item`=:last_item WHERE `id`=:batch_id"#;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_drop(sql, params! {ts,last_item,batch_id})
            .await
            .ok()
    }

    /// Persist a full LastEntityState (LAST + LAST_FORM + LAST_SENSE) to the DB.
    pub async fn set_last_state_for_batch(
        &self,
        batch_id: i64,
        state: &crate::qs_command::LastEntityState,
    ) -> Option<()> {
        let encoded = state.encode();
        self.set_last_item_for_batch(batch_id, &Some(encoded)).await
    }

    pub async fn get_user_name(&self, user_id: i64) -> Option<String> {
        let auth_db = "s53220__quickstatements_auth";
        let sql = format!(
            r#"SELECT name FROM {}.user WHERE user_id=:user_id"#,
            auth_db
        );

        let first = self
            .pool
            .get_conn()
            .await
            .ok()?
            .exec_iter(sql, params! {user_id})
            .await
            .ok()?
            .map_and_drop(from_row::<String>)
            .await
            .ok()?
            .first()
            .cloned()?;
        Some(first)
    }

    async fn get_oauth_for_batch(
        &self,
        batch_id: i64,
    ) -> Option<wikibase::mediawiki::api::OAuthParams> {
        let auth_db = "s53220__quickstatements_auth";
        let sql = format!(
            r#"SELECT serialized_json FROM {}.batch_oauth WHERE batch_id=:batch_id"#,
            auth_db
        );

        let first = self
            .pool
            .get_conn()
            .await
            .ok()?
            .exec_iter(sql, params! {batch_id})
            .await
            .ok()?
            .map_and_drop(from_row::<String>)
            .await
            .ok()?
            .first()
            .cloned()?;
        let j = serde_json::from_str(&first).ok()?;
        Some(wikibase::mediawiki::api::OAuthParams::new_from_json(&j))
    }

    pub async fn set_bot_api_auth(
        &self,
        mw_api: &mut wikibase::mediawiki::api::Api,
        batch_id: i64,
    ) -> Result<(), String> {
        match self.get_oauth_for_batch(batch_id).await {
            Some(oauth_params) => {
                mw_api.set_oauth(Some(oauth_params));
                Ok(())
            }
            None => {
                let filename = self.params["config"]["bot_config_file"]
                    .as_str()
                    .ok_or_else(|| {
                        format!(
                            "Neither OAuth nor bot info available for batch #{}",
                            batch_id
                        )
                    })?;

                // Read config file off the async runtime to avoid blocking
                let filename = filename.to_owned();
                let settings = tokio::task::spawn_blocking(move || {
                    let config_file = config::File::with_name(&filename);
                    Config::builder()
                        .add_source(config_file)
                        .build()
                        .map_err(|e| format!("Cannot read bot config '{}': {}", filename, e))
                })
                .await
                .map_err(|e| format!("spawn_blocking failed: {}", e))??;

                let lgname = settings
                    .get_string("user.user")
                    .map_err(|e| format!("Bot config missing user.user: {}", e))?;
                let lgpassword = settings
                    .get_string("user.pass")
                    .map_err(|e| format!("Bot config missing user.pass: {}", e))?;
                mw_api
                    .login(lgname, lgpassword)
                    .await
                    .map_err(|e| format!("Bot login failed: {}", e))?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mock_api(server: &MockServer) -> Api {
        // Mount siteinfo response for Api::new
        let siteinfo: serde_json::Value =
            serde_json::from_str(include_str!("../test_data/siteinfo_wikidata.json")).unwrap();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&siteinfo))
            .mount(server)
            .await;
        Api::new(&format!("{}/w/api.php", server.uri()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_is_user_blocked_false() {
        let server = MockServer::start().await;
        let mut mw_api = mock_api(&server).await;

        let not_blocked: serde_json::Value =
            serde_json::from_str(include_str!("../test_data/user_not_blocked.json")).unwrap();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&not_blocked))
            .mount(&server)
            .await;

        let result = QuickStatements::is_user_blocked(&mut mw_api, "Magnus Manske")
            .await
            .unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_is_user_blocked_true() {
        let server = MockServer::start().await;
        let mut mw_api = mock_api(&server).await;

        let blocked: serde_json::Value =
            serde_json::from_str(include_str!("../test_data/user_blocked.json")).unwrap();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&blocked))
            .mount(&server)
            .await;

        let result = QuickStatements::is_user_blocked(&mut mw_api, "Yves Schneider")
            .await
            .unwrap();
        assert!(result);
    }

    /// A QuickStatements with an unconnected pool, for testing DB-free methods.
    fn test_qs() -> QuickStatements {
        let params = json!({"mysql":{"host":"127.0.0.1","schema":"s","user":"u","pass":"p"}});
        QuickStatements {
            pool: QuickStatements::create_mysql_pool(&params),
            params,
            running_batch_ids: Arc::new(RwLock::new(HashSet::new())),
            user_counter: Arc::new(RwLock::new(HashMap::new())),
            max_batches_per_user: 2,
            verbose: false,
        }
    }

    #[tokio::test]
    async fn test_deactivate_batch_run_removes_batch_id() {
        let qs = test_qs();
        qs.running_batch_ids.write().await.insert(42_i64);
        qs.user_counter.write().await.insert(1_i64, 1_i64);

        qs.deactivate_batch_run(42, 1).await;

        assert!(!qs.running_batch_ids.read().await.contains(&42));
        assert_eq!(*qs.user_counter.read().await.get(&1).unwrap(), 0);
    }

    #[tokio::test]
    async fn test_deactivate_batch_run_is_idempotent() {
        let qs = test_qs();
        qs.running_batch_ids.write().await.insert(42_i64);
        qs.user_counter.write().await.insert(1_i64, 2_i64);

        // Second call must not decrement the counter again
        qs.deactivate_batch_run(42, 1).await;
        qs.deactivate_batch_run(42, 1).await;

        assert_eq!(*qs.user_counter.read().await.get(&1).unwrap(), 1);
    }

    #[tokio::test]
    async fn test_deactivate_batch_run_no_underflow() {
        let qs = test_qs();
        qs.running_batch_ids.write().await.insert(42_i64);
        qs.user_counter.write().await.insert(1_i64, 0_i64);

        qs.deactivate_batch_run(42, 1).await;

        assert_eq!(*qs.user_counter.read().await.get(&1).unwrap(), 0);
    }
}
