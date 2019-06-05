use crate::qs_command::QuickStatementsCommand;
use crate::qs_config::QuickStatements;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wikibase;

#[derive(Debug, Clone)]
pub struct QuickStatementsBot {
    batch_id: i64,
    user_id: i64,
    config: Arc<Mutex<QuickStatements>>,
    mw_api: Option<mediawiki::api::Api>,
    entities: wikibase::entity_container::EntityContainer,
    last_entity_id: Option<String>,
    current_entity_id: Option<String>,
    current_property_id: Option<String>,
}

impl QuickStatementsBot {
    pub fn new(config: Arc<Mutex<QuickStatements>>, batch_id: i64, user_id: i64) -> Self {
        Self {
            batch_id: batch_id,
            user_id: user_id,
            config: config.clone(),
            mw_api: None,
            entities: wikibase::entity_container::EntityContainer::new(),
            last_entity_id: None,
            current_entity_id: None,
            current_property_id: None,
        }
    }

    pub fn start(self: &mut Self) -> Result<(), String> {
        let mut config = self.config.lock().map_err(|e| format!("{:?}", e))?;
        config
            .restart_batch(self.batch_id)
            .ok_or("Can't (re)start batch".to_string())?;
        self.last_entity_id = config.get_last_item_from_batch(self.batch_id);
        match config.get_api_url(self.batch_id) {
            Some(url) => {
                let mut mw_api = mediawiki::api::Api::new(url).map_err(|e| format!("{:?}", e))?;
                mw_api.set_edit_delay(Some(1000)); // 1000ms=1sec
                config.set_bot_api_auth(&mut mw_api, self.batch_id);
                self.mw_api = Some(mw_api);
            }
            None => return Err("No site/API info available".to_string()),
        }

        config.set_batch_running(self.batch_id, self.user_id);
        Ok(())
    }

    pub fn run(self: &mut Self) -> Result<bool, String> {
        //println!("Batch #{}: doing stuff", self.batch_id);
        match self.get_next_command() {
            Some(mut command) => {
                match self.execute_command(&mut command) {
                    Ok(_) => {}
                    Err(_message) => {}//self.set_command_status("ERROR", Some(&message), &mut command),
                }
                Ok(true)
            }
            None => {
                let mut config = self.config.lock().map_err(|e| format!("{:?}", e))?;
                config
                    .set_batch_finished(self.batch_id, self.user_id)
                    .ok_or("Can't set batch as finished".to_string())?;
                Ok(false)
            }
        }
    }

    fn get_next_command(&self) -> Option<QuickStatementsCommand> {
        let mut config = self.config.lock().ok()?;
        config.get_next_command(self.batch_id)
    }

    fn load_main_command_item(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<Option<wikibase::Entity>, String> {
        let command_action = command.get_action()?;
        // Add/remove require the main item to be loaded
        if command_action == "add" || command_action == "remove" {
            self.load_command_items(command);
            if self.current_entity_id.is_none() {
                return Err("No (last) item available".to_string());
            }
            let ret = self.get_main_item(command)?;
            Ok(Some(ret))
        } else {
            Ok(None)
        }
    }

    fn execute_command(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        self.set_command_status("RUN", None, command)?;
        self.current_property_id = None;
        self.current_entity_id = None;

        command.insert_last_item_into_sources_and_qualifiers(&self.last_entity_id)?;
        let main_item = self.load_main_command_item(command)?;
        let action = command.action_to_execute(&main_item);

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

    pub fn get_main_item(
        &mut self,
        command: &mut QuickStatementsCommand,
    ) -> Result<wikibase::Entity, String> {
        let q = match command.json["item"].as_str() {
            Some(q) => q.to_string(),
            None => return Err("Item expected but not set".to_string()),
        };
        let mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::get_item_from_command batch #{} has no mw_api",
            command.batch_id
        ))?;
        //println!("LOADING ENTITY {}", &q);
        match self.entities.load_entities(&mw_api, &vec![q.to_owned()]) {
            Ok(_) => {}
            Err(_e) => {
                //println!("ERROR: {:?}", &e);
                return Err("Error while loading into entities".to_string());
            }
        }

        let i = match self.entities.get_entity(q) {
            Some(i) => i,
            None => return Err("Failed to get item".to_string()),
        };
        Ok(i.clone())
    }

    fn reset_entities(self: &mut Self, res: &Value, command: &QuickStatementsCommand) {
        match command.json["item"].as_str() {
            Some(q) => {
                if q.to_uppercase() != "LAST" {
                    self.last_entity_id = Some(q.to_string());
                    self.entities.remove_entity(q);
                    return;
                }
            }
            None => {}
        }

        match &res["entity"] {
            serde_json::Value::Null => {}
            entity_json => match wikibase::entity_diff::EntityDiff::get_entity_id(&entity_json) {
                Some(q) => {
                    self.last_entity_id = Some(q);
                    self.entities
                        .set_entity_from_json(&entity_json)
                        .expect("Setting entity from JSON failed");
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
        // TODO baserev?
        let mut mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::run_action batch #{} has no mw_api",
            self.batch_id
        ))?;
        params.insert(
            "token".to_string(),
            mw_api
                .get_edit_token()
                .map_err(|e| format!("QuickStatementsBot::run_action get_edit_token '{}'", e))?,
        );

        let res = match mw_api.post_query_api_json_mut(&params) {
            Ok(x) => x,
            Err(_e) => return Err("Wikidata editing fail".to_string()),
        };

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
                println!("\nCOMMAND ERROR #{}:\n{:?}\n{}", &command.id, &params, &res);
                match res["error"]["info"].as_str() {
                    Some(s) => {
                        command.json["meta"]["message"] = json!(s);
                    }
                    None => {}
                }
                Err("No success flag set in API result".to_string())
            }
        }
    }

    fn load_command_items(self: &mut Self, command: &mut QuickStatementsCommand) {
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
        match &self.current_entity_id {
            Some(q) => command.json["item"] = Value::from(q.clone()),
            None => {}
        }

        //println!("Q:{:?} / P:{:?}",&self.current_entity_id, &self.current_property_id);
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

        let mut config = self.config.lock().map_err(|e| format!("{:?}", e))?;
        config
            .set_command_status(command, status, message.map(|s| s.to_string()))
            .ok_or(format!(
                "Can't config.set_command_status for batch #{}",
                self.batch_id
            ))?;
        config
            .set_last_item_for_batch(self.batch_id, &self.last_entity_id)
            .ok_or(format!(
                "Can't config.set_command_status for batch #{}",
                self.batch_id
            ))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
