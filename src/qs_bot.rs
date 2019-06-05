use crate::qs_command::QuickStatementsCommand;
use crate::qs_config::QuickStatements;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wikibase;

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

        config.set_batch_running(self.batch_id);
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
                    .set_batch_finished(self.batch_id)
                    .ok_or("Can't set batch as finished".to_string())?;
                Ok(false)
            }
        }
    }

    fn get_next_command(&self) -> Option<QuickStatementsCommand> {
        let mut config = self.config.lock().ok()?;
        config.get_next_command(self.batch_id)
    }

    fn create_new_entity(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        let data = match &command.json["data"].as_object() {
            Some(_) => match serde_json::to_string(&command.json["data"]) {
                Ok(s) => s,
                _ => "{}".to_string(),
            },
            None => "{}".to_string(),
        };
        let new_type = match command.json["type"].as_str() {
            Some(t) => t,
            None => return Err("No type set".to_string()),
        };
        self.run_action(
            json!({
                "action":"wbeditentity",
                "new":new_type,
                "data":data,
            }),
            command,
        ) // baserevid?
    }

    fn merge_entities(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        let item1 = match command.json["item1"].as_str() {
            Some(t) => t,
            None => return Err("item1 not set".to_string()),
        };
        let item2 = match command.json["item2"].as_str() {
            Some(t) => t,
            None => return Err("item2 not set".to_string()),
        };
        self.run_action(
            json!({
                "action":"wbmergeitems",
                "fromid":item1,
                "toid":item2,
                "ignoreconflicts":"description"
            }),
            command,
        ) // baserevid?
    }

    fn set_label(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        let i = self.get_item_from_command(command)?.to_owned();
        let language = command.json["language"]
            .as_str()
            .ok_or("Can't find language".to_string())?;
        let text = command.json["value"]
            .as_str()
            .ok_or("Can't find text (=value)".to_string())?;
        match i.label_in_locale(language) {
            Some(s) => {
                if s == text {
                    return Ok(());
                }
            }
            None => {}
        }
        self.run_action(json!({"action":"wbsetlabel","id":self.get_prefixed_id(i.id()),"language":language,"value":text}),command) // baserevid?
    }

    fn add_alias(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        let i = self.get_item_from_command(command)?.to_owned();
        let language = command.json["language"]
            .as_str()
            .ok_or("Can't find language".to_string())?;
        let text = command.json["value"]
            .as_str()
            .ok_or("Can't find text (=value)".to_string())?;
        self.run_action(json!({"action":"wbsetaliases","id":self.get_prefixed_id(i.id()),"language":language,"add":text}),command) // baserevid?
    }

    fn set_description(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        let i = self.get_item_from_command(command)?.to_owned();
        let language = command.json["language"]
            .as_str()
            .ok_or("Can't find language".to_string())?;
        let text = command.json["value"]
            .as_str()
            .ok_or("Can't find text (=value)".to_string())?;
        match i.description_in_locale(language) {
            Some(s) => {
                if s == text {
                    return Ok(());
                }
            }
            None => {}
        }
        self.run_action(json!({"action":"wbsetdescription","id":self.get_prefixed_id(i.id()),"language":language,"value":text}),command) // baserevid?
    }

    fn set_sitelink(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let i = self.get_item_from_command(command)?.to_owned();
        let site = match &command.json["site"].as_str() {
            Some(s) => s.to_owned(),
            None => return Err("site not set".to_string()),
        };
        let title = match &command.json["value"].as_str() {
            Some(s) => s.to_owned(),
            None => return Err("value (title) not set".to_string()),
        };

        // Check if this same sitelink is already set
        match i.sitelinks() {
            Some(sitelinks) => {
                let title_underscores = title.replace(" ", "_");
                for sl in sitelinks {
                    if sl.site() == site && sl.title().replace(" ", "_") == title_underscores {
                        return Ok(());
                    }
                }
            }
            None => {}
        }

        self.run_action(
            json!({
                "action":"wbsetsitelink",
                "id":self.get_prefixed_id(i.id()),
                "linksite":site,
                "linktitle":title,
            }),
            command,
        ) // baserevid?
    }

    fn add_statement(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let i = self.get_item_from_command(command)?.to_owned();

        let property = match command.json["property"].as_str() {
            Some(p) => p.to_owned(),
            None => return Err("Property not found".to_string()),
        };
        let value = serde_json::to_string(&command.json["datavalue"]["value"])
            .map_err(|e| format!("{:?}", e))?;

        match self.get_statement_id(command)? {
            Some(_statement_id) => {
                //println!("Such a statement already exists as {}", &statement_id);
                return Ok(());
            }
            None => {}
        }

        self.run_action(
            json!({
                "action":"wbcreateclaim",
                "entity":self.get_prefixed_id(i.id()),
                "snaktype":self.get_snak_type_for_datavalue(&command.json["datavalue"])?,
                "property":property,
                "value":value
            }),
            command,
        ) // baserevid?
    }

    fn get_snak_type_for_datavalue(&self, dv: &Value) -> Result<String, String> {
        if dv["value"].as_object().is_some() {
            return Ok("value".to_string());
        }
        let ret = match &dv["value"].as_str() {
            Some("novalue") => "novalue",
            Some("somevalue") => "somevalue",
            Some(_) => "value",
            None => return Err(format!("Cannot determine snak type: {}", dv)),
        };
        Ok(ret.to_string())
    }

    fn check_prop(&self, s: &str) -> Result<String, String> {
        lazy_static! {
            static ref RE_PROP: Regex = Regex::new(r#"^P\d+$"#)
                .expect("QuickStatementsBot::check_prop:RE_PROP does not compile");
        }
        match RE_PROP.is_match(s) {
            true => Ok(s.to_string()),
            false => Err(format!("'{}' is not a property", &s)),
        }
    }

    fn add_qualifier(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let statement_id = match self.get_statement_id(command)? {
            Some(id) => id,
            None => {
                return Err(format!(
                    "add_qualifier: Could not get statement ID for {:?}",
                    command
                ))
            }
        };

        let qual_prop = match command.json["qualifier"]["prop"].as_str() {
            Some(p) => self.check_prop(p)?,
            None => return Err("Incomplete command parameters: prop".to_string()),
        };

        let qual_value = &command.json["qualifier"]["value"]["value"];
        if !qual_value.is_string() && !qual_value.is_object() {
            return Err("Incomplete command parameters: value.value".to_string());
        }

        self.run_action(
            json!({
                "action":"wbsetqualifier",
                "claim":statement_id,
                "property":qual_prop,
                "value":serde_json::to_string(&qual_value).map_err(|e|format!("{:?}",e))?,
                "snaktype":self.get_snak_type_for_datavalue(&command.json["qualifier"])?,
            }),
            command,
        ) // baserevid?
    }

    fn add_sources(self: &mut Self, command: &mut QuickStatementsCommand) -> Result<(), String> {
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        let statement_id = match self.get_statement_id(command)? {
            Some(id) => id,
            None => {
                return Err(format!(
                    "add_sources: Could not get statement ID for {:?}",
                    command
                ))
            }
        };
        //println!("SOURCES:{}", &command.json["sources"]);

        let snaks = match &command.json["sources"].as_array() {
            Some(sources) => {
                let mut snaks = json!({});
                for source in sources.iter() {
                    //println!("SOURCE: {}", &source);
                    let prop = match source["prop"].as_str() {
                        Some(prop) => prop,
                        None => return Err("No prop value in source".to_string()),
                    };
                    let prop = self.check_prop(prop)?;
                    let snaktype = self.get_snak_type_for_datavalue(&source)?;
                    let snaktype = snaktype.to_owned();
                    let snak = match snaktype.as_str() {
                        "value" => json!({
                            "property":prop.to_owned(),
                            "snaktype":"value",
                            "datavalue":source["value"],
                        }),
                        other => json!({
                            "property":prop.to_owned(),
                            "snaktype":other,
                        }),
                    };
                    if snaks[&prop].as_array().is_none() {
                        snaks[&prop] = json!([]);
                    }
                    snaks[prop]
                        .as_array_mut()
                        .ok_or(
                            "QuickStatementsBot::add_sources snaks[prop] does not as_array_mut()"
                                .to_string(),
                        )?
                        .push(snak);
                }
                snaks
            }
            None => return Err("Incomplete command parameters: sources".to_string()),
        };

        self.run_action(
            json!({
                "action":"wbsetreference",
                "statement":statement_id,
                "snaks":serde_json::to_string(&snaks).map_err(|e|format!("{:?}",e))?,
            }),
            command,
        ) // baserevid?
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

    fn get_prefixed_id(&self, s: &str) -> String {
        s.to_string() // TODO necessary?
    }

    fn is_same_datavalue(&self, dv1: &wikibase::DataValue, dv2: &Value) -> Option<bool> {
        lazy_static! {
            static ref RE_TIME: Regex = Regex::new("^(?P<a>[+-]{0,1})0*(?P<b>.+)$")
                .expect("QuickStatementsBot::is_same_datavalue:RE_TIME does not compile");
        }

        if dv1.value_type().string_value() != dv2["type"].as_str()? {
            return Some(false);
        }

        let v2 = &dv2["value"];
        match dv1.value() {
            wikibase::Value::Coordinate(v) => Some(
                v.globe() == v2["globe"].as_str()?
                    && *v.latitude() == v2["latitude"].as_f64()?
                    && *v.longitude() == v2["longitude"].as_f64()?,
            ),
            wikibase::Value::MonoLingual(v) => Some(
                v.language() == v2["language"].as_str()?
                    && self.normalize_string(&v.text().to_string())
                        == self.normalize_string(&v2["text"].as_str()?.to_string()),
            ),
            wikibase::Value::Entity(v) => Some(v.id() == v2["id"].as_str()?),
            wikibase::Value::Quantity(v) => {
                Some(*v.amount() == v2["amount"].as_str()?.parse::<f64>().ok()?)
            }
            wikibase::Value::StringValue(v) => Some(
                self.normalize_string(&v.to_string())
                    == self.normalize_string(&v2.as_str()?.to_string()),
            ),
            wikibase::Value::Time(v) => {
                let t1 = RE_TIME.replace_all(v.time(), "$a$b");
                let t2 = RE_TIME.replace_all(v2["time"].as_str()?, "$a$b");
                Some(v.calendarmodel() == v2["calendarmodel"].as_str()? && t1 == t2)
            }
        }
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
        let mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::get_item_from_command batch #{} has no mw_api",
            self.batch_id
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
        Ok(i)
    }

    fn get_statement_id(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<Option<String>, String> {
        let i = self.get_item_from_command(command)?.to_owned();

        let property = match command.json["property"].as_str() {
            Some(p) => p,
            None => return Err("Property expected but not set".to_string()),
        };
        /*
        let datavalue = match command.json["datavalue"].as_object() {
            Some(dv) => dv,
            None => return Err("Datavalue expected but not set".to_string()),
        };
        */

        for claim in i.claims() {
            if claim.main_snak().property() != property {
                continue;
            }
            let dv = match claim.main_snak().data_value() {
                Some(dv) => dv,
                None => continue,
            };
            //println!("!!{:?} : {:?}", &dv, &datavalue);
            match self.is_same_datavalue(&dv, &command.json["datavalue"]) {
                Some(b) => {
                    if b {
                        let id = claim
                            .id()
                            .ok_or(format!(
                                "QuickStatementsBot::get_statement_id batch #{} command {:?}",
                                &self.batch_id, &command
                            ))?
                            .to_string();
                        //println!("Using statement ID '{}'", &id);
                        return Ok(Some(id));
                    }
                }
                None => continue,
            }
        }
        Ok(None)
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
                    let id = self.last_entity_id.clone().expect(
                        "QuickStatementsBot::replace_last_item: can't clone last_entity_id",
                    );
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

        /*
        println!(
            "{:?}/{:?}",
            &self.current_entity_id, &self.current_property_id
        );
        println!("{}", &command.json);
        */

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

    fn remove_statement(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        let statement_id = match self.get_statement_id(command)? {
            Some(id) => id,
            None => return Err("remove_statement: Statement not found".to_string()),
        };
        //println!("remove_statement: Using statement ID {}", &statement_id);
        self.run_action(
            json!({"action":"wbremoveclaims","claim":statement_id}),
            command,
        ) // baserevid?
    }

    fn remove_sitelink(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        command.json["value"] = json!("");
        self.set_sitelink(command)
    }

    fn remove_from_entity(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        self.load_command_items(command);
        self.insert_last_item_into_sources_and_qualifiers(command)?;
        if self.current_entity_id.is_none() {
            return Err("No (last) item available".to_string());
        }

        match command.json["what"].as_str() {
            Some("statement") => self.remove_statement(command),
            Some("sitelink") => self.remove_sitelink(command),
            _other => Err("Bad 'what'".to_string()),
        }
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
                    match command.json["id"].as_str() {
                        Some(q) => {
                            let q = self.fix_entity_id(q.to_string());
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

    fn fix_entity_id(&self, id: String) -> String {
        id.trim().to_uppercase()
    }

    fn execute_command(
        self: &mut Self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        self.set_command_status("RUN", None, command)?;
        self.current_property_id = None;
        self.current_entity_id = None;

        let result = match command.json["action"].as_str().unwrap_or("") {
            "create" => self.create_new_entity(command),
            "merge" => self.merge_entities(command),
            "add" => self.add_to_entity(command),
            "remove" => self.remove_from_entity(command),
            other => Err(format!("Unknown action '{}'", &other)),
        };

        match &result {
            Err(message) => self.set_command_status("ERROR", Some(message), command),
            _ => self.set_command_status("DONE", None, command),
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
