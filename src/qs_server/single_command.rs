//! Runs one command of a batch that runs in the browser, with the user's
//! OAuth token. LAST/LAST_FORM/LAST_SENSE travel with each request.

use crate::qs_bot::QuickStatementsBot;
use crate::qs_command::{LastEntityState, QuickStatementsCommand};
use crate::qs_config::QuickStatements;
use serde_json::{json, Value};
use std::sync::Arc;
use wikimisc::mediawiki::api::Api;

/// Runs the command, and returns it with `status` (`done`/`error`), a
/// `message` and the resolved `item`, as PHP's `runSingleCommand` does.
/// `json` must be an object.
pub async fn run(
    qs: Arc<QuickStatements>,
    api: Api,
    user_id: i64,
    mut json: Value,
    last: LastEntityState,
) -> (Value, LastEntityState) {
    // The bot writes to `meta`, so it must be an object. The frontend sends
    // the command back with the message of an earlier attempt.
    if !json["meta"].is_object() {
        json["meta"] = json!({});
    }
    json["meta"]["message"] = json!("");
    let mut bot = QuickStatementsBot::new(qs, None, user_id);
    bot.set_mw_api(api);
    bot.set_last_state(last);
    let mut command = QuickStatementsCommand::new_from_json(&json);
    let result = bot.execute_command(&mut command).await;
    let mut last = bot.last_state().clone();
    finish(&mut command.json, &mut last, result);
    (command.json, last)
}

fn finish(json: &mut Value, last: &mut LastEntityState, result: Result<(), String>) {
    let created = json["action"] == "create";
    // The bot puts the wiki's own message (e.g. its error) here
    let wiki_message = json["meta"]["message"]
        .as_str()
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    match result {
        Ok(()) => {
            json["status"] = json!("done");
            if let Some(message) = wiki_message {
                json["message"] = json!(message);
            }
        }
        Err(e) => {
            json["status"] = json!("error");
            json["message"] = json!(wiki_message.unwrap_or(e));
            if created {
                // Commands with LAST must fail rather than edit an earlier entity
                *last = LastEntityState::default();
            }
        }
    }
    let item = match (created, json["type"].as_str()) {
        (true, Some("form")) => last.last_form.clone(),
        (true, Some("sense")) => last.last_sense.clone(),
        (true, _) => last.last.clone(),
        (false, _) => json["item"]
            .as_str()
            .and_then(|item| last.resolve(&item.to_uppercase()).cloned()),
    };
    if let Some(item) = item {
        json["item"] = json!(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(last: &str) -> LastEntityState {
        LastEntityState {
            last: Some(last.to_string()),
            last_form: Some(format!("{last}-F1")),
            last_sense: None,
        }
    }

    #[test]
    fn success_resolves_last() {
        let mut json = json!({"action":"add","item":"LAST","meta":{"message":"already has"}});
        let mut last = state("Q5");
        finish(&mut json, &mut last, Ok(()));
        assert_eq!(json["status"], "done");
        assert_eq!(json["item"], "Q5");
        assert_eq!(json["message"], "already has");
    }

    #[test]
    fn created_entity_becomes_the_item() {
        let mut json = json!({"action":"create","type":"form","item":"L1"});
        let mut last = state("L1");
        finish(&mut json, &mut last, Ok(()));
        assert_eq!(json["item"], "L1-F1");
    }

    #[test]
    fn errors_show_the_wikis_message() {
        let mut json = json!({"action":"add","item":"Q1","meta":{"message":"The wiki says no"}});
        finish(
            &mut json,
            &mut state("Q5"),
            Err("No success flag".to_string()),
        );
        assert_eq!(json["message"], "The wiki says no");
    }

    #[test]
    fn failed_create_clears_last() {
        let mut json = json!({"action":"create","type":"item"});
        let mut last = state("Q5");
        finish(&mut json, &mut last, Err("nope".to_string()));
        assert_eq!(json["status"], "error");
        assert_eq!(json["message"], "nope");
        assert!(last.last.is_none() && last.last_form.is_none());
        assert!(json["item"].is_null());
    }
}
