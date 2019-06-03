use crate::qs_command::QuickStatementsCommand;
use config::*;
use mysql as my;
//use regex::Regex;
use serde_json::Value;
//use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
//use std::sync::{Arc, Mutex};
//use wikibase;

#[derive(Debug, Clone)]
pub struct QuickStatements {
    params: Value,
    pool: Option<my::Pool>,
    running_batch_ids: HashSet<i64>,
}

impl QuickStatements {
    pub fn new_from_config_json(filename: &str) -> Self {
        let file = File::open(filename).unwrap();
        let params: Value = serde_json::from_reader(file).unwrap();
        let mut params = params.clone();

        // Load the PHP/JS config into params as ["config"], or create empty object
        params["config"] = match params["config_file"].as_str() {
            Some(filename) => {
                let file = File::open(filename).unwrap();
                serde_json::from_reader(file).unwrap()
            }
            None => serde_json::from_str("{}").unwrap(),
        };

        let mut ret = Self {
            params: params,
            pool: None,
            running_batch_ids: HashSet::new(),
        };
        ret.create_mysql_pool();
        ret
    }

    pub fn get_api_url(&self) -> Option<&str> {
        match self.params["config"]["site"].as_str() {
            Some(site) => self.params["config"]["sites"][site]["api"].as_str(),
            None => None,
        }
    }

    fn create_mysql_pool(&mut self) {
        // ssh magnus@tools-login.wmflabs.org -L 3307:tools-db:3306 -N
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
            Ok(pool) => Some(pool),
            _ => None,
        }
    }

    pub fn get_next_batch(&self) -> Option<i64> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        for row in pool
            .prep_exec(
                r#"SELECT * FROM batch WHERE `status` IN ('TEST') ORDER BY `ts_last_change`"#, // 'INIT','RUN' TESTING
                (),
            )
            .unwrap()
        {
            let row = row.unwrap();
            let id = match &row["id"] {
                my::Value::Int(x) => *x as i64,
                _ => continue,
            };
            if self.running_batch_ids.contains(&id) {
                continue;
            }
            return Some(id);
            /*
            let status = match &row["status"] {
                my::Value::Bytes(x) => String::from_utf8_lossy(x),
                _ => continue,
            };
            println!("{}:{}", &id, &status);
            */
        }
        None
    }

    pub fn set_batch_running(&mut self, batch_id: i64) {
        println!("set_batch_running: Starting batch #{}", batch_id);
        self.running_batch_ids.insert(batch_id);
    }

    pub fn set_batch_finished(&mut self, batch_id: i64) {
        println!("set_batch_finished: Batch #{}", batch_id);
        // TODO update batch status/time
        self.running_batch_ids.remove(&batch_id);
    }

    pub fn get_next_command(&mut self, batch_id: i64) -> Option<QuickStatementsCommand> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        let sql =
            r#"SELECT * FROM command WHERE batch_id=? AND status IN ('INIT') ORDER BY num LIMIT 1"#;
        for row in pool.prep_exec(sql, (my::Value::Int(batch_id),)).unwrap() {
            let row = row.unwrap();
            return Some(QuickStatementsCommand::new_from_row(row));
        }
        None
    }

    pub fn set_command_status<S: Into<String>>(
        self: &mut Self,
        command_id: i64,
        new_status: &str,
        new_message: Option<S>,
    ) {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => panic!("set_command_status: MySQL pool not available"),
        };
        if false {
            // TODO deactivated for testing
            let pe = match new_message {
                Some(message) => pool.prep_exec(
                    r#"UPDATE command SET status=?,message=? WHERE id=?"#,
                    (
                        my::Value::from(new_status),
                        my::Value::from(message.into()),
                        my::Value::from(command_id),
                    ),
                ),
                None => pool.prep_exec(
                    r#"UPDATE command SET status=? WHERE id=?"#,
                    (my::Value::from(new_status), my::Value::from(command_id)),
                ),
            };
            pe.unwrap();
        }
    }

    fn get_oauth_for_batch(self: &mut Self, batch_id: i64) -> Option<mediawiki::api::OAuthParams> {
        let pool = match &self.pool {
            Some(pool) => pool,
            None => return None,
        };
        let auth_db = "s53220__quickstatements_auth";
        let sql = format!(r#"SELECT * FROM {}.batch_oauth WHERE batch_id=?"#, auth_db);
        for row in pool.prep_exec(sql, (my::Value::from(batch_id),)).unwrap() {
            let row = row.unwrap();
            let serialized_json = match &row["serialized_json"] {
                my::Value::Bytes(x) => String::from_utf8_lossy(x),
                _ => return None,
            };

            match serde_json::from_str(&serialized_json) {
                Ok(j) => return Some(mediawiki::api::OAuthParams::new_from_json(&j)),
                _ => return None,
            }
        }
        None
    }

    pub fn set_bot_api_auth(self: &mut Self, mw_api: &mut mediawiki::api::Api, batch_id: i64) {
        match self.get_oauth_for_batch(batch_id) {
            Some(oauth_params) => {
                // Using OAuth
                println!("USING OAUTH");
                mw_api.set_oauth(Some(oauth_params));
            }
            None => {
                match self.params["config"]["bot_config_file"].as_str() {
                    Some(filename) => {
                        println!("USING BOT");
                        // Using Bot
                        let mut settings = Config::default();
                        settings.merge(config::File::with_name(filename)).unwrap();
                        let lgname = settings.get_str("user.user").unwrap();
                        let lgpassword = settings.get_str("user.pass").unwrap();
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
