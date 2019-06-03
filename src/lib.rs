#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate lazy_static;

use config::*;
use mysql as my;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::sync::{Arc, Mutex};
use wikibase;

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

#[derive(Debug, Clone)]
pub struct QuickStatementsBot {
    batch_id: i64,
    config: Arc<Mutex<QuickStatements>>,
    mw_api: Option<mediawiki::api::Api>,
    entities: wikibase::entity_container::EntityContainer,
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
            entities: wikibase::entity_container::EntityContainer::new(),
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

    fn set_label(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        let i = self.get_item_from_command(command)?.to_owned();
        let language = command.json["language"].as_str().unwrap();
        let text = command.json["value"].as_str().unwrap();
        match i.label_in_locale(language) {
            Some(s) => {
                if s == text {
                    return Ok(());
                }
            }
            None => {}
        }
        self.run_action(json!({"action":"wbsetlabel","id":self.get_prefixed_id(i.id()),"language":language,"value":text}),command) // TODO baserevid?
    }

    fn add_alias(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        let i = self.get_item_from_command(command)?.to_owned();
        let language = command.json["language"].as_str().unwrap();
        let text = command.json["value"].as_str().unwrap();
        self.run_action(json!({"action":"wbsetaliases","id":self.get_prefixed_id(i.id()),"language":language,"add":text}),command) // TODO baserevid?
    }

    fn set_description(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        let i = self.get_item_from_command(command)?.to_owned();
        let language = command.json["language"].as_str().unwrap();
        let text = command.json["value"].as_str().unwrap();
        match i.description_in_locale(language) {
            Some(s) => {
                if s == text {
                    return Ok(());
                }
            }
            None => {}
        }
        self.run_action(json!({"action":"wbsetdescription","id":self.get_prefixed_id(i.id()),"language":language,"value":text}),command) // TODO baserevid?
    }

    fn set_sitelink(self: &mut Self, _command: &mut QuickStatementsCommand) -> Result<(), String> {
        // TODO
        Ok(())
    }

    fn add_statement(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        println!("ADD STATEMENT 0");
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        println!("ADD STATEMENT 1");
        let i = self.get_item_from_command(command)?.to_owned();
        println!("ADD STATEMENT 2");

        let property = match command.json["property"].as_str() {
            Some(p) => p.to_owned(),
            None => return Err("Property not found".to_string()),
        };
        println!("ADD STATEMENT 3");
        let value = match serde_json::to_string(&command.json["datavalue"]["value"]) {
            Ok(v) => v,
            Err(_) => return Err("Bad datavalue.value".to_string()),
        };
        println!("ADD STATEMENT 4");

        match self.get_statement_id(command) {
            Ok(_) => {
                println!("Such a statement already exists, return");
                return Ok(());
            }
            _ => {}
        }
        println!("ADD STATEMENT 5");

        self.run_action(
            json!({
                "action":"wbcreateclaim",
                "entity":self.get_prefixed_id(i.id()),
                "snaktype":self.get_snak_type_for_datavalue(&command.json["datavalue"])?,
                "property":property,
                "value":value
            }),
            command,
        ) // TODO baserevid?
    }

    fn get_snak_type_for_datavalue(&self, dv: &Value) -> Result<String, String> {
        let ret = match &dv["value"].as_str() {
            Some("novalue") => "novalue",
            Some("somevalue") => "somevalue",
            Some(_) => "value",
            None => return Err("Cannot determine snak type".to_string()),
        };
        Ok(ret.to_string())
    }

    fn add_qualifier(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let _statement_id = self.get_statement_id(command)?;
        // TODO
        Ok(())
    }

    fn add_sources(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let _statement_id = self.get_statement_id(command)?;
        // TODO
        Ok(())
    }

    fn run_action(
        self: &mut Self,
        j: Value,
        _command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        // TODO
        println!("Running action {}", &j);
        let mut params: HashMap<String, String> = HashMap::new();
        for (k, v) in j.as_object().unwrap() {
            params.insert(k.to_string(), v.as_str().unwrap().to_string());
            // serde_json::to_string(v).unwrap()
        }
        let mut mw_api = self.mw_api.to_owned().unwrap();
        params.insert("token".to_string(), mw_api.get_edit_token().unwrap());
        println!("As: {:?}", &params);
        match mw_api.post_query_api_json_mut(&params) {
            Ok(x) => {
                println!("WIKIDATA OK: {:?}", &x);
                Ok(())
            }
            Err(e) => {
                println!("WIKIDATA ERROR: {:?}", &e);
                Err("Wikidata editing fail".to_string())
            }
        }
    }

    fn get_prefixed_id(&self, s: &str) -> String {
        s.to_string() // TODO FIXME
    }

    fn is_claim_base_for_command(
        &self,
        claim: &wikibase::Statement,
        existing: &wikibase::Statement,
    ) -> Option<String> {
        lazy_static! {
            static ref RE_TIME: Regex = Regex::new("^(?P<a>[+-]{0,1})0*(?P<b>.+)$").unwrap();
        }
        if claim.main_snak().datatype() != existing.main_snak().datatype() {
            return None;
        }
        if claim.main_snak().data_value().is_none() || existing.main_snak().data_value().is_none() {
            return None;
        }

        let statement_id = match claim.id() {
            Some(id) => id,
            None => return None,
        };

        let dv_c = match claim.main_snak().data_value() {
            Some(dv) => dv,
            None => return None,
        };
        let dv_e = match existing.main_snak().data_value() {
            Some(dv) => dv,
            None => return None,
        };

        if dv_c.value_type() != dv_e.value_type() {
            return None;
        }

        if claim.main_snak().snak_type() != existing.main_snak().snak_type() {
            return None;
        }

        match claim.main_snak().snak_type() {
            wikibase::SnakType::NoValue => return Some(statement_id),
            wikibase::SnakType::UnknownValue => return Some(statement_id),
            _ => {}
        }

        match (dv_c.value(), dv_e.value()) {
            (wikibase::Value::Coordinate(vc), wikibase::Value::Coordinate(ve)) => {
                if vc.globe() != ve.globe()
                    || vc.latitude() != ve.latitude()
                    || vc.longitude() != ve.longitude()
                {
                    return None;
                }
            }
            (wikibase::Value::MonoLingual(vc), wikibase::Value::MonoLingual(ve)) => {
                if vc.language() != ve.language()
                    || self.normalize_string(&vc.text().to_string())
                        != self.normalize_string(&ve.text().to_string())
                {
                    return None;
                }
            }
            (wikibase::Value::Entity(vc), wikibase::Value::Entity(ve)) => {
                if vc.id() != ve.id() {
                    return None;
                }
            }
            (wikibase::Value::Quantity(vc), wikibase::Value::Quantity(ve)) => {
                if *vc.amount() != *ve.amount() {
                    return None;
                }
            }
            (wikibase::Value::StringValue(vc), wikibase::Value::StringValue(ve)) => {
                if self.normalize_string(vc) != self.normalize_string(ve) {
                    return None;
                }
            }
            (wikibase::Value::Time(vc), wikibase::Value::Time(ve)) => {
                if vc.calendarmodel() != ve.calendarmodel() || vc.precision() != ve.precision() {
                    return None;
                }
                let tc = RE_TIME.replace_all(vc.time(), "$a$b");
                let te = RE_TIME.replace_all(ve.time(), "$a$b");
                if tc != te {
                    return None;
                }
            }
            _ => return None,
        }

        Some(statement_id)
    }

    fn normalize_string(&self, s: &String) -> String {
        // TODO necessary?
        // In PHP: normalizer_normalize (using Form D)
        s.to_string()
    }

    fn get_item_from_command(
        &mut self,
        command: &mut QuickStatementsCommand,
    ) -> Result<&wikibase::Entity, String> {
        let q = match command.json["item"].as_str() {
            Some(q) => q.to_string(),
            None => return Err("Item expected but not set".to_string()),
        };
        let mw_api = self.mw_api.to_owned().unwrap();
        println!("LOADING ENTITY {}", &q);
        match self.entities.load_entities(&mw_api, &vec![q.to_owned()]) {
            Ok(_) => {}
            Err(e) => {
                println!("ERROR: {:?}", &e);
                return Err("Error while loading into entities".to_string());
            }
        }

        let i = match self.entities.get_entity(q) {
            Some(i) => i,
            None => return Err("Failed to get item".to_string()),
        };
        Ok(i)
    }

    fn create_fake_item_from_command(
        &self,
        command: &mut QuickStatementsCommand,
    ) -> Result<wikibase::Entity, wikibase::WikibaseError> {
        let mut j = json!({"id":"Q0","claims":[{}],"labels":[],"descriptions":[],"aliases":[],"sitelinks":[]});
        j["claims"][0] = json!({
            "value":command.json["datavalue"].clone(),
            "property":command.json["property"].clone()
        });
        wikibase::from_json::entity_from_json(&j)
    }

    fn get_statement_id(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<String, String> {
        let _property = match command.json["property"].as_str() {
            Some(p) => p,
            None => return Err("Property expected but not set".to_string()),
        };
        let _datavalue = match command.json["datavalue"].as_object() {
            Some(dv) => dv,
            None => return Err("Datavalue expected but not set".to_string()),
        };

        let i = self.get_item_from_command(command)?.to_owned();
        let dummy_item = match self.create_fake_item_from_command(command) {
            Ok(item) => item,
            _ => return Err("Cannot create dummy item/statement".to_string()),
        };

        let dummy_statement = match dummy_item.claims().get(0) {
            Some(statement) => statement,
            None => return Err("Can't create statement".to_string()),
        };

        for claim in i.claims() {
            match self.is_claim_base_for_command(&claim, &dummy_statement) {
                Some(id) => {
                    return Ok(id);
                }
                None => {}
            }
        }

        Err("Base statement not found".to_string())
    }

    fn replace_last_item(&self, v: &mut Value) -> Result<(), String> {
        if !v.is_object() {
            return Ok(());
        }
        if self.last_entity_id.is_none() {
            return Ok(()); //Err("Last item expected but not set".to_string());
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

    /// Replaces LAST in the command with the last item, or fails
    /// This method is called propagateLastItem in the PHP version
    fn insert_last_item_into_sources_and_qualifiers(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
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
