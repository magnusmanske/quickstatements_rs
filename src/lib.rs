#[macro_use]
extern crate serde_json;

use config::*;
use mysql as my;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct QuickStatementsCommand {
    id: i64,
    batch_id: i64,
    num: i64,
    json: Value,
    status: String,
    message: String,
    ts_change: String,
}

impl QuickStatementsCommand {
    pub fn new_from_row(row: my::Row) -> Self {
        Self {
            id: QuickStatementsCommand::rowvalue_as_i64(&row["id"]),
            batch_id: QuickStatementsCommand::rowvalue_as_i64(&row["batch_id"]),
            num: QuickStatementsCommand::rowvalue_as_i64(&row["num"]),
            json: match &row["json"] {
                my::Value::Bytes(x) => serde_json::from_str(&String::from_utf8_lossy(x)).unwrap(),
                _ => Value::Null,
            },
            status: QuickStatementsCommand::rowvalue_as_string(&row["status"]),
            message: QuickStatementsCommand::rowvalue_as_string(&row["message"]),
            ts_change: QuickStatementsCommand::rowvalue_as_string(&row["ts_change"]),
        }
    }

    fn rowvalue_as_i64(v: &my::Value) -> i64 {
        match v {
            my::Value::Int(x) => *x,
            _ => 0,
        }
    }

    fn rowvalue_as_string(v: &my::Value) -> String {
        match v {
            my::Value::Bytes(x) => String::from_utf8_lossy(x).to_string(),
            _ => String::from(""),
        }
    }
}

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
                mw_api.set_oauth(Some(oauth_params));
            }
            None => {
                match self.params["config"]["bot_config_file"].as_str() {
                    Some(filename) => {
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

#[derive(Debug, Clone)]
pub struct QuickStatementsBot {
    batch_id: i64,
    config: Arc<Mutex<QuickStatements>>,
    mw_api: Option<mediawiki::api::Api>,
    last_entity_id: Option<String>,
    current_entity_id: Option<String>,
    current_property_id: Option<String>,
}

impl QuickStatementsBot {
    pub fn new(config: Arc<Mutex<QuickStatements>>, batch_id: i64) -> Self {
        Self {
            batch_id: batch_id,
            config: config.clone(),
            mw_api: None,
            last_entity_id: None,
            current_entity_id: None,
            current_property_id: None,
        }
    }

    pub fn start(self: &mut Self) {
        let mut config = self.config.lock().unwrap();
        match config.get_api_url() {
            Some(url) => {
                let mut mw_api = mediawiki::api::Api::new(url).unwrap();
                config.set_bot_api_auth(&mut mw_api, self.batch_id);
                self.mw_api = Some(mw_api);
            }
            None => {
                panic!("No site/API info available");
            }
        }

        config.set_batch_running(self.batch_id);
    }

    pub fn run(self: &mut Self) -> bool {
        println!("Batch #{}: doing stuff", self.batch_id);
        match self.get_next_command() {
            Some(mut command) => {
                match self.execute_command(&mut command) {
                    Ok(_) => {}
                    Err(message) => self.set_command_status("ERROR", Some(&message), &mut command),
                }
                true
            }
            None => {
                let mut config = self.config.lock().unwrap();
                config.set_batch_finished(self.batch_id);
                false
            }
        }
    }

    fn get_next_command(&self) -> Option<QuickStatementsCommand> {
        let mut config = self.config.lock().unwrap();
        config.get_next_command(self.batch_id)
    }

    fn create_new_entity(
        self: &mut Self,
        _command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        // TODO
        Ok(())
    }

    fn merge_entities(
        self: &mut Self,
        _command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        // TODO
        Ok(())
    }

    fn set_label(self: &mut Self, _command: &mut QuickStatementsCommand) -> Result<(), String> {
        // TODO
        Ok(())
    }

    fn add_alias(self: &mut Self, _command: &mut QuickStatementsCommand) -> Result<(), String> {
        // TODO
        Ok(())
    }

    fn set_description(
        self: &mut Self,
        _command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        // TODO
        Ok(())
    }

    fn set_sitelink(self: &mut Self, _command: &mut QuickStatementsCommand) -> Result<(), String> {
        // TODO
        Ok(())
    }

    fn add_statement(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        // TODO
        Ok(())
    }

    fn add_qualifier(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let _statement_id = match self.get_statement_id(command) {
            Some(id) => id,
            None => return Err("No statement ID available".to_string()),
        };
        // TODO
        Ok(())
    }

    fn add_sources(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let _statement_id = match self.get_statement_id(command) {
            Some(id) => id,
            None => return Err("No statement ID available".to_string()),
        };
        // TODO
        Ok(())
    }

    fn get_statement_id(self: &mut Self, command: &mut QuickStatementsCommand) -> Option<String> {
        if command.json["property"].as_str().is_none() {
            return None;
        }
        if command.json["datavalue"].as_object().is_none() {
            return None;
        }
        // TODO load item and find statement
        None
    }

    fn replace_last_item(&self, v: &mut Value) -> Result<(), String> {
        if !v.is_object() {
            return Ok(());
        }
        if self.last_entity_id.is_none() {
            return Err("Last item expected but not set".to_string());
        }
        match &v["type"].as_str() {
            Some("wikibase-entityid") => {}
            _ => return Ok(()),
        }
        match &v["value"]["id"].as_str() {
            Some(id) => {
                if &self.fix_entity_id(id.to_string()) == "LAST" {
                    let id = self.last_entity_id.clone().unwrap();
                    v["value"]["id"] = json!(id);
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn insert_last_item_into_sources_and_qualifiers(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        // This is called propagateLastItem in the PHP version
        self.replace_last_item(&mut command.json["datavalue"])?;
        self.replace_last_item(&mut command.json["qualifier"]["value"])?;
        match command.json["sources"].as_array_mut() {
            Some(arr) => {
                for mut v in arr {
                    self.replace_last_item(&mut v)?
                }
            }
            None => {}
        }
        Ok(())
    }

    fn add_to_entity(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.load_command_items(command);
        if self.current_entity_id.is_none() {
            return Err("No (last) item available".to_string());
        }

        println!(
            "{:?}/{:?}",
            &self.current_entity_id, &self.current_property_id
        );
        println!("{}", &command.json);

        match command.json["what"].as_str() {
            Some("label") => self.set_label(command),
            Some("alias") => self.add_alias(command),
            Some("description") => self.set_description(command),
            Some("sitelink") => self.set_sitelink(command),
            Some("statement") => self.add_statement(command),
            Some("qualifier") => self.add_qualifier(command),
            Some("sources") => self.add_sources(command),
            _other => Err("Bad 'what'".to_string()),
        }
    }

    fn remove_from_entity(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        self.load_command_items(command);
        if self.current_entity_id.is_none() {
            return Err("No (last) item available".to_string());
        }
        // TODO
        Ok(())
    }

    fn get_entity_id_option(&self, v: &Value) -> Option<String> {
        match v.as_str() {
            Some(s) => Some(self.fix_entity_id(s.to_string())),
            None => None,
        }
    }

    fn load_command_items(self: &mut Self, command: &mut QuickStatementsCommand) {
        // Reset
        self.current_property_id = self.get_entity_id_option(&command.json["property"]);
        self.current_entity_id = self.get_entity_id_option(&command.json["item"]);

        // Special case
        match command.json["what"].as_str() {
            Some(what) => {
                if what == "statement"
                    && command.json["item"].as_str().is_none()
                    && command.json["id"].as_str().is_some()
                {
                    let q = command.json["id"].as_str().unwrap();
                    let q = self.fix_entity_id(q.to_string());
                    self.current_entity_id = Some(q.clone());
                }
            }
            None => {}
        }

        if self.current_entity_id == Some("LAST".to_string()) {
            self.current_entity_id = self.last_entity_id.clone();
        }
        match &self.current_entity_id {
            Some(q) => command.json["item"] = Value::from(q.clone()),
            None => {}
        }

        println!(
            "Q:{:?} / P:{:?}",
            &self.current_entity_id, &self.current_property_id
        );
    }

    fn fix_entity_id(&self, id: String) -> String {
        id.trim().to_uppercase()
    }

    fn execute_command(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        self.set_command_status("RUN", None, command);
        self.current_property_id = None;
        self.current_entity_id = None;

        // TODO
        // $summary = "[[:toollabs:quickstatements/#/batch/{$batch_id}|batch #{$batch_id}]] by [[User:{$this->user_name}|]]" ;
        // if ( !isset($cmd->json->summary) ) $cmd->summary = $summary ; else $cmd->summary .= '; ' . $summary ;

        let result = match command.json["action"].as_str().unwrap() {
            "create" => self.create_new_entity(command),
            "merge" => self.merge_entities(command),
            "add" => self.add_to_entity(command),
            "remove" => self.remove_from_entity(command),
            other => Err(format!("Unknown action '{}'", &other)),
        };

        // TODO update last item if Ok(())
        match &result {
            Err(message) => self.set_command_status("ERROR", Some(message), command),
            _ => self.set_command_status("DONE", None, command),
        }
        result
    }

    fn set_command_status(
        self: &mut Self,
        status: &str,
        message: Option<&str>,
        command: &mut QuickStatementsCommand,
    ) {
        if status == "DONE" {
            if self.current_entity_id.is_some() {
                self.last_entity_id = self.current_entity_id.clone();
            }
        }

        let mut config = self.config.lock().unwrap();
        config.set_command_status(command.id, status, message);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
