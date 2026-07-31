use crate::error::{QsError, QsResult};
use crate::qs_command::{LastEntityState, QuickStatementsCommand};
use crate::qs_config::QuickStatements;
use crate::qs_parser::COMMONS_API;
use log;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;
use wikibase;

/// A failing command never stops its batch — it is marked ERROR and the batch
/// moves on. Runs of consecutive failures (which suggest a systemic problem,
/// e.g. revoked OAuth) are logged every this many commands.
const CONSECUTIVE_COMMAND_ERROR_WARN_EVERY: u32 = 5;

/// Adaptive edit pacing: run at full speed while the API is happy, back off on
/// pushback (rate limit / throttle / lag), and decay back to full speed on
/// success. Backoff doubles from the minimum up to the maximum; each successful
/// edit halves the delay again, down to the configured `edit_delay_ms` floor.
/// Per-user rate limits are enforced server-side and deliberately not raised
/// here: the user shares their edit budget with their own manual edits.
const THROTTLE_BACKOFF_MIN_MS: u64 = 5_000;
const THROTTLE_BACKOFF_MAX_MS: u64 = 60_000;

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
    /// Current adaptive delay between edits; see THROTTLE_BACKOFF_* above.
    adaptive_delay_ms: u64,
    /// Floor for the adaptive delay, from the `edit_delay_ms` config key.
    min_delay_ms: u64,
    entity_revision: VecDeque<(String, usize)>,
    consecutive_command_errors: u32,
}

impl QuickStatementsBot {
    pub fn new(config: Arc<QuickStatements>, batch_id: Option<i64>, user_id: i64) -> Self {
        let min_delay_ms = config.edit_delay_ms().unwrap_or(0);
        Self {
            batch_id,
            user_id,
            config,
            mw_api: None,
            entities: wikibase::entity_container::EntityContainer::new(),
            last_state: LastEntityState::default(),
            current_entity_id: None,
            current_property_id: None,
            adaptive_delay_ms: min_delay_ms,
            min_delay_ms,
            entity_revision: VecDeque::new(),
            consecutive_command_errors: 0,
        }
    }

    /// API pushback: double the adaptive delay (starting at the minimum
    /// backoff) and return it as the pre-retry sleep in milliseconds.
    fn bump_backoff(&mut self) -> u64 {
        self.adaptive_delay_ms =
            (self.adaptive_delay_ms * 2).clamp(THROTTLE_BACKOFF_MIN_MS, THROTTLE_BACKOFF_MAX_MS);
        self.adaptive_delay_ms
    }

    /// Successful edit: decay the adaptive delay back towards the floor.
    fn decay_delay(&mut self) {
        self.adaptive_delay_ms = (self.adaptive_delay_ms / 2).max(self.min_delay_ms);
    }

    pub async fn start(&mut self) -> Result<(), String> {
        match self.batch_id {
            Some(batch_id) => {
                let config = self.config.clone();
                config
                    .restart_batch(batch_id)
                    .await
                    .ok_or("Can't (re)start batch".to_string())?;
                // restart_batch only touches INIT/RUN batches; if the user stopped
                // the batch in the meantime, its status is unchanged and we bail out.
                config
                    .check_batch_not_stopped(batch_id)
                    .await
                    .map_err(|e| e.to_string())?;
                self.last_state = config.get_last_state_from_batch(batch_id).await;
                match config.get_api_url(batch_id).await {
                    Some(url) => {
                        let mut mw_api = wikibase::mediawiki::api::Api::new(url)
                            .await
                            .map_err(|e| format!("{:?}", e))?;
                        // Edit pacing is done adaptively by this bot (see
                        // THROTTLE_BACKOFF_*), not with a fixed delay in the API layer.
                        mw_api.set_edit_delay(None);
                        mw_api.set_maxlag(config.maxlag_s());
                        mw_api.set_max_retry_attempts(1000);
                        config.set_bot_api_auth(&mut mw_api, batch_id).await?;
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

    pub fn batch_id(&self) -> Option<i64> {
        self.batch_id
    }

    /// Gives up on this batch after repeated transient failures: frees its slot
    /// and puts it back in the queue, so it will be picked up again later.
    pub async fn release_batch(&self, message: &str) {
        if let Some(batch_id) = self.batch_id {
            let _ = self
                .config
                .set_batch_status("INIT", message, batch_id, self.user_id)
                .await;
        }
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
        for (k, v) in action.as_object().ok_or("Action is not a JSON object")? {
            params.insert(
                k.to_string(),
                v.as_str()
                    .ok_or(format!(
                        "Cannot convert param '{}' value to string: {}",
                        k, v
                    ))?
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
            match self.batch_id {
                Some(id) => log::info!("Batch #{}: {}", id, msg),
                None => log::info!("No batch ID: {}", msg),
            }
        }
    }

    /// Returns `Ok(true)` when a command was executed, `Ok(false)` when the batch is done,
    /// or `Err` for transient failures (caller should retry).
    pub async fn run(&mut self) -> Result<bool, String> {
        self.log("[run] Getting next command".to_string());
        let command = match self.get_next_command().await {
            Ok(c) => c,
            Err(e) => {
                let is_transient = matches!(e, QsError::MysqlAsyncError(_));
                if is_transient {
                    return Err(format!("Transient DB error in get_next_command: {}", e));
                }
                // Permanent: batch was stopped, or no batch_id set
                if let Some(batch_id) = self.batch_id {
                    let _ = self
                        .config
                        .deactivate_batch_run(batch_id, self.user_id)
                        .await;
                }
                return Ok(false);
            }
        };

        match command {
            Some(mut command) => {
                self.log("[run] Executing command".to_string());
                // Mark the command RUN here: if this write fails, the command stays
                // INIT and would be picked up again immediately, so surface it as a
                // transient error to get the caller's backoff instead of hot-looping.
                self.set_command_status("RUN", None, &mut command).await?;
                match self.execute_command(&mut command).await {
                    Ok(_) => self.consecutive_command_errors = 0,
                    Err(e) => {
                        log::error!(
                            "Batch #{} command #{}: {}",
                            self.batch_id.unwrap_or(0),
                            command.id,
                            e
                        );
                        // The command itself is marked ERROR by execute_command;
                        // the batch carries on with the next one. Long runs of
                        // failures are only logged, so a systemic problem stays
                        // visible without killing the remaining commands.
                        self.consecutive_command_errors += 1;
                        if self
                            .consecutive_command_errors
                            .is_multiple_of(CONSECUTIVE_COMMAND_ERROR_WARN_EVERY)
                        {
                            log::warn!(
                                "Batch #{}: {} consecutive command errors, still running (systemic problem? e.g. revoked OAuth)",
                                self.batch_id.unwrap_or(0),
                                self.consecutive_command_errors
                            );
                        }
                    }
                }
                self.log("[run] Command executed".to_string());
                Ok(true)
            }
            None => {
                self.log("[run] No more commands".to_string());
                if let Some(batch_id) = self.batch_id {
                    let _ = self.config.set_batch_finished(batch_id, self.user_id).await;
                }
                Ok(false)
            }
        }
    }

    async fn get_next_command(&self) -> QsResult<Option<QuickStatementsCommand>> {
        match self.batch_id {
            Some(batch_id) => {
                self.config.check_batch_not_stopped(batch_id).await?;
                self.config.get_next_command(batch_id).await
            }
            None => Err(QsError::NoMatchSetError),
        }
    }

    /// Returns true if this command type operates on lexeme sub-entities
    /// and does not require loading the main entity from the API.
    fn is_lexeme_subentity_command(command: &QuickStatementsCommand) -> bool {
        matches!(
            command.json["what"].as_str(),
            Some("lemma")
                | Some("lexical_category")
                | Some("language")
                | Some("representation")
                | Some("grammatical_feature")
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
        static RE_MEDIA_INFO: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"^M\d+$"#)
                .expect("QuickStatementsBot::try_create_fake_entity:RE_MEDIA_INFO does not compile")
        });

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

        let user_name = self
            .config
            .get_user_name(self.user_id)
            .await
            .ok_or("User not found".to_string())?;

        // Reuse the bot's configured API (already has auth, maxlag, retry settings)
        let mut mw_api = self
            .mw_api
            .clone()
            .ok_or("No mw_api available for block check".to_string())?;

        QuickStatements::is_user_blocked(&mut mw_api, &user_name)
            .await
            .map_err(|e| e.to_string())
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
        self.current_property_id = None;
        self.current_entity_id = None;

        self.log("[execute_command] Prep".to_string());
        // Preparation failures (unresolvable LAST, entity fails to load, ...) must
        // mark the command ERROR too, or it stays RUN in the DB forever.
        if let Err(e) = command.insert_last_item_into_sources_and_qualifiers(&self.last_state) {
            self.set_command_status("ERROR", Some(&e), command).await?;
            return Err(e);
        }
        let main_item = match self.prepare_to_execute(command).await {
            Ok(main_item) => main_item,
            Err(e) => {
                self.set_command_status("ERROR", Some(&e), command).await?;
                return Err(e);
            }
        };
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

        // Update LAST from command's item field, but only if it's a concrete entity ID.
        let has_concrete_item = if let Some(q) = command.json["item"].as_str() {
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
                true
            } else {
                false
            }
        } else {
            false
        };

        // Cache the full entity from the API response (e.g. wbeditentity).
        // Even when a concrete item was already processed above, the response
        // may carry a richer entity JSON that we still want to cache.
        match &res["entity"] {
            serde_json::Value::Null => {}
            entity_json => {
                if let Some(q) = wikibase::entity_diff::EntityDiff::get_entity_id(entity_json) {
                    // Don't overwrite LAST from entity response if we already set it
                    // from the command, unless the entity ID actually changed.
                    if !has_concrete_item {
                        self.last_state.last = Some(q.to_owned());
                    }
                    // CREATE / CREATE_LEXEME: clear LAST_FORM and LAST_SENSE
                    if command_action == "create" && !matches!(command_type, "form" | "sense") {
                        self.last_state.last_form = None;
                        self.last_state.last_sense = None;
                    }
                    if let Err(e) = self.entities.set_entity_from_json(entity_json) {
                        log::error!("Failed to set entity from JSON for {}: {}", q, e);
                    }
                    // The full entity is now cached; drop any revision pin so the next
                    // load uses the cache instead of an anonymous fetch from a possibly
                    // lagging replica (which may not even know a new entity yet).
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

        let mut mw_api = self.mw_api.to_owned().ok_or(format!(
            "QuickStatementsBot::run_action batch #{} has no mw_api",
            self.batch_id.unwrap_or(0)
        ))?;

        const MAX_JSON_RETRIES: usize = 3;
        const MAX_THROTTLE_RETRIES: usize = 10;
        let mut json_retries = 0usize;
        let mut throttle_retries = 0usize;
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
                // Usually an HTML rate-limit page, i.e. the edit was rejected. If the
                // edit was applied and only the response was lost, this retry may
                // duplicate it; MediaWiki offers no idempotency token to prevent that.
                Err(wikibase::mediawiki::MediaWikiError::Serde(_))
                    if json_retries < MAX_JSON_RETRIES =>
                {
                    json_retries += 1;
                    self.log(format!(
                        "[run_action] Non-JSON API response, retrying ({}/{})",
                        json_retries, MAX_JSON_RETRIES
                    ));
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                Err(e) => return Err(format!("Wiki editing failed: {:?}", e)),
            };
            self.log("[run_action] Post post_query_api_json_mut".to_string());

            let retry_after = self.check_run_action_result(res, &params, command)?;
            match retry_after {
                None => {
                    // Pace edits with the current adaptive delay, then relax it
                    if self.adaptive_delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(self.adaptive_delay_ms)).await;
                    }
                    self.decay_delay();
                    return Ok(());
                }
                Some(d) => {
                    throttle_retries += 1;
                    if throttle_retries > MAX_THROTTLE_RETRIES {
                        return Err(format!(
                            "Too many throttle retries ({}) for command #{}",
                            throttle_retries, command.id
                        ));
                    }
                    tokio::time::sleep(d).await;
                }
            }
        }
    }

    /// Checks the command result.
    /// Returns Ok(None) when done, Ok(Some(duration)) to retry after sleeping, Err on fatal error.
    fn check_run_action_result(
        &mut self,
        res: Value,
        params: &HashMap<String, String>,
        command: &mut QuickStatementsCommand,
    ) -> Result<Option<Duration>, String> {
        static RE_QUAL_OK: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new("^The statement has already a qualifier with hash")
                .expect("QuickStatementsBot::run_action:RE_QUAL_OK does not compile")
        });
        static RE_REF_OK: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new("^The statement has already a reference with hash")
                .expect("QuickStatementsBot::run_action:RE_REF_OK does not compile")
        });

        match res["success"].as_i64() {
            Some(num) => {
                if num == 1 {
                    self.reset_entities(&res, command);
                    Ok(None)
                } else {
                    Err(format!("Success flag is '{}' in API result", num))
                }
            }
            None => {
                // Check for rate limiting / throttle — handle both old and new Wikimedia error formats
                let error_code = res["error"]["code"].as_str().unwrap_or("");
                if error_code == "maxlag" {
                    let lag = res["error"]["lag"].as_f64().unwrap_or(5.0);
                    let lag_ms = (lag.ceil() as u64 + 1) * 1000;
                    let sleep_ms = self.bump_backoff().max(lag_ms);
                    log::warn!(
                        "Batch #{}: Maxlag exceeded (lag: {}s), sleeping {}ms",
                        self.batch_id.unwrap_or(0),
                        lag,
                        sleep_ms
                    );
                    return Ok(Some(Duration::from_millis(sleep_ms)));
                }
                if matches!(error_code, "ratelimited" | "actionthrottled") {
                    let sleep_ms = self.bump_backoff();
                    log::warn!(
                        "Batch #{}: Rate limited by API (code: {}), sleeping {}ms",
                        self.batch_id.unwrap_or(0),
                        error_code,
                        sleep_ms
                    );
                    return Ok(Some(Duration::from_millis(sleep_ms)));
                }
                if let Some(arr) = res["error"]["messages"].as_array() {
                    let throttled = arr.iter().any(|a| {
                        matches!(
                            a["name"].as_str(),
                            Some("actionthrottledtext" | "ratelimited")
                        )
                    });
                    if throttled {
                        let sleep_ms = self.bump_backoff();
                        log::warn!(
                            "Batch #{}: Throttled by API, sleeping {}ms",
                            self.batch_id.unwrap_or(0),
                            sleep_ms
                        );
                        return Ok(Some(Duration::from_millis(sleep_ms)));
                    }
                }
                if let Some(s) = res["error"]["info"].as_str() {
                    command.json["meta"]["message"] = json!(s);
                    // That qualifier/reference already exists: logically a success,
                    // so keep LAST and the entity cache in sync like a normal success
                    if RE_QUAL_OK.is_match(s) || RE_REF_OK.is_match(s) {
                        self.reset_entities(&res, command);
                        return Ok(None);
                    }
                }
                log::error!("COMMAND ERROR #{}:\n{:?}\n{}", command.id, params, res);
                Err("No success flag set in API result".to_string())
            }
        }
    }

    // LAST / LAST_FORM / LAST_SENSE are maintained by reset_entities() from the
    // API response; setting them here as well would overwrite e.g. a freshly
    // created entity ID with None.
    async fn set_command_status(
        &self,
        status: &str,
        message: Option<&str>,
        command: &mut QuickStatementsCommand,
    ) -> Result<(), String> {
        if self.batch_id.is_none() {
            return Ok(());
        }

        // Write LAST state first: if this DB write fails we bail out before
        // updating the command status, keeping the two in sync. In the worst
        // case the LAST state is stale but the command is still RUN/INIT and
        // will be retried.
        self.config
            .set_last_state_for_batch(self.batch_id.unwrap(), &self.last_state) // unwrap safe
            .await
            .ok_or(format!(
                "Can't config.set_last_state_for_batch for batch #{}",
                self.batch_id.unwrap()
            ))?;
        self.config
            .set_command_status(command, status, message.map(|s| s.to_string()))
            .await
            .ok_or(format!(
                "Can't config.set_command_status for batch #{}",
                self.batch_id.unwrap() // Safe
            ))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bot() -> QuickStatementsBot {
        let config = Arc::new(QuickStatements::new_for_tests());
        QuickStatementsBot::new(config, Some(1), 0)
    }

    #[test]
    fn check_run_action_result_success_updates_last_state() {
        let mut bot = test_bot();
        let mut command = QuickStatementsCommand::new_from_json(
            &json!({"action":"add","what":"statement","item":"Q5"}),
        );
        let res = json!({"success":1,"pageinfo":{"lastrevid":123}});

        let result = bot.check_run_action_result(res, &HashMap::new(), &mut command);

        assert_eq!(result, Ok(None));
        assert_eq!(bot.last_state.last, Some("Q5".to_string()));
        assert_eq!(bot.entity_revision.front(), Some(&("Q5".to_string(), 123)));
    }

    // "Already exists" API errors are logically a success and must update LAST
    // like one, or a following LAST command works on the wrong entity
    #[test]
    fn check_run_action_result_existing_qualifier_updates_last_state() {
        let mut bot = test_bot();
        let mut command = QuickStatementsCommand::new_from_json(
            &json!({"action":"add","what":"qualifier","item":"Q123"}),
        );
        let res = json!({"error":
            {"info":"The statement has already a qualifier with hash deadbeef"}});

        let result = bot.check_run_action_result(res, &HashMap::new(), &mut command);

        assert_eq!(result, Ok(None));
        assert_eq!(bot.last_state.last, Some("Q123".to_string()));
    }

    #[test]
    fn check_run_action_result_existing_reference_updates_last_state() {
        let mut bot = test_bot();
        let mut command = QuickStatementsCommand::new_from_json(
            &json!({"action":"add","what":"sources","item":"Q123"}),
        );
        let res = json!({"error":
            {"info":"The statement has already a reference with hash deadbeef"}});

        let result = bot.check_run_action_result(res, &HashMap::new(), &mut command);

        assert_eq!(result, Ok(None));
        assert_eq!(bot.last_state.last, Some("Q123".to_string()));
    }

    #[test]
    fn check_run_action_result_other_error_is_fatal() {
        let mut bot = test_bot();
        let mut command = QuickStatementsCommand::new_from_json(&json!({"item":"Q123"}));
        let res = json!({"error":{"code":"failed-save","info":"Some other error"}});

        let result = bot.check_run_action_result(res, &HashMap::new(), &mut command);

        assert!(result.is_err());
        assert_eq!(bot.last_state.last, None);
    }

    #[test]
    fn check_run_action_result_maxlag_is_retried() {
        let mut bot = test_bot();
        let mut command = QuickStatementsCommand::new_from_json(&json!({"item":"Q123"}));
        let res = json!({"error":{"code":"maxlag","lag":3.2}});

        let result = bot.check_run_action_result(res, &HashMap::new(), &mut command);

        assert_eq!(result, Ok(Some(Duration::from_millis(5000))));
    }

    // Repeated pushback must double the backoff (capped), and successful edits
    // must decay it back to the configured floor
    #[test]
    fn adaptive_delay_backoff_doubles_and_decays() {
        let mut bot = test_bot();
        bot.min_delay_ms = 0;
        bot.adaptive_delay_ms = 0;

        assert_eq!(bot.bump_backoff(), THROTTLE_BACKOFF_MIN_MS);
        assert_eq!(bot.bump_backoff(), 2 * THROTTLE_BACKOFF_MIN_MS);
        for _ in 0..10 {
            bot.bump_backoff();
        }
        assert_eq!(bot.adaptive_delay_ms, THROTTLE_BACKOFF_MAX_MS);

        for _ in 0..20 {
            bot.decay_delay();
        }
        assert_eq!(bot.adaptive_delay_ms, 0);
    }

    #[test]
    fn adaptive_delay_respects_configured_floor() {
        let mut bot = test_bot();
        bot.min_delay_ms = 250;
        bot.adaptive_delay_ms = THROTTLE_BACKOFF_MIN_MS;

        for _ in 0..20 {
            bot.decay_delay();
        }
        assert_eq!(bot.adaptive_delay_ms, 250);
    }

    #[test]
    fn check_run_action_result_ratelimited_backs_off_exponentially() {
        let mut bot = test_bot();
        bot.min_delay_ms = 0;
        bot.adaptive_delay_ms = 0;
        let mut command = QuickStatementsCommand::new_from_json(&json!({"item":"Q123"}));
        let res = json!({"error":{"code":"ratelimited"}});

        let first = bot.check_run_action_result(res.clone(), &HashMap::new(), &mut command);
        let second = bot.check_run_action_result(res, &HashMap::new(), &mut command);

        assert_eq!(
            first,
            Ok(Some(Duration::from_millis(THROTTLE_BACKOFF_MIN_MS)))
        );
        assert_eq!(
            second,
            Ok(Some(Duration::from_millis(2 * THROTTLE_BACKOFF_MIN_MS)))
        );
    }

    #[test]
    fn check_run_action_result_throttled_message_backs_off() {
        let mut bot = test_bot();
        bot.min_delay_ms = 0;
        bot.adaptive_delay_ms = 0;
        let mut command = QuickStatementsCommand::new_from_json(&json!({"item":"Q123"}));
        let res = json!({"error":{"messages":[{"name":"actionthrottledtext"}]}});

        let result = bot.check_run_action_result(res, &HashMap::new(), &mut command);

        assert_eq!(
            result,
            Ok(Some(Duration::from_millis(THROTTLE_BACKOFF_MIN_MS)))
        );
    }
}
