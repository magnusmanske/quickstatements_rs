//! OAuth 1.0a for MediaWiki's Special:OAuth, as PHP's `MW_OAuth`.
//!
//! The protocol lives in [`wikimisc::oauth1`]; this module only maps the
//! tool's config (env vars or an ini file) and the API shapes QuickStatements
//! needs on top of it.

use crate::error::{QsError, QsResult};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use wikimisc::mediawiki::api::OAuthParams;
use wikimisc::mediawiki::reqwest::Client;
use wikimisc::oauth1::{Consumer, OAuth as SharedOAuth};

pub use wikimisc::oauth1::Token;

const DEFAULT_USER_AGENT: &str =
    "QuickStatements (https://github.com/magnusmanske/quickstatements_rs)";

#[derive(Debug)]
pub struct OAuth {
    client: SharedOAuth,
    consumer: Consumer,
    user_agent: String,
    /// `oob`, or where MediaWiki sends the user after authorizing
    callback: String,
}

impl OAuth {
    /// The OAuth consumer of a site in the frontend config, or `None` if it
    /// has no `oauth` block. Like PHP, the consumer comes from
    /// `MW_CONSUMER_TOKEN`/`MW_CONSUMER_SECRET`, or else the `ini_file`.
    pub fn from_site(site: &Value, tool_base: &str) -> QsResult<Option<Self>> {
        let config = &site["oauth"];
        let Some(url) = config["mwOAuthUrl"].as_str() else {
            return Ok(None);
        };
        let env = |key| std::env::var(key).ok().filter(|v| !v.is_empty());
        let ini = match (env("MW_CONSUMER_TOKEN"), config["ini_file"].as_str()) {
            (None, Some(file)) => parse_ini(&std::fs::read_to_string(file).map_err(|e| {
                QsError::ConfigError(format!("Cannot read OAuth ini file '{file}': {e}"))
            })?),
            _ => HashMap::new(),
        };
        let (key, secret) = match (env("MW_CONSUMER_TOKEN"), env("MW_CONSUMER_SECRET")) {
            (Some(key), Some(secret)) => (key, secret),
            _ => match (ini.get("consumerKey"), ini.get("consumerSecret")) {
                (Some(key), Some(secret)) => (key.to_owned(), secret.to_owned()),
                _ => {
                    return Err(QsError::ConfigError(
                        "No OAuth consumer: set MW_CONSUMER_TOKEN and MW_CONSUMER_SECRET, or oauth.ini_file".into(),
                    ))
                }
            },
        };
        let user_agent = ini.get("agent").map_or(DEFAULT_USER_AGENT, String::as_str);
        let consumer = Consumer::new(key, secret, user_agent);
        let public_url = config["publicMwOAuthUrl"]
            .as_str()
            .unwrap_or(url)
            .to_string();
        let http = Client::builder()
            .user_agent(user_agent)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(25))
            .build()
            .map_err(|e| QsError::ConfigError(e.to_string()))?;
        let client = SharedOAuth::new(consumer.clone(), http).with_urls(url, public_url);
        // A consumer registered with a fixed callback URL must send "oob"
        let oob = env("MW_CONSUMER_CALLBACK_OOB").is_some_and(|v| v == "1" || v == "true");
        let callback = match tool_base {
            "./" => "oob".to_string(),
            _ if oob => "oob".to_string(),
            base => format!("{base}api.php"),
        };
        Ok(Some(Self {
            client,
            consumer,
            user_agent: user_agent.to_string(),
            callback,
        }))
    }

    /// Fetches a request token, and returns it with the URL where the user
    /// authorizes it.
    pub async fn request_token(&self) -> QsResult<(Token, String)> {
        self.client
            .initiate(&self.callback)
            .await
            .map_err(oauth_error)
    }

    /// Exchanges an authorized request token for an access token.
    pub async fn access_token(&self, request: &Token, verifier: &str) -> QsResult<Token> {
        self.client
            .access_token(request, verifier)
            .await
            .map_err(oauth_error)
    }

    /// The user's `userinfo` from `api_url`, or `None` if the token is not
    /// (or no longer) valid.
    pub async fn userinfo(&self, api_url: &str, token: &Token) -> QsResult<Option<Value>> {
        let params = [
            ("action", "query"),
            ("meta", "userinfo"),
            ("uiprop", "blockinfo|groups|rights"),
        ];
        let response = self
            .client
            .api(api_url, Some(token), &params)
            .await
            .map_err(oauth_error)?;
        let userinfo = &response["query"]["userinfo"];
        Ok(userinfo["id"]
            .as_i64()
            .filter(|id| *id > 0)
            .map(|_| userinfo.clone()))
    }

    /// The credentials the bot edits with, as stored in
    /// `batch_oauth.serialized_json`.
    pub fn batch_credentials(&self, token: &Token) -> Value {
        json!({
            "gConsumerKey": self.consumer.key(),
            "gConsumerSecret": self.consumer.secret(),
            "gTokenKey": token.key,
            "gTokenSecret": token.secret,
            "gUserAgent": self.user_agent,
        })
    }

    pub fn api_params(&self, token: &Token) -> OAuthParams {
        OAuthParams::new_from_json(&self.batch_credentials(token))
    }
}

fn oauth_error(e: impl std::fmt::Display) -> QsError {
    QsError::StringError(format!("OAuth request failed: {e}"))
}

/// The `key = value` lines of an ini file without sections, like PHP's `parse_ini_file`.
fn parse_ini(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with([';', '#', '[']))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ini_reads_keys() {
        let ini = parse_ini("; comment\n[section]\nagent = QS test\nconsumerKey = \"abc\"\n");
        assert_eq!(ini["agent"], "QS test");
        assert_eq!(ini["consumerKey"], "abc");
        assert_eq!(ini.len(), 2);
    }

    #[test]
    fn from_site_without_oauth_is_none() {
        assert!(
            OAuth::from_site(&json!({}), "https://qs.example/")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn authorize_url_and_credentials() {
        let site = json!({"oauth":{"mwOAuthUrl":"https://wiki.example/w/index.php?title=Special:OAuth","ini_file":"/nonexistent"}});
        // Without the environment variables, the missing ini file is an error
        if std::env::var("MW_CONSUMER_TOKEN").is_err() {
            assert!(OAuth::from_site(&site, "https://qs.example/").is_err());
        }
        let consumer = Consumer::new("ck", "cs", "ua");
        let client = SharedOAuth::new(consumer.clone(), Client::new());
        let oauth = OAuth {
            client,
            consumer,
            user_agent: "ua".into(),
            callback: "oob".into(),
        };
        let token = Token {
            key: "tk".into(),
            secret: "ts".into(),
        };
        let credentials = oauth.batch_credentials(&token);
        assert_eq!(credentials["gConsumerKey"], "ck");
        assert_eq!(credentials["gTokenSecret"], "ts");
        let params = oauth.api_params(&token);
        assert_eq!(params.g_token_key.as_deref(), Some("tk"));
    }
}
