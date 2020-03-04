use crate::qs_command::QuickStatementsCommand;
use crate::qs_config::QuickStatements;
use crate::qs_parser::COMMONS_API;
use chrono::Local;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::{thread, time};
use wikibase;

#[derive(Debug, Clone)]
pub struct QuickStatementsBot {
    batch_id: Option<i64>,
    user_id: i64,
    config: Arc<QuickStatements>,
    mw_api: Option<wikibase::mediawiki::api::Api>,
    pub entities: wikibase::entity_container::EntityContainer,
    last_entity_id: Option<String>,
    current_entity_id: Option<String>,
    current_property_id: Option<String>,
    throttled_delay_ms: u64,
    entity_revision: VecDeque<(String, usize)>,
}

impl QuickStatementsBot {
    pub fn new(config: Arc<QuickStatements>, batch_id: Option<i64>, user_id: i64) -> Self {
        Self {
            batch_id: batch_id,
            user_id: user_id,
            config: config.clone(),
            mw_api: None,
            entities: wikibase::entity_container::EntityContainer::new(),
            last_entity_id: None,
            current_entity_id: None,
            current_property_id: None,
            throttled_delay_ms: 5000,
            entity_revision: VecDeque::new(),
        }
    }

    pub fn start(&mut self) -> Result<(), String> {
        match self.batch_id {
            Some(batch_id) => {
                let config = self.config.clone();
                config
                    .restart_batch(batch_id)
                    .ok_or("Can't (re)start batch".to_string())?;
                self.last_entity_id = config.get_last_item_from_batch(batch_id);
                match config.get_api_url(batch_id) {
                    Some(url) => {
                        let mut mw_api = wikibase::mediawiki::api::Api::new(url)
                            .map_err(|e| format!("{:?}", e))?;
                        mw_api.set_edit_delay(config.edit_delay_ms());
                        mw_api.set_maxlag(config.maxlag_s());
                        mw_api.set_max_retry_attempts(1000);
                        config.set_bot_api_auth(&mut mw_api, batch_id);
                        self.mw_api = Some(mw_api);
                    }
                    None => return Err("No site/API info available".to_string()),
                }

                config.set_batch_running(batch_id, self.user_id);
            }
            None => {
                return Err(format!("No batch ID set"));
            }
        }

        Ok(())
    }

    pub fn set_mw_api(&mut self, mw_api: wikibase::mediawiki::api::Api) {
        self.mw_api = Some(mw_api);
    }

    fn log(&self, msg: String) {
        if self.config.verbose() {
            let date = Local::now();
            let timestamp = date.format("%Y-%m-%d][%H:%M:%S");
            match self.batch_id {
                Some(id) => {
                    println!("{} Batch #{}: {}", timestamp, id, msg);
                }
                None => {
                    println!("{} No batch ID: {}", timestamp, msg);
                }
            }
        }
    }

    pub fn run(&mut self) -> Result<bool, String> {
        //Check if batch is still valid (STOP etc)
        self.log(format!("[run] Getting next command"));
        let command = match self.get_next_command() {
            Ok(c) => c,
            Err(_) => {
                match self.batch_id {
                    Some(batch_id) => {
                        self.config
                            .deactivate_batch_run(batch_id, self.user_id)
                            .ok_or("Can't set batch as stopped".to_string())?;
                    }
                    None => {}
                }
                return Ok(false);
            }
        };

        match command {
            Some(mut command) => {
                self.log(format!("[run] Executing command"));
                match self.execute_command(&mut command) {
                    Ok(_) => {}
                    Err(_message) => {} //self.set_command_status("ERROR", Some(&message), &mut command),
                }
                self.log(format!("[run] Command executed"));
                Ok(true)
            }
            None => {
                self.log(format!("[run] No more commands"));
                match self.batch_id {
                    Some(batch_id) => {
                        self.config
                            .set_batch_finished(batch_id, self.user_id)
                            .ok_or("Can't set batch as finished".to_string())?;
                    }
                    None => {}
                }
                Ok(false)
            }
        }
    }

    fn get_next_command(&self) -> Result<Option<QuickStatementsCommand>, String> {
        match self.batch_id {
            Some(batch_id) => {
                self.config.check_batch_not_stopped(batch_id)?;
                let result = self.config.get_next_command(batch_id);
                Ok(result)
            }
            None => Err(format!("No match ID set")),
        }
    }

    fn prepare_to_execute(
        self: &mut Self,
        command: &QuickStatementsCommand,
    ) -> Result<Option<wikibase::Entity>, String> {
        let command_action = command.get_action()?;
        self.log(format!("[prepare_to_execute] Action '{}'", &command_action));
        // Add/remove require the main item to be loaded
        if command_action == "add" || command_action == "remove" {
            // Reset
            self.current_property_id = command.get_entity_id_option(&command.json["property"]);
            self.current_entity_id = command.get_entity_id_option(&command.json["item"]);

            // Special case
            match command.json["what"].as_str() {
                Some(what) => {
                    if what == "statement"
                        && command.json["item"].as_str().is_none()
                        && command.json["id"].as_str().is_some()
                    {
                        match command.json["id"].as_str() {
                            Some(q) => {
                                let q = QuickStatementsCommand::fix_entity_id(q.to_string());
                                self.current_entity_id = Some(q.clone());
                            }
                            None => {}
                        }
                    }
                }
                None => {}
            }

            if self.current_entity_id == Some("LAST".to_string()) {
                self.current_entity_id = self.last_entity_id.clone();
            }
            let q = match &self.current_entity_id {
                Some(q) => q.to_string(),
                None => return Err("No (last) item available".to_string()),
            };

            let item = self.load_entity(q)?;
            Ok(Some(item.clone()))
        } else {
            Ok(None)
        }
    }

    fn load_entity(&mut self, entity_id: String) -> Result<wikibase::Entity, String> {
        let mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::get_item_from_command  has no mw_api"
        ))?;

        let revision = self
            .entity_revision
            .iter()
            .filter(|er| er.0 == entity_id)
            .map(|er| er.1)
            .nth(0);

        match self
            .entities
            .load_entity_revision(&mw_api, entity_id.to_string(), revision)
        {
            Ok(item) => Ok(item.to_owned()),
            Err(e) => self.try_create_fake_entity(entity_id, revision, e.to_string()),
        }
    }

    /// Commons MediaInfo entities have a designated ID but might not exists, yet are still good to edit.
    /// This function will try to detect this case, and temporarily create a fake entity, or return the original error
    fn try_create_fake_entity(
        &mut self,
        entity_id: String,
        revision: Option<usize>,
        original_error: String,
    ) -> Result<wikibase::Entity, String> {
        lazy_static! {
            static ref RE_MEDIA_INFO: Regex = Regex::new(r#"^M\d+$"#).expect(
                "QuickStatementsBot::try_create_fake_entity:RE_MEDIA_INFO does not compile"
            );
        }

        let mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::try_create_fake_entity has no mw_api"
        ))?;

        let the_error = Err(format!(
            "Error while loading into entities: {} rev. {:?} '{}'",
            entity_id, revision, original_error
        ));

        if revision.is_none()
            && mw_api.api_url() == COMMONS_API
            && RE_MEDIA_INFO.is_match(&entity_id)
        {
            let fake_entity = wikibase::Entity::new_mediainfo(
                entity_id.to_owned(),
                vec![],
                vec![],
                vec![],
                false,
            );
            let fake_entity_json = json!(fake_entity);
            self.entities
                .set_entity_from_json(&fake_entity_json)
                .map_err(|e| e.to_string())?;
            match self.entities.get_entity(entity_id) {
                Some(entity) => Ok(entity),
                None => the_error,
            }
        } else {
            the_error
        }
    }

    pub fn execute_command(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        self.log(format!("[execute_command] Init"));
        self.set_command_status("RUN", None, command)?;
        self.current_property_id = None;
        self.current_entity_id = None;

        self.log(format!("[execute_command] Prep"));
        command.insert_last_item_into_sources_and_qualifiers(&self.last_entity_id)?;
        let main_item = self.prepare_to_execute(command)?;
        let action = command.action_to_execute(&main_item);

        self.log(format!("[execute_command] Go"));
        match action {
            Ok(action) => match self.run_action(action, command) {
                Ok(_) => self.set_command_status("DONE", None, command),
                Err(e) => {
                    self.set_command_status("ERROR", Some(&e), command)?;
                    Err(e)
                }
            },
            Err(e) => {
                self.set_command_status("ERROR", Some(&e), command)?;
                Err(e)
            }
        }
    }

    fn reset_entities(self: &mut Self, res: &Value, command: &QuickStatementsCommand) {
        self.log(format!("[reset_entities] Init"));
        match command.json["item"].as_str() {
            Some(q) => {
                if q.to_uppercase() != "LAST" {
                    self.log(format!("[reset_entities] Start"));
                    self.last_entity_id = Some(q.to_string());
                    self.entities.remove_entity(q);
                    match res["pageinfo"]["lastrevid"].as_u64() {
                        Some(revision_id) => {
                            self.entity_revision.retain(|er| er.0 != q);
                            self.entity_revision
                                .push_front((q.to_string(), revision_id as usize));
                            self.entity_revision.truncate(5); // Keep only the last 5 around to save RAM
                        }
                        None => {}
                    }
                    self.log(format!("[reset_entities] End"));
                    return;
                }
            }
            None => {}
        }

        match &res["entity"] {
            serde_json::Value::Null => {}
            entity_json => match wikibase::entity_diff::EntityDiff::get_entity_id(&entity_json) {
                Some(q) => {
                    self.last_entity_id = Some(q.to_owned());
                    self.entities
                        .set_entity_from_json(&entity_json)
                        .expect("Setting entity from JSON failed");
                    self.entity_revision.retain(|er| er.0 != q);
                    return;
                }
                None => {}
            },
        };
    }

    fn add_summary(
        &self,
        params: &mut HashMap<String, String>,
        command: &mut QuickStatementsCommand,
    ) {
        let summary: String = format!(
            "[[:toollabs:quickstatements/#/batch/{}|batch #{}]]",
            command.batch_id, command.batch_id
        );
        let new_summary = match &params.get("summary") {
            Some(s) => s.to_string() + &"; ".to_string() + &summary,
            None => summary,
        };
        params.insert("summary".to_string(), new_summary);
    }

    fn run_action(
        self: &mut Self,
        j: Value,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        if !j["already_done"].is_null() {
            return Ok(());
        }

        self.log(format!("[run_action] Init"));

        //println!("Running action {}", &j);
        let mut params: HashMap<String, String> = HashMap::new();
        for (k, v) in j
            .as_object()
            .ok_or("QUickStatementsBot::run_action: j is not an object".to_string())?
        {
            params.insert(
                k.to_string(),
                v.as_str()
                    .ok_or(format!(
                        "QuickStatementsBot::run_action Can't as_str '{}'",
                        &v
                    ))?
                    .to_string(),
            );
        }
        self.add_summary(&mut params, command);
        self.log(format!("[run_action] Summary added"));

        // TODO baserev?
        let mut mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::run_action batch #{} has no mw_api",
            self.batch_id.unwrap_or(0)
        ))?;
        params.insert(
            "token".to_string(),
            mw_api
                .get_edit_token()
                .map_err(|e| format!("QuickStatementsBot::run_action get_edit_token '{}'", e))?,
        );

        self.log(format!("[run_action] Pre  post_query_api_json_mut"));
        let res = match mw_api.post_query_api_json_mut(&params) {
            Ok(x) => x,
            Err(e) => return Err(format!("Wiki editing failed: {:?}", e)),
        };
        self.log(format!("[run_action] Post post_query_api_json_mut"));
        //println!("{}", ::serde_json::to_string_pretty(&res).unwrap());

        lazy_static! {
            static ref RE_QUAL_OK: Regex =
                Regex::new("^The statement has already a qualifier with hash")
                    .expect("QuickStatementsBot::run_action:RE_QUAL_OK does not compile");
            static ref RE_REF_OK: Regex =
                Regex::new("^The statement has already a reference with hash")
                    .expect("QuickStatementsBot::run_action:RE_REF_OK does not compile");
        }

        match res["success"].as_i64() {
            Some(num) => {
                if num == 1 {
                    self.reset_entities(&res, command);
                    Ok(())
                } else {
                    Err(format!("Success flag is '{}' in API result", num))
                }
            }
            None => {
                match res["error"]["messages"].as_array() {
                    Some(arr) => {
                        for a in arr {
                            match a["name"].as_str() {
                                Some(s) => {
                                    if s == "actionthrottledtext" {
                                        // Throttled, try again
                                        println!(
                                            "Batch #{}: Throttled by API, sleeping {}ms",
                                            self.batch_id.unwrap_or(0),
                                            self.throttled_delay_ms
                                        );
                                        thread::sleep(time::Duration::from_millis(
                                            self.throttled_delay_ms,
                                        ));
                                        return self.run_action(j, command);
                                    }
                                }
                                None => {}
                            }
                        }
                    }
                    None => {}
                }
                match res["error"]["info"].as_str() {
                    Some(s) => {
                        command.json["meta"]["message"] = json!(s);
                        // That qualifier already exists, return OK
                        if RE_QUAL_OK.is_match(s) {
                            return Ok(());
                        }
                        // That reference already exists, return OK
                        if RE_REF_OK.is_match(s) {
                            return Ok(());
                        }
                    }
                    None => {}
                }
                println!("\nCOMMAND ERROR #{}:\n{:?}\n{}", &command.id, &params, &res);
                Err("No success flag set in API result".to_string())
            }
        }
    }

    fn set_command_status(
        self: &mut Self,
        status: &str,
        message: Option<&str>,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        if status == "DONE" {
            self.last_entity_id = self.current_entity_id.clone();
        }
        if self.batch_id.is_none() {
            return Ok(());
        }

        self.config
            .set_command_status(command, status, message.map(|s| s.to_string()))
            .ok_or(format!(
                "Can't config.set_command_status for batch #{}",
                self.batch_id.unwrap() //Safe
            ))?;
        self.config
            .set_last_item_for_batch(self.batch_id.unwrap(), &self.last_entity_id) // unwrap safe
            .ok_or(format!(
                "Can't config.set_command_status for batch #{}",
                self.batch_id.unwrap() //Safe
            ))?;

        Ok(())
    }
}
