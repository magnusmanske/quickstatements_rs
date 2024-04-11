use crate::qs_command::QuickStatementsCommand;
use anyhow::Result;
use chrono::prelude::Utc;
use config::*;
use mysql_async as my;
use mysql_async::from_row;
use mysql_async::prelude::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::sync::{Arc, RwLock};

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
        let params: Value = serde_json::from_reader(file).ok()?;
        let mut params = params.clone();

        // Load the PHP/JS config into params as ["config"], or create empty object
        params["config"] = match params["config_file"].as_str() {
            Some(filename) => {
                let file = File::open(filename).ok()?;
                serde_json::from_reader(file).ok()?
            }
            None => json!({}),
        };

        let ret = Self {
            params: params.clone(),
            pool: Self::create_mysql_pool(&params).ok()?,
            running_batch_ids: Arc::new(RwLock::new(HashSet::new())),
            user_counter: Arc::new(RwLock::new(HashMap::new())),
            max_batches_per_user: 2,
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

    pub fn edit_delay_ms(&self) -> Option<u64> {
        match self.params["edit_delay_ms"].as_u64() {
            Some(x) => Some(x),
            None => Some(1000), // default: 1000ms=1sec
        }
    }

    pub fn maxlag_s(&self) -> Option<u64> {
        match self.params["set_maxlag"].as_u64() {
            Some(x) => Some(x),
            None => Some(5), // default: 5sec
        }
    }

    pub async fn get_site_from_batch(&self, batch_id: i64) -> Option<String> {
        let sql = r#"SELECT site FROM batch WHERE id=:batch_id"#;
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

    pub fn number_of_bots_running(&self) -> usize {
        self.running_batch_ids.read().unwrap().len()
    }

    pub fn timestamp(&self) -> String {
        let now = Utc::now();
        now.format("%Y%m%d%H%M%S").to_string()
    }

    pub async fn restart_batch(&self, batch_id: i64) -> Option<()> {
        let mut conn = self.pool.get_conn().await.ok()?;
        let ts = self.timestamp();
        conn.exec_drop(r#"UPDATE `batch` SET `status`="RUN",`message`="",`ts_last_change`=:ts WHERE id=:batch_id AND `status`!="TEST""#, params!{ts,batch_id}).await.ok()?;
        let ts = self.timestamp();
        conn.exec_drop(r#"UPDATE `command` SET `status`="INIT",`message`="",`ts_change`=:ts WHERE `status`="RUN" AND `batch_id`=:batch_id"#, params!{ts,batch_id}).await.ok()
    }

    pub async fn reset_all_running_batches(&self) -> Result<()> {
        let mut conn = self.pool.get_conn().await?;
        let ts = self.timestamp();
        conn.exec_drop(r#"UPDATE `batch` SET `status`="INIT",`message`="",`ts_last_change`=:ts WHERE `status`="RUN""#, params!{ts}).await?;
        Ok(())
    }

    pub async fn get_api_url(&self, batch_id: i64) -> Option<&str> {
        let site: String = match self.get_site_from_batch(batch_id).await {
            Some(site) => site,
            None => match self.params["config"]["site"].as_str() {
                Some(s) => s.to_string(),
                None => return None,
            },
        };
        self.get_api_for_site(&site)
    }

    fn create_mysql_pool(params: &Value) -> Result<my::Pool, String> {
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

        Ok(mysql_async::Pool::new(opts))
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

    pub async fn get_next_batch(&self) -> Option<(i64, i64)> {
        let mut sql: String = "SELECT id,user FROM batch WHERE `status` IN (".to_string();
        sql += "'INIT','RUN'";
        //sql += "'TEST'" ;
        sql += ")";

        //sql += " AND id=13324"; // TESTING: Specific batch only
        //sql += " AND user=4420"; // TESTING: [[User:Magnus Manske]] only
        sql += r#" AND NOT EXISTS (SELECT * FROM command WHERE batch_id=batch.id AND json rlike '"item":"L\\d')"#; // TESTING: Available batches that do NOT use lexemes

        // Find users that are already running the maximum of simultaneous jobs
        // This is to prevent MW API "too many edits" errors
        // Also, it's more fair
        let bad_users: Vec<String> = self
            .user_counter
            .read()
            .unwrap()
            .iter()
            .filter_map(|(user_id, cnt)| {
                if *cnt >= self.max_batches_per_user {
                    Some(user_id.to_string())
                } else {
                    None
                }
            })
            .collect();
        if !bad_users.is_empty() {
            sql += " AND user NOT IN (";
            sql += &bad_users.join(",");
            sql += ")";
        }
        sql += " ORDER BY `ts_last_change`";

        let results = self
            .pool
            .get_conn()
            .await
            .ok()?
            .exec_iter(sql, ())
            .await
            .ok()?
            .map_and_drop(from_row::<(i64, i64)>)
            .await
            .ok()?;
        results
            .iter()
            .filter(|(id, _)| !self.running_batch_ids.read().unwrap().contains(id))
            .cloned()
            .next()
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
        println!(
            "set_batch_running: Starting batch #{} for user {}",
            batch_id, user_id
        );

        let _ = self.reinitialize_open_batches().await;

        // Increase user batch counter
        self.running_batch_ids.write().unwrap().insert(batch_id);
        let user_counter = match self.user_counter.read().unwrap().get(&user_id) {
            Some(cnt) => *cnt,
            None => 0,
        };
        self.user_counter
            .write()
            .unwrap()
            .insert(user_id, user_counter + 1);

        println!("Currently {} bots running", self.number_of_bots_running());
    }

    pub fn deactivate_batch_run(&self, batch_id: i64, user_id: i64) -> Option<()> {
        // Decrease user batch counter
        self.running_batch_ids.write().unwrap().insert(batch_id);
        let user_counter = match self.user_counter.read().unwrap().get(&user_id) {
            Some(cnt) => *cnt,
            None => 0,
        };
        self.user_counter
            .write()
            .unwrap()
            .insert(user_id, user_counter - 1);
        self.running_batch_ids.write().unwrap().remove(&batch_id);
        println!("Currently {} bots running", self.number_of_bots_running());
        Some(())
    }

    pub async fn set_batch_finished(&self, batch_id: i64, user_id: i64) -> Option<()> {
        println!("set_batch_finished: Batch #{}", batch_id);
        self.set_batch_status("DONE", "", batch_id, user_id).await
    }

    pub async fn check_batch_not_stopped(&self, batch_id: i64) -> Result<(), String> {
        let sql = r#"SELECT id FROM batch WHERE id=:batch_id AND `status` NOT IN ('RUN','INIT')"#;

        let results = self
            .pool
            .get_conn()
            .await
            .map_err(|e| e.to_string())?
            .exec_iter(sql, params! {batch_id})
            .await
            .map_err(|e| e.to_string())?
            .map_and_drop(from_row::<usize>)
            .await
            .map_err(|e| e.to_string())?;
        match results.is_empty() {
            true => Ok(()),
            false => Err(format!(
                "QuickStatementsConfig::check_batch_not_stopped: batch #{} is not RUN or INIT",
                batch_id
            )),
        }
    }

    async fn set_batch_status(
        &self,
        status: &str,
        message: &str,
        batch_id: i64,
        user_id: i64,
    ) -> Option<()> {
        let ts = self.timestamp();
        let sql = r#"UPDATE `batch` SET `status`=:status,`message`=:message,`ts_last_change`=:ts WHERE id=:batch_id"#;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_drop(sql, params! {status,message,ts,batch_id})
            .await
            .ok()?;
        self.deactivate_batch_run(batch_id, user_id)
    }

    pub async fn get_next_command(&self, batch_id: i64) -> Option<QuickStatementsCommand> {
        let sql = r#"SELECT id,batch_id,num,json,`status`,message,ts_change FROM command WHERE batch_id=:batch_id AND status IN ('INIT') ORDER BY num LIMIT 1"#;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_iter(sql, params! {batch_id})
            .await
            .ok()?
            .map_and_drop(from_row::<(i64, i64, i64, String, String, String, String)>)
            .await
            .ok()?
            .iter()
            .map(QuickStatementsCommand::from_row)
            .next()
    }

    pub async fn set_command_status(
        &self,
        command: &mut QuickStatementsCommand,
        new_status: &str,
        new_message: Option<String>,
    ) -> Option<()> {
        command.json["meta"]["status"] = json!(new_status.to_string().trim().to_uppercase());

        let message: String = match &new_message {
            Some(s) => s.to_string(),
            None => "".to_string(),
        };
        command.json["meta"]["message"] = json!(message);

        let json = match serde_json::to_string(&command.json) {
            Ok(s) => s,
            _ => "{}".to_string(),
        };

        let command_id = command.id;
        let ts = self.timestamp();
        let sql = r#"UPDATE `command` SET `ts_change`=:ts,`json`=:json,`status`=:new_status,`message`=:message WHERE `id`=:command_id"#;
        self.pool
            .get_conn()
            .await
            .ok()?
            .exec_drop(sql, params! {ts,json,new_status,message,command_id})
            .await
            .ok()
    }

    pub async fn set_last_item_for_batch(
        &self,
        batch_id: i64,
        last_item: &Option<String>,
    ) -> Option<()> {
        let last_item = match last_item {
            Some(q) => q.to_string(),
            None => "".to_string(),
        };

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
    ) {
        match self.get_oauth_for_batch(batch_id).await {
            Some(oauth_params) => {
                // Using OAuth
                mw_api.set_oauth(Some(oauth_params));
            }
            None => {
                match self.params["config"]["bot_config_file"].as_str() {
                    Some(filename) => {
                        // Using Bot
                        let config_file = config::File::with_name(filename);
                        let settings = Config::builder()
                            .add_source(config_file)
                            .build()
                            .expect("Cannot create config from config file");
                        let lgname = settings
                            .get_string("user.user")
                            .expect("QuickStatements::set_bot_api_auth: Can't get user name");
                        let lgpassword = settings
                            .get_string("user.pass")
                            .expect("QuickStatements::set_bot_api_auth: Can't get user password");
                        mw_api
                            .login(lgname, lgpassword)
                            .await
                            .expect("Cannot login as bot");
                    }
                    None => panic!(
                        "Neither OAuth nor bot info available for batch #{}",
                        batch_id
                    ),
                }
            }
        }
    }
}
