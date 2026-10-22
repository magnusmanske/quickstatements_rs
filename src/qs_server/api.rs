//! `api.php`: the actions of the PHP API, with the same parameters and
//! responses. Parameters come from the query string and the (urlencoded or
//! multipart) body, like PHP's `$_REQUEST`.

use super::batches::{self, Batch};
use super::session::{self, User};
use super::{import, single_command, AppState, MAX_REQUEST_BYTES};
use crate::error::QsError;
use crate::qs_command::LastEntityState;
use crate::qs_config::QuickStatements;
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum_extra::extract::cookie::PrivateCookieJar;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use wikimisc::mediawiki::api::Api;

/// Most batches `get_batches_info` returns at once
const MAX_BATCHES: u64 = 1000;
/// Most commands `get_commands_from_batch` returns at once
const MAX_COMMANDS: u64 = 10_000;

/// A failed action. Internal errors are logged, not shown.
enum ApiError {
    User(String),
    Internal(QsError),
}

impl From<QsError> for ApiError {
    fn from(e: QsError) -> Self {
        Self::Internal(e)
    }
}

impl From<String> for ApiError {
    fn from(message: String) -> Self {
        Self::User(message)
    }
}

impl From<&str> for ApiError {
    fn from(message: &str) -> Self {
        Self::User(message.to_string())
    }
}

impl ApiError {
    fn message(self) -> String {
        match self {
            Self::User(message) => message,
            Self::Internal(e) => {
                log::error!("API: {e}");
                "Could not complete the request.".to_string()
            }
        }
    }
}

/// Fields of the response besides `status`
type ApiResult = Result<Value, ApiError>;

pub async fn handle(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    match read_params(request).await {
        Ok(params) => {
            Call {
                state,
                jar,
                headers,
                params,
            }
            .run()
            .await
        }
        Err(e) => json_response(Err(e)),
    }
}

async fn read_params(request: Request) -> Result<HashMap<String, String>, ApiError> {
    let mut params: HashMap<String, String> = request
        .uri()
        .query()
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if content_type.starts_with("multipart/form-data") {
        let mut multipart = Multipart::from_request(request, &())
            .await
            .map_err(|e| e.body_text())?;
        while let Some(field) = multipart.next_field().await.map_err(|e| e.body_text())? {
            if let Some(name) = field.name().map(str::to_string) {
                params.insert(name, field.text().await.map_err(|e| e.body_text())?);
            }
        }
    } else if content_type.starts_with("application/x-www-form-urlencoded") {
        let body = axum::body::to_bytes(request.into_body(), MAX_REQUEST_BYTES)
            .await
            .map_err(|_| "The request is too large")?;
        params.extend(form_urlencoded::parse(&body).into_owned());
    }
    Ok(params)
}

fn json_response(result: ApiResult) -> Response {
    let body = match result {
        Ok(mut fields) => {
            fields["status"] = json!("OK");
            fields
        }
        Err(e) => json!({ "status": e.message() }),
    };
    (
        [(header::CACHE_CONTROL, "no-cache, must-revalidate")],
        Json(body),
    )
        .into_response()
}

/// One API request
struct Call {
    state: AppState,
    jar: PrivateCookieJar,
    headers: HeaderMap,
    params: HashMap<String, String>,
}

impl Call {
    async fn run(self) -> Response {
        if self.params.contains_key("oauth_verifier") {
            return self.finish_login().await;
        }
        let result = match self.param("action") {
            "import" => {
                return self
                    .import()
                    .await
                    .unwrap_or_else(|e| json_response(Err(e)))
            }
            "oauth_redirect" => return self.start_login().await,
            "logout" => return self.logout(),
            "is_logged_in" => return self.is_logged_in().await,
            "get_token" => self.get_token().await,
            "get_batch_info" => self.get_batch_info().await,
            "get_batches_info" => self.get_batches_info().await,
            "get_commands_from_batch" => self.get_commands_from_batch().await,
            "get_batch" => self.get_batch().await,
            "run_single_command" => self.run_single_command().await,
            "start_batch" => self.change_batch_status("INIT").await,
            "stop_batch" => self.change_batch_status("STOP").await,
            "run_batch" => self.run_batch().await,
            "reset_errors" => self.reset_errors().await,
            action => Err(format!("Unknown action '{action}'").into()),
        };
        json_response(result)
    }

    fn qs(&self) -> &QuickStatements {
        &self.state.qs
    }

    fn param(&self, name: &str) -> &str {
        self.params.get(name).map_or("", String::as_str)
    }

    fn int(&self, name: &str) -> Option<i64> {
        self.param(name).trim().parse().ok()
    }

    /// Like PHP: set, and neither empty nor "0"
    fn flag(&self, name: &str, default: bool) -> bool {
        self.params
            .get(name)
            .map_or(default, |v| !matches!(v.trim(), "" | "0"))
    }

    /// The `site` parameter, or the default site; it must be configured
    fn site(&self) -> Result<String, ApiError> {
        let site = match self.param("site").trim().to_lowercase() {
            site if site.is_empty() => self.state.default_site.clone(),
            site => site,
        };
        match self.qs().get_api_for_site(&site) {
            Some(_) => Ok(site),
            None => Err(format!("Site '{site}' not in config file").into()),
        }
    }

    async fn logged_in_user(&self) -> Result<Option<User>, ApiError> {
        Ok(session::current_user(&self.state, &self.jar).await?)
    }

    async fn user(&self) -> Result<User, ApiError> {
        self.logged_in_user()
            .await?
            .ok_or_else(|| "Not logged in".into())
    }

    /// Changes on behalf of the logged-in user must come from this tool's own
    /// pages (protection against cross-site request forgery)
    fn require_same_origin(&self) -> Result<(), ApiError> {
        let header = |name| self.headers.get(name).and_then(|v| v.to_str().ok());
        let allowed = match header("sec-fetch-site") {
            Some(site) => site == "same-origin" || site == "none",
            None => {
                header("origin").is_none_or(|origin| self.state.origins.iter().any(|o| o == origin))
            }
        };
        if allowed {
            Ok(())
        } else {
            Err("Invalid origin".into())
        }
    }

    /// The batch, if the user may change it: as its owner or an admin
    async fn changeable_batch(&self, user: &User, batch_id: i64) -> Result<Batch, ApiError> {
        let batch = batches::batch(self.qs(), batch_id)
            .await?
            .ok_or_else(|| format!("No batch #{batch_id}"))?;
        if batch.user != user.id && !user.is_admin() {
            return Err("Not allowed to change this batch".into());
        }
        Ok(batch)
    }

    async fn set_batch_status(
        &self,
        user: &User,
        batch: &Batch,
        status: &str,
    ) -> Result<(), ApiError> {
        // Stopping a batch is always allowed
        if status != "STOP" {
            self.require_owner_not_blocked(batch).await?;
        }
        let message = format!("Status set by User:{}", user.name);
        self.qs()
            .set_batch_status(status, &message, batch.id, batch.user)
            .await
            .ok_or_else(|| "Could not change the batch status".into())
    }

    async fn require_owner_not_blocked(&self, batch: &Batch) -> Result<(), ApiError> {
        let qs = self.qs();
        let owner = qs
            .get_user_name(batch.user)
            .await
            .ok_or_else(|| format!("Cannot determine user name for batch #{}", batch.id))?;
        let api_url = qs
            .get_api_url(batch.id)
            .await
            .ok_or_else(|| format!("No API for batch #{}", batch.id))?;
        let mut api = Api::new(api_url).await.map_err(QsError::from)?;
        if QuickStatements::is_user_blocked(&mut api, &owner).await? {
            qs.set_batch_status("BLOCKED", "", batch.id, batch.user)
                .await;
            return Err(format!("User:{owner} is blocked").into());
        }
        Ok(())
    }

    async fn add_batch(
        &self,
        commands: &[Value],
        user_id: i64,
        name: &str,
        site: &str,
        credentials: &Value,
    ) -> ApiResult {
        if commands.is_empty() {
            return Err("No commands".into());
        }
        let batch_id =
            batches::create_batch(self.qs(), commands, user_id, name.trim(), site, credentials)
                .await?;
        Ok(json!({ "batch_id": batch_id }))
    }

    // ---- Login ----

    async fn start_login(self) -> Response {
        let Some(oauth) = self.state.oauth.clone() else {
            return json_response(Err("Login is not configured".into()));
        };
        match oauth.request_token().await {
            Ok((request, url)) => {
                let jar = session::start_login(self.jar, &request, self.state.secure_cookies);
                (jar, Redirect::to(&url)).into_response()
            }
            Err(e) => json_response(Err(e.into())),
        }
    }

    /// MediaWiki sends the user back here after they authorized the login
    async fn finish_login(self) -> Response {
        let tool_base = self.state.tool_base.clone();
        // No login in progress: a revisited or expired callback URL
        let (Some(oauth), Some(request)) =
            (self.state.oauth.clone(), session::request_token(&self.jar))
        else {
            return Redirect::to(&tool_base).into_response();
        };
        if self
            .params
            .get("oauth_token")
            .is_some_and(|t| *t != request.key)
        {
            return json_response(Err(
                "This login attempt does not match. Please log in again.".into(),
            ));
        }
        match oauth
            .access_token(&request, self.param("oauth_verifier"))
            .await
        {
            Ok(access) => {
                let jar = session::finish_login(self.jar, &access, self.state.secure_cookies);
                (jar, Redirect::to(&tool_base)).into_response()
            }
            Err(e) => json_response(Err(e.into())),
        }
    }

    fn logout(self) -> Response {
        if let Err(e) = self.require_same_origin() {
            return json_response(Err(e));
        }
        let response = json_response(Ok(json!({ "data": { "is_logged_in": false } })));
        (session::logout(self.jar), response).into_response()
    }

    /// Also as JSONP, for other tools
    async fn is_logged_in(&self) -> Response {
        let user = session::current_user(&self.state, &self.jar)
            .await
            .unwrap_or_else(|e| {
                log::error!("Cannot check the login: {e}");
                None
            });
        let data = match user {
            Some(user) => json!({ "query": { "userinfo": user.userinfo }, "is_logged_in": true }),
            None => json!({ "is_logged_in": false }),
        };
        match self.params.get("callback").filter(|c| !c.is_empty()) {
            None => json_response(Ok(json!({ "data": data }))),
            Some(callback) if is_valid_callback(callback) => {
                let body = json!({ "status": "OK", "data": data });
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    format!("{callback}({body})"),
                )
                    .into_response()
            }
            Some(_) => json_response(Err("Invalid callback".into())),
        }
    }

    async fn get_token(&self) -> ApiResult {
        self.require_same_origin()?;
        let Some(user) = self.logged_in_user().await? else {
            return Ok(json!({ "data": { "is_logged_in": false } }));
        };
        batches::save_user(self.qs(), user.id, &user.name).await?;
        let token =
            batches::api_token(self.qs(), user.id, self.flag("force_generate", false)).await?;
        Ok(json!({ "data": { "token": token, "is_logged_in": true } }))
    }

    // ---- Reading batches ----

    async fn get_batch_info(&self) -> ApiResult {
        let batch_id = self
            .int("batch")
            .filter(|id| *id > 0)
            .ok_or("Missing batch number")?;
        Ok(json!({ "data": batches::batch_status(self.qs(), &[batch_id]).await? }))
    }

    async fn get_batches_info(&self) -> ApiResult {
        let limit = self.int("limit").unwrap_or(20).clamp(1, MAX_BATCHES as i64) as u64;
        let offset = self.int("offset").unwrap_or(0).max(0) as u64;
        let ids = batches::latest_batch_ids(self.qs(), self.param("user"), limit, offset).await?;
        Ok(json!({ "data": batches::batch_status(self.qs(), &ids).await? }))
    }

    async fn get_commands_from_batch(&self) -> ApiResult {
        let batch_id = self.int("batch").unwrap_or(0);
        let start = self.int("start").unwrap_or(0);
        let limit = self.int("limit").unwrap_or(0).clamp(0, MAX_COMMANDS as i64) as u64;
        let statuses: Vec<String> = self
            .param("filter")
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        let commands =
            batches::commands(self.qs(), batch_id, start, Some(limit), &statuses).await?;
        let commands: Vec<Value> = commands.iter().map(batches::command_json).collect();
        Ok(json!({ "data": commands }))
    }

    /// All commands of a batch, by number, with their row as `_meta`
    async fn get_batch(&self) -> ApiResult {
        let batch_id = self.int("id").unwrap_or(0);
        let commands: Map<String, Value> = batches::commands(self.qs(), batch_id, 0, None, &[])
            .await?
            .iter()
            .map(|command| {
                let mut meta = batches::command_json(command);
                let mut json = Some(meta["json"].take())
                    .filter(Value::is_object)
                    .unwrap_or(json!({}));
                if let Some(meta) = meta.as_object_mut() {
                    meta.remove("json");
                }
                json["_meta"] = meta;
                (command.num.to_string(), json)
            })
            .collect();
        Ok(json!({ "id": self.param("id"), "data": { "commands": commands } }))
    }

    // ---- Importing and running ----

    async fn import(&self) -> Result<Response, ApiError> {
        let entity_base = self.state.entity_base.as_str();
        let format = match self.param("format") {
            "" => "v1",
            format => format,
        };
        let commands = import::parse(
            self.param("data"),
            format,
            self.flag("compress", true),
            entity_base,
        )
        .await?;

        if self.flag("temporary", false) {
            let content = json!({ "status": "OK", "data": { "commands": commands } });
            let name = import::save_temporary(&self.state.tmp_dir, &content)?;
            if self.flag("openpage", false) {
                return Ok(Html(import::open_page(&name, self.param("site"))).into_response());
            }
            return Ok(json_response(Ok(json!({ "data": name }))));
        }

        // Submitted by a tool, with the user's API token
        if self.flag("submit", false) {
            let user_name = self.param("username");
            let user_id = batches::user_with_token(self.qs(), user_name, self.param("token"))
                .await?
                .ok_or("User name and token do not match")?;
            let credentials = batches::latest_credentials(self.qs(), user_id).await?.ok_or_else(|| {
                format!("Problem generating OAuth signature; user '{user_name}' needs to have submitted a batch manually at least once before")
            })?;
            let site = self.site()?;
            let mut result = self
                .add_batch(
                    &commands,
                    user_id,
                    self.param("batchname"),
                    &site,
                    &credentials,
                )
                .await?;
            result["site"] = json!(site);
            return Ok(json_response(Ok(result)));
        }

        Ok(json_response(Ok(
            json!({ "data": { "commands": commands } }),
        )))
    }

    async fn run_single_command(&self) -> ApiResult {
        self.require_same_origin()?;
        let user = self.user().await?;
        let oauth = self.state.oauth.as_ref().ok_or("Login is not configured")?;
        let site = self.site()?;
        let command: Value = serde_json::from_str(self.param("command"))
            .ok()
            .filter(Value::is_object)
            .ok_or("Bad command JSON")?;
        let last = |name| Some(self.param(name).to_string()).filter(|s| !s.is_empty());
        let last = LastEntityState {
            last: last("last_item"),
            last_form: last("last_form"),
            last_sense: last("last_sense"),
        };

        let api_url = self.qs().get_api_for_site(&site).unwrap_or_default();
        let mut api = Api::new(api_url).await.map_err(QsError::from)?;
        api.set_oauth(Some(oauth.api_params(&user.token)));
        api.set_maxlag(self.qs().maxlag_s());
        api.set_edit_delay(None);

        let (command, last) =
            single_command::run(self.state.qs.clone(), api, user.id, command, last).await;
        Ok(json!({
            "command": command,
            "last_item": last.last.unwrap_or_default(),
            "last_form": last.last_form.unwrap_or_default(),
            "last_sense": last.last_sense.unwrap_or_default(),
        }))
    }

    /// Starts a batch in the background
    async fn run_batch(&self) -> ApiResult {
        self.require_same_origin()?;
        let user = self.user().await?;
        let oauth = self.state.oauth.as_ref().ok_or("Login is not configured")?;
        let site = self.site()?;
        let commands: Vec<Value> = match self.param("commands") {
            "" => vec![],
            commands => serde_json::from_str(commands).map_err(|_| "Bad commands JSON")?,
        };
        batches::save_user(self.qs(), user.id, &user.name).await?;
        let credentials = oauth.batch_credentials(&user.token);
        self.add_batch(&commands, user.id, self.param("name"), &site, &credentials)
            .await
    }

    async fn change_batch_status(&self, status: &str) -> ApiResult {
        self.require_same_origin()?;
        let user = self.user().await?;
        let batch = self
            .changeable_batch(&user, self.int("batch").unwrap_or(0))
            .await?;
        self.set_batch_status(&user, &batch, status).await?;
        Ok(json!({}))
    }

    async fn reset_errors(&self) -> ApiResult {
        self.require_same_origin()?;
        let batch_id = self.int("batch_id").unwrap_or(0);
        if batch_id <= 0 {
            return Err(format!("Bad batch ID #{batch_id}").into());
        }
        let user = self.user().await?;
        let batch = self.changeable_batch(&user, batch_id).await?;
        if let Some(archive) = self.qs().archive() {
            archive.restore_batch(self.qs(), batch_id).await?;
        }
        let count = batches::reset_errors(self.qs(), batch_id).await?;
        if count > 0 {
            self.set_batch_status(&user, &batch, "INIT").await?;
        }
        Ok(json!({ "init": count }))
    }
}

/// A JavaScript identifier or property path, so it cannot inject code
fn is_valid_callback(callback: &str) -> bool {
    callback.len() <= 100
        && !callback.starts_with(|c: char| c.is_ascii_digit())
        && callback
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks() {
        assert!(is_valid_callback("jQuery123_456"));
        assert!(is_valid_callback("app.callbacks.$done"));
        for callback in ["alert(1)", "a;b", "1abc", "a b", "</script>"] {
            assert!(!is_valid_callback(callback), "{callback}");
        }
    }

    #[test]
    fn json_responses() {
        let ok = json_response(Ok(json!({ "batch_id": 1 })));
        assert_eq!(
            ok.headers()[header::CACHE_CONTROL],
            "no-cache, must-revalidate"
        );
        let user = ApiError::from("Not logged in").message();
        assert_eq!(user, "Not logged in");
        // Internal details stay in the log
        let internal = ApiError::from(QsError::DatabaseError("secret".into())).message();
        assert_eq!(internal, "Could not complete the request.");
    }
}
