use crate::qs_command::QuickStatementsCommand;
use chrono::prelude::*;
use config::*;
use mysql as my;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, Clone)]
pub struct QuickStatements {
    params: Value,
    pool: Option<Arc<Mutex<my::Pool>>>,
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

        let mut ret = Self {
            params: params,
            pool: None,
            running_batch_ids: Arc::new(RwLock::new(HashSet::new())),
            user_counter: Arc::new(RwLock::new(HashMap::new())),
            max_batches_per_user: 2,
            verbose: false,
        };
        ret.create_mysql_pool();
        match ret.pool {
            Some(_) => Some(ret),
            None => None,
        }
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

    pub fn get_site_from_batch(&self, batch_id: i64) -> Option<String> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        for row in pool
            .lock()
            .unwrap()
            .prep_exec(
                r#"SELECT site FROM batch WHERE id=?"#,
                (my::Value::Int(batch_id),),
            )
            .ok()?
        {
            let row = row.ok()?;
            let site: String = match &row["site"] {
                my::Value::Bytes(x) => String::from_utf8_lossy(&x).to_string(),
                _ => continue,
            };
            //println!("Site from batch: {}", &site);
            return Some(site);
        }
        None
    }

    pub fn number_of_bots_running(&self) -> usize {
        self.running_batch_ids.read().unwrap().len()
    }

    pub fn timestamp(&self) -> String {
        let now = Utc::now();
        now.format("%Y%m%d%H%M%S").to_string()
    }

    pub fn restart_batch(&self, batch_id: i64) -> Option<()> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        pool.lock().unwrap().prep_exec(
            r#"UPDATE `batch` SET `status`="RUN",`message`="",`ts_last_change`=? WHERE id=? AND `status`!="TEST""#,
            (my::Value::from(self.timestamp()), my::Value::Int(batch_id)),
        ).ok()?;
        pool.lock().unwrap().prep_exec(
            r#"UPDATE `command` SET `status`="INIT",`message`="",`ts_change`=? WHERE `status`="RUN" AND `batch_id`=?"#,
            (my::Value::from(self.timestamp()),my::Value::Int(batch_id),),
        )
        .ok()?;
        Some(())
    }

    pub fn get_api_url(&self, batch_id: i64) -> Option<&str> {
        let site: String = match self.get_site_from_batch(batch_id) {
            Some(site) => site,
            None => match self.params["config"]["site"].as_str() {
                Some(s) => s.to_string(),
                None => return None,
            },
        };
        self.get_api_for_site(&site)
    }

    fn create_mysql_pool(&mut self) {
        if !self.params["mysql"].is_object() {
            return;
        }
        let mut builder = my::OptsBuilder::new();
        //println!("{}", &self.params);
        builder
            .ip_or_hostname(self.params["mysql"]["host"].as_str())
            .db_name(self.params["mysql"]["schema"].as_str())
            .user(self.params["mysql"]["user"].as_str())
            .pass(self.params["mysql"]["pass"].as_str());
        match self.params["mysql"]["port"].as_u64() {
            Some(port) => {
                builder.tcp_port(port as u16);
            }
            None => {}
        }

        // Min 2, max 7 connections
        self.pool = match my::Pool::new_manual(2, 7, builder) {
            Ok(pool) => Some(Arc::new(Mutex::new(pool))),
            _ => None,
        };
        match self.pool {
            Some(_) => println!("Pool established"),
            None => println!("No pool"),
        }
    }

    pub fn get_last_item_from_batch(&self, batch_id: i64) -> Option<String> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        for row in pool
            .lock()
            .unwrap()
            .prep_exec(
                r#"SELECT last_item FROM batch WHERE `id`=?"#,
                (my::Value::from(batch_id),),
            )
            .ok()?
        {
            let row = row.ok()?;
            return match &row["last_item"] {
                my::Value::Bytes(x) => Some(String::from_utf8_lossy(x).to_string()),
                _ => None,
            };
        }
        None
    }

    pub fn get_next_batch(&self) -> Option<(i64, i64)> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };

        let mut sql: String = "SELECT * FROM batch WHERE `status` IN (".to_string();
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

        for row in pool.lock().unwrap().prep_exec(sql, ()).ok()? {
            let row = row.ok()?;
            let id = match &row["id"] {
                my::Value::Int(x) => *x as i64,
                _ => continue,
            };
            if self.running_batch_ids.read().unwrap().contains(&id) {
                continue;
            }
            let user = match &row["user"] {
                my::Value::Int(x) => *x as i64,
                _ => continue,
            };
            return Some((id, user));
        }
        None
    }

    pub fn reinitialize_open_batches(&self) -> Option<()> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        let sql = "UPDATE batch SET status='INIT' WHERE status='DONE' AND id IN (SELECT DISTINCT batch_id FROM command WHERE status='INIT' and batch_id>12000)" ;
        pool.lock().unwrap().prep_exec(sql, ()).ok()?;
        Some(())
    }

    pub fn set_batch_running(&self, batch_id: i64, user_id: i64) {
        println!(
            "set_batch_running: Starting batch #{} for user {}",
            batch_id, user_id
        );

        match self.reinitialize_open_batches() {
            _ => {}
        }

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

    pub fn set_batch_finished(&self, batch_id: i64, user_id: i64) -> Option<()> {
        println!("set_batch_finished: Batch #{}", batch_id);
        self.set_batch_status("DONE", "", batch_id, user_id)
    }

    pub fn check_batch_not_stopped(&self, batch_id: i64) -> Result<(), String> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => {
                return Err(format!(
                    "QuickStatementsConfig::check_batch_not_stopped: Can't get DB handle"
                ))
            }
        };
        let sql: String = format!(
            "SELECT * FROM batch WHERE id={} AND `status` NOT IN ('RUN','INIT')",
            batch_id
        );
        let result = match pool.lock().unwrap().prep_exec(sql, ()) {
            Ok(r) => r,
            Err(e) => return Err(format!("Error: {}", e)),
        };
        for _row in result {
            return Err(format!(
                "QuickStatementsConfig::check_batch_not_stopped: batch #{} is not RUN or INIT",
                batch_id
            ));
        }
        Ok(())
    }

    fn set_batch_status(
        &self,
        status: &str,
        message: &str,
        batch_id: i64,
        user_id: i64,
    ) -> Option<()> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        pool.lock()
            .unwrap()
            .prep_exec(
                r#"UPDATE `batch` SET `status`=?,`message`=?,`ts_last_change`=? WHERE id=?"#,
                (
                    my::Value::from(status),
                    my::Value::from(message),
                    my::Value::from(self.timestamp()),
                    my::Value::Int(batch_id),
                ),
            )
            .ok()?;
        self.deactivate_batch_run(batch_id, user_id)
    }

    pub fn get_next_command(&self, batch_id: i64) -> Option<QuickStatementsCommand> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        let sql =
            r#"SELECT * FROM command WHERE batch_id=? AND status IN ('INIT') ORDER BY num LIMIT 1"#;
        for row in pool
            .lock()
            .unwrap()
            .prep_exec(sql, (my::Value::Int(batch_id),))
            .ok()?
        {
            let row = row.ok()?;
            return Some(QuickStatementsCommand::new_from_row(row));
        }
        None
    }

    pub fn set_command_status(
        self: &Self,
        command: &mut QuickStatementsCommand,
        new_status: &str,
        new_message: Option<String>,
    ) -> Option<()> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };

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

        pool.lock().unwrap().prep_exec(
            r#"UPDATE `command` SET `ts_change`=?,`json`=?,`status`=?,`message`=? WHERE `id`=?"#,
            (
                my::Value::from(self.timestamp()),
                my::Value::from(json),
                my::Value::from(new_status),
                my::Value::from(message),
                my::Value::from(&command.id),
            ),
        )
        .ok()?;
        Some(())
    }

    pub fn set_last_item_for_batch(
        self: &Self,
        batch_id: i64,
        last_item: &Option<String>,
    ) -> Option<()> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        let last_item = match last_item {
            Some(q) => q.to_string(),
            None => "".to_string(),
        };

        let ts = self.timestamp();
        pool.lock()
            .unwrap()
            .prep_exec(
                r#"UPDATE `batch` SET `ts_last_change`=?,`last_item`=? WHERE `id`=?"#,
                (
                    my::Value::from(ts),
                    my::Value::from(last_item),
                    my::Value::from(batch_id),
                ),
            )
            .ok()?;
        Some(())
    }

    fn get_oauth_for_batch(
        self: &Self,
        batch_id: i64,
    ) -> Option<wikibase::mediawiki::api::OAuthParams> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        let auth_db = "s53220__quickstatements_auth";
        let sql = format!(r#"SELECT * FROM {}.batch_oauth WHERE batch_id=?"#, auth_db);
        for row in pool
            .lock()
            .unwrap()
            .prep_exec(sql, (my::Value::from(batch_id),))
            .ok()?
        {
            let row = row.ok()?;
            let serialized_json = match &row["serialized_json"] {
                my::Value::Bytes(x) => String::from_utf8_lossy(x),
                _ => return None,
            };

            match serde_json::from_str(&serialized_json) {
                Ok(j) => return Some(wikibase::mediawiki::api::OAuthParams::new_from_json(&j)),
                _ => return None,
            }
        }
        None
    }

    pub fn set_bot_api_auth(&self, mw_api: &mut wikibase::mediawiki::api::Api, batch_id: i64) {
        match self.get_oauth_for_batch(batch_id) {
            Some(oauth_params) => {
                // Using OAuth
                mw_api.set_oauth(Some(oauth_params));
            }
            None => {
                match self.params["config"]["bot_config_file"].as_str() {
                    Some(filename) => {
                        // Using Bot
                        let mut settings = Config::default();
                        settings
                            .merge(config::File::with_name(filename))
                            .expect("QuickStatements::set_bot_api_auth: Can't merge settings");
                        let lgname = settings
                            .get_str("user.user")
                            .expect("QuickStatements::set_bot_api_auth: Can't get user name");
                        let lgpassword = settings
                            .get_str("user.pass")
                            .expect("QuickStatements::set_bot_api_auth: Can't get user password");
                        mw_api
                            .login(lgname, lgpassword)
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
