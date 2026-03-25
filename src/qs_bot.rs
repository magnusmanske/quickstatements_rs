use crate::qs_command::{LastEntityState, QuickStatementsCommand};
use crate::qs_config::QuickStatements;
use crate::qs_parser::COMMONS_API;
use chrono::Local;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use wikibase;

#[derive(Debug, Clone)]
pub struct QuickStatementsBot {
    batch_id: Option<i64>,
    user_id: i64,
    config: Arc<QuickStatements>,
    mw_api: Option<wikibase::mediawiki::api::Api>,
    pub entities: wikibase::entity_container::EntityContainer,
    last_state: LastEntityState,
    current_entity_id: Option<String>,
    current_property_id: Option<String>,
    throttled_delay_ms: u64,
    entity_revision: VecDeque<(String, usize)>,
}

impl QuickStatementsBot {
    pub fn new(config: Arc<QuickStatements>, batch_id: Option<i64>, user_id: i64) -> Self {
        Self {
            batch_id,
            user_id,
            config,
            mw_api: None,
            entities: wikibase::entity_container::EntityContainer::new(),
            last_state: LastEntityState::default(),
            current_entity_id: None,
            current_property_id: None,
            throttled_delay_ms: 5000,
            entity_revision: VecDeque::new(),
        }
    }

    pub async fn start(&mut self) -> Result<(), String> {
        match self.batch_id {
            Some(batch_id) => {
                let config = self.config.clone();
                config
                    .restart_batch(batch_id)
                    .await
                    .ok_or("Can't (re)start batch".to_string())?;
                self.last_state = config.get_last_state_from_batch(batch_id).await;
                match config.get_api_url(batch_id).await {
                    Some(url) => {
                        let mut mw_api = wikibase::mediawiki::api::Api::new(url)
                            .await
                            .map_err(|e| format!("{:?}", e))?;
                        mw_api.set_edit_delay(config.edit_delay_ms());
                        mw_api.set_maxlag(config.maxlag_s());
                        mw_api.set_max_retry_attempts(1000);
                        config.set_bot_api_auth(&mut mw_api, batch_id).await;
                        self.mw_api = Some(mw_api);
                    }
                    None => return Err("No site/API info available".to_string()),
                }

                config.set_batch_running(batch_id, self.user_id).await;
            }
            None => {
                return Err("No batch ID set".to_string());
            }
        }

        Ok(())
    }

    pub fn set_mw_api(&mut self, mw_api: wikibase::mediawiki::api::Api) {
        self.mw_api = Some(mw_api);
    }

    pub fn set_last_state(&mut self, state: LastEntityState) {
        self.last_state = state;
    }

    /// Execute a command for debugging: prepare params, call the API, and return
    /// both the request params and the full API response (or error).
    pub async fn debug_command(
        &mut self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(HashMap<String, String>, Value), String> {
        command.insert_last_item_into_sources_and_qualifiers(&self.last_state)?;
        let main_item = self.prepare_to_execute(command).await?;
        let action = command.action_to_execute(&main_item)?;

        if !action["already_done"].is_null() {
            return Err("Command is already_done (duplicate)".to_string());
        }

        let mut params: HashMap<String, String> = HashMap::new();
        for (k, v) in action
            .as_object()
            .ok_or("Action is not a JSON object")?
        {
            params.insert(
                k.to_string(),
                v.as_str()
                    .ok_or(format!("Cannot convert param '{}' value to string: {}", k, v))?
                    .to_string(),
            );
        }
        self.add_summary(&mut params, command);

        // Actually execute the API call
        let mut mw_api = self.mw_api.to_owned().ok_or("No mw_api set")?;
        params.insert(
            "token".to_string(),
            mw_api
                .get_edit_token()
                .await
                .map_err(|e| format!("get_edit_token: {}", e))?,
        );

        let response = match mw_api.post_query_api_json_mut(&params).await {
            Ok(json) => json,
            Err(e) => {
                // Return the error as a JSON value so the caller can still see the params
                serde_json::json!({"_error": format!("{:?}", e)})
            }
        };

        // Don't put the token in the debug output
        params.remove("token");

        Ok((params, response))
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

    pub async fn run(&mut self) -> Result<bool, String> {
        //Check if batch is still valid (STOP etc)
        self.log("[run] Getting next command".to_string());
        let command = match self.get_next_command().await {
            Ok(c) => c,
            Err(_) => {
                if let Some(batch_id) = self.batch_id {
                    self.config
                        .deactivate_batch_run(batch_id, self.user_id)
                        .ok_or("Can't set batch as stopped".to_string())?;
                }
                return Ok(false);
            }
        };

        match command {
            Some(mut command) => {
                self.log("[run] Executing command".to_string());
                match self.execute_command(&mut command).await {
                    Ok(_) => {}
                    Err(_message) => {} //self.set_command_status("ERROR", Some(&message), &mut command),
                }
                self.log("[run] Command executed".to_string());
                Ok(true)
            }
            None => {
                self.log("[run] No more commands".to_string());
                if let Some(batch_id) = self.batch_id {
                    self.config
                        .set_batch_finished(batch_id, self.user_id)
                        .await
                        .ok_or("Can't set batch as finished".to_string())?;
                }
                Ok(false)
            }
        }
    }

    async fn get_next_command(&self) -> Result<Option<QuickStatementsCommand>, String> {
        match self.batch_id {
            Some(batch_id) => {
                self.config.check_batch_not_stopped(batch_id).await?;
                let result = self.config.get_next_command(batch_id).await;
                Ok(result)
            }
            None => Err("No match ID set".to_string()),
        }
    }

    /// Returns true if this command type operates on lexeme sub-entities
    /// and does not require loading the main entity from the API.
    fn is_lexeme_subentity_command(command: &QuickStatementsCommand) -> bool {
        matches!(
            command.json["what"].as_str(),
            Some("lemma") | Some("lexical_category")
            | Some("language") | Some("representation") | Some("grammatical_feature")
            | Some("gloss")
        )
    }

    /// Resolve LAST / LAST_FORM / LAST_SENSE in current_entity_id using last_state.
    fn resolve_current_entity_id(&mut self) {
        if let Some(ref id) = self.current_entity_id {
            let upper = id.to_uppercase();
            if let Some(resolved) = self.last_state.resolve(&upper) {
                self.current_entity_id = Some(resolved.clone());
            }
        }
    }

    async fn prepare_to_execute(
        &mut self,
        command: &QuickStatementsCommand,
    ) -> Result<Option<wikibase::Entity>, String> {
        let command_action = command.get_action()?;
        self.log(format!("[prepare_to_execute] Action '{}'", &command_action));
        // Form/sense creation: resolve LAST but don't load entity
        if command_action == "create" {
            if let Some(entity_type) = command.json["type"].as_str() {
                if entity_type == "form" || entity_type == "sense" {
                    self.current_entity_id = command.get_entity_id_option(&command.json["item"]);
                    self.resolve_current_entity_id();
                    return Ok(None);
                }
            }
        }

        // Add/remove require the main item to be loaded
        if command_action == "add" || command_action == "remove" {
            // Reset
            self.current_property_id = command.get_entity_id_option(&command.json["property"]);
            self.current_entity_id = command.get_entity_id_option(&command.json["item"]);

            // Lexeme sub-entity commands don't need to load the entity
            if Self::is_lexeme_subentity_command(command) {
                self.resolve_current_entity_id();
                return Ok(None);
            }

            // Special case
            if let Some(what) = command.json["what"].as_str() {
                if what == "statement"
                    && command.json["item"].as_str().is_none()
                    && command.json["id"].as_str().is_some()
                {
                    if let Some(q) = command.json["id"].as_str() {
                        let q = QuickStatementsCommand::fix_entity_id(q.to_string());
                        self.current_entity_id = Some(q.clone());
                    }
                }
            }

            self.resolve_current_entity_id();
            let q = match &self.current_entity_id {
                Some(q) => q.to_string(),
                None => return Err("No (last) item available".to_string()),
            };

            let item = self.load_entity(q).await?;
            Ok(Some(item.clone()))
        } else {
            Ok(None)
        }
    }

    async fn load_entity(&mut self, entity_id: String) -> Result<wikibase::Entity, String> {
        let mw_api = self
            .mw_api
            .to_owned()
            .ok_or("QuickStatementsBot::get_item_from_command  has no mw_api".to_string())?;

        let revision = self
            .entity_revision
            .iter()
            .filter(|er| er.0 == entity_id)
            .map(|er| er.1)
            .next();

        match self
            .entities
            .load_entity_revision(&mw_api, entity_id.to_string(), revision)
            .await
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

        let mw_api = self
            .mw_api
            .to_owned()
            .ok_or("QuickStatementsBot::try_create_fake_entity has no mw_api".to_string())?;

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

    async fn check_if_user_is_blocked(
        &self,
        command: &mut QuickStatementsCommand,
    ) -> Result<bool, String> {
        // Only check randomly every 20 commands to keep API load down
        if command.id % 20 != 0 {
            return Ok(false);
        }

        // Get user name
        let user_name = self
            .config
            .get_user_name(self.user_id)
            .await
            .ok_or("User not found".to_string())?;

        // Get MediaWiki API
        let api_url = self
            .config
            .get_api_url(self.batch_id.unwrap_or_default())
            .await
            .ok_or_else(|| "API URL not found".to_string())?;
        let mut mw_api = wikibase::mediawiki::api::Api::new(api_url)
            .await
            .map_err(|e| format!("{:?}", e))?;

        // Check if user has a blockid
        QuickStatements::is_user_blocked(&mut mw_api, &user_name).await
    }

    pub async fn execute_command(
        &mut self,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        if Ok(true) == self.check_if_user_is_blocked(command).await {
            let _ = self.set_command_status("BLOCKED", None, command).await;
            let _ = self
                .config
                .set_batch_status("BLOCKED", "", self.batch_id.unwrap_or(0), self.user_id)
                .await;
            return Err("User is blocked".to_string());
        }
        self.log("[execute_command] Init".to_string());
        self.set_command_status("RUN", None, command).await?;
        self.current_property_id = None;
        self.current_entity_id = None;

        self.log("[execute_command] Prep".to_string());
        command.insert_last_item_into_sources_and_qualifiers(&self.last_state)?;
        let main_item = self.prepare_to_execute(command).await?;
        let action = command.action_to_execute(&main_item);

        self.log("[execute_command] Go".to_string());
        match action {
            Ok(action) => match self.run_action(action, command).await {
                Ok(_) => self.set_command_status("DONE", None, command).await,
                Err(e) => {
                    self.set_command_status("ERROR", Some(&e), command).await?;
                    Err(e)
                }
            },
            Err(e) => {
                self.set_command_status("ERROR", Some(&e), command).await?;
                Err(e)
            }
        }
    }

    fn reset_entities(&mut self, res: &Value, command: &QuickStatementsCommand) {
        self.log("[reset_entities] Init".to_string());

        // Extract the command type to determine LAST_FORM / LAST_SENSE tracking
        let command_type = command.json["type"].as_str().unwrap_or("");
        let command_action = command.json["action"].as_str().unwrap_or("");

        // For ADD_FORM: extract new form ID from the API response
        if command_type == "form" && command_action == "create" {
            if let Some(form_id) = res["form"]["id"].as_str() {
                self.last_state.last_form = Some(form_id.to_string());
                self.log(format!("[reset_entities] LAST_FORM = {}", form_id));
            }
        }

        // For ADD_SENSE: extract new sense ID from the API response
        if command_type == "sense" && command_action == "create" {
            if let Some(sense_id) = res["sense"]["id"].as_str() {
                self.last_state.last_sense = Some(sense_id.to_string());
                self.log(format!("[reset_entities] LAST_SENSE = {}", sense_id));
            }
        }

        if let Some(q) = command.json["item"].as_str() {
            let upper = q.to_uppercase();
            if !matches!(upper.as_str(), "LAST" | "LAST_FORM" | "LAST_SENSE") {
                self.log("[reset_entities] Start".to_string());
                self.last_state.last = Some(q.to_string());
                self.entities.remove_entity(q);
                if let Some(revision_id) = res["pageinfo"]["lastrevid"].as_u64() {
                    self.entity_revision.retain(|er| er.0 != q);
                    self.entity_revision
                        .push_front((q.to_string(), revision_id as usize));
                    self.entity_revision.truncate(5); // Keep only the last 5 around to save RAM
                }
                self.log("[reset_entities] End".to_string());
                return;
            }
        }

        match &res["entity"] {
            serde_json::Value::Null => {}
            entity_json => {
                if let Some(q) = wikibase::entity_diff::EntityDiff::get_entity_id(entity_json) {
                    self.last_state.last = Some(q.to_owned());
                    // CREATE / CREATE_LEXEME: clear LAST_FORM and LAST_SENSE
                    if command_action == "create"
                        && !matches!(command_type, "form" | "sense")
                    {
                        self.last_state.last_form = None;
                        self.last_state.last_sense = None;
                    }
                    self.entities
                        .set_entity_from_json(entity_json)
                        .expect("Setting entity from JSON failed");
                    self.entity_revision.retain(|er| er.0 != q);
                }
            }
        }
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
            Some(s) => s.to_string() + "; " + &summary,
            None => summary,
        };
        params.insert("summary".to_string(), new_summary);
    }

    async fn run_action(
        &mut self,
        j: Value,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        if !j["already_done"].is_null() {
            return Ok(());
        }

        self.log("[run_action] Init".to_string());

        //println!("Running action {}", &j);
        let mut params: HashMap<String, String> = HashMap::new();
        for (k, v) in j
            .as_object()
            .ok_or("QuickStatementsBot::run_action: j is not an object".to_string())?
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
        self.log("[run_action] Summary added".to_string());

        // TODO baserev?
        let mut mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::run_action batch #{} has no mw_api",
            self.batch_id.unwrap_or(0)
        ))?;
        let mut invalid_json_retries = 3usize;
        loop {
            params.insert(
                "token".to_string(),
                mw_api.get_edit_token().await.map_err(|e| {
                    format!("QuickStatementsBot::run_action get_edit_token '{}'", e)
                })?,
            );

            self.log("[run_action] Pre  post_query_api_json_mut".to_string());
            let res = match mw_api.post_query_api_json_mut(&params).await {
                Ok(x) => x,
                Err(wikibase::mediawiki::MediaWikiError::Serde(_)) if invalid_json_retries > 0 => {
                    // API returned non-JSON (e.g. HTML rate-limit page from CDN) — retry after delay
                    invalid_json_retries -= 1;
                    self.log(format!(
                        "[run_action] Non-JSON API response, retrying ({} retries left)",
                        invalid_json_retries
                    ));
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                Err(e) => return Err(format!("Wiki editing failed: {:?}", e)),
            };
            self.log("[run_action] Post post_query_api_json_mut".to_string());

            let res = self.check_run_action_result(res, &params, command)?;
            if !res {
                return Ok(());
            }

            tokio::time::sleep(Duration::from_millis(self.throttled_delay_ms)).await;
        }
    }

    /// Checks the command result, returns Ok(true) to re-do, Ok(false) if done, Err otherwise
    fn check_run_action_result(
        &mut self,
        res: Value,
        params: &HashMap<String, String>,
        command: &mut QuickStatementsCommand,
    ) -> Result<bool, String> {
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
                    Ok(false)
                } else {
                    Err(format!("Success flag is '{}' in API result", num))
                }
            }
            None => {
                // Check for rate limiting / throttle — handle both old and new Wikimedia error formats
                let error_code = res["error"]["code"].as_str().unwrap_or("");
                if matches!(error_code, "ratelimited" | "actionthrottled") {
                    println!(
                        "Batch #{}: Rate limited by API (code: {}), sleeping {}ms",
                        self.batch_id.unwrap_or(0),
                        error_code,
                        self.throttled_delay_ms
                    );
                    return Ok(true);
                }
                if let Some(arr) = res["error"]["messages"].as_array() {
                    for a in arr {
                        if let Some(s) = a["name"].as_str() {
                            if matches!(s, "actionthrottledtext" | "ratelimited") {
                                // Throttled, try again
                                println!(
                                    "Batch #{}: Throttled by API, sleeping {}ms",
                                    self.batch_id.unwrap_or(0),
                                    self.throttled_delay_ms
                                );
                                return Ok(true);
                            }
                        }
                    }
                }
                if let Some(s) = res["error"]["info"].as_str() {
                    command.json["meta"]["message"] = json!(s);
                    // That qualifier already exists, return OK
                    if RE_QUAL_OK.is_match(s) {
                        return Ok(false);
                    }
                    // That reference already exists, return OK
                    if RE_REF_OK.is_match(s) {
                        return Ok(false);
                    }
                }
                println!("\nCOMMAND ERROR #{}:\n{:?}\n{}", &command.id, &params, &res);
                Err("No success flag set in API result".to_string())
            }
        }
    }

    async fn set_command_status(
        &mut self,
        status: &str,
        message: Option<&str>,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        if status == "DONE" {
            self.last_state.last = self.current_entity_id.clone();
        }
        if self.batch_id.is_none() {
            return Ok(());
        }

        self.config
            .set_command_status(command, status, message.map(|s| s.to_string()))
            .await
            .ok_or(format!(
                "Can't config.set_command_status for batch #{}",
                self.batch_id.unwrap() //Safe
            ))?;
        self.config
            .set_last_state_for_batch(self.batch_id.unwrap(), &self.last_state) // unwrap safe
            .await
            .ok_or(format!(
                "Can't config.set_last_state_for_batch for batch #{}",
                self.batch_id.unwrap() //Safe
            ))?;

        Ok(())
    }
}
