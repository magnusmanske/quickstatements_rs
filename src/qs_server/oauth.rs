//! OAuth 1.0a client for MediaWiki's Special:OAuth, as PHP's `MW_OAuth`.
//! Requests are signed with HMAC-SHA1; the protocol parameters go into the
//! Authorization header.

use super::random_string;
use crate::error::{QsError, QsResult};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::Sha1;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wikimisc::mediawiki::api::OAuthParams;
use wikimisc::mediawiki::reqwest::{self, header::AUTHORIZATION, Url};

const DEFAULT_USER_AGENT: &str =
    "QuickStatements (https://github.com/magnusmanske/quickstatements_rs)";

/// RFC 3986 encoding, as the OAuth signature requires
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode(s: &str) -> String {
    utf8_percent_encode(s, ENCODE_SET).to_string()
}

/// A consumer, request or access token
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub key: String,
    pub secret: String,
}

#[derive(Debug)]
pub struct OAuth {
    consumer: Token,
    user_agent: String,
    /// Special:OAuth, as this server reaches it
    url: String,
    /// Special:OAuth, as the user's browser reaches it
    public_url: String,
    /// `oob`, or where MediaWiki sends the user after authorizing
    callback: String,
    http: reqwest::Client,
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
        let consumer = match (env("MW_CONSUMER_TOKEN"), env("MW_CONSUMER_SECRET")) {
            (Some(key), Some(secret)) => Token { key, secret },
            _ => match (ini.get("consumerKey"), ini.get("consumerSecret")) {
                (Some(key), Some(secret)) => Token {
                    key: key.to_owned(),
                    secret: secret.to_owned(),
                },
                _ => {
                    return Err(QsError::ConfigError(
                        "No OAuth consumer: set MW_CONSUMER_TOKEN and MW_CONSUMER_SECRET, or oauth.ini_file".into(),
                    ))
                }
            },
        };
        // A consumer registered with a fixed callback URL must send "oob"
        let oob = env("MW_CONSUMER_CALLBACK_OOB").is_some_and(|v| v == "1" || v == "true");
        let callback = match tool_base {
            "./" => "oob".to_string(),
            _ if oob => "oob".to_string(),
            base => format!("{base}api.php"),
        };
        let user_agent = ini.get("agent").map_or(DEFAULT_USER_AGENT, String::as_str);
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(25))
            .build()
            .map_err(|e| QsError::ConfigError(e.to_string()))?;
        Ok(Some(Self {
            consumer,
            user_agent: user_agent.to_string(),
            url: url.to_string(),
            public_url: config["publicMwOAuthUrl"]
                .as_str()
                .unwrap_or(url)
                .to_string(),
            callback,
            http,
        }))
    }

    /// Fetches a request token, and returns it with the URL where the user authorizes it
    pub async fn request_token(&self) -> QsResult<(Token, String)> {
        let token = self
            .fetch_token("initiate", None, &[("oauth_callback", &self.callback)])
            .await?;
        let separator = if self.public_url.contains('?') {
            '&'
        } else {
            '?'
        };
        let authorize = format!(
            "{}/authorize{separator}oauth_token={}&oauth_consumer_key={}",
            self.public_url,
            encode(&token.key),
            encode(&self.consumer.key)
        );
        Ok((token, authorize))
    }

    /// Exchanges an authorized request token for an access token
    pub async fn access_token(&self, request: &Token, verifier: &str) -> QsResult<Token> {
        self.fetch_token("token", Some(request), &[("oauth_verifier", verifier)])
            .await
    }

    /// The user's `userinfo` from `api_url`, or `None` if the token is not (or no longer) valid
    pub async fn userinfo(&self, api_url: &str, token: &Token) -> QsResult<Option<Value>> {
        let params = [
            ("action", "query"),
            ("meta", "userinfo"),
            ("uiprop", "blockinfo|groups|rights"),
            ("format", "json"),
        ];
        let url = Url::parse_with_params(api_url, params).map_err(|e| e.to_string())?;
        let response: Value = serde_json::from_str(&self.get(url, Some(token), &[]).await?)?;
        let userinfo = &response["query"]["userinfo"];
        Ok(userinfo["id"]
            .as_i64()
            .filter(|id| *id > 0)
            .map(|_| userinfo.clone()))
    }

    /// The credentials the bot edits with, as stored in `batch_oauth.serialized_json`
    pub fn batch_credentials(&self, token: &Token) -> Value {
        json!({
            "gConsumerKey": self.consumer.key,
            "gConsumerSecret": self.consumer.secret,
            "gTokenKey": token.key,
            "gTokenSecret": token.secret,
            "gUserAgent": self.user_agent,
        })
    }

    pub fn api_params(&self, token: &Token) -> OAuthParams {
        OAuthParams::new_from_json(&self.batch_credentials(token))
    }

    async fn fetch_token(
        &self,
        action: &str,
        token: Option<&Token>,
        oauth_params: &[(&str, &str)],
    ) -> QsResult<Token> {
        let url = format!("{}/{action}", self.url);
        let url = Url::parse_with_params(&url, [("format", "json")]).map_err(|e| e.to_string())?;
        parse_token(&self.get(url, token, oauth_params).await?)
    }

    /// A signed GET request
    async fn get(
        &self,
        url: Url,
        token: Option<&Token>,
        extra_oauth_params: &[(&str, &str)],
    ) -> QsResult<String> {
        let nonce = random_string(32);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let mut params = vec![
            ("oauth_consumer_key", self.consumer.key.as_str()),
            ("oauth_nonce", &nonce),
            ("oauth_signature_method", "HMAC-SHA1"),
            ("oauth_timestamp", &timestamp),
            ("oauth_version", "1.0"),
        ];
        if let Some(token) = token {
            params.push(("oauth_token", &token.key));
        }
        params.extend_from_slice(extra_oauth_params);
        let token_secret = token.map_or("", |t| t.secret.as_str());
        let signature = sign("GET", &url, &params, &self.consumer.secret, token_secret);
        params.push(("oauth_signature", &signature));
        let header = params
            .iter()
            .map(|(k, v)| format!("{}=\"{}\"", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join(", ");
        let request = self
            .http
            .get(url)
            .header(AUTHORIZATION, format!("OAuth {header}"));
        let response = request.send().await.map_err(oauth_error)?;
        response.text().await.map_err(oauth_error)
    }
}

fn oauth_error(e: reqwest::Error) -> QsError {
    QsError::StringError(format!("OAuth request failed: {e}"))
}

/// The OAuth 1.0a HMAC-SHA1 signature of a request; `params` are the
/// parameters besides those in the URL's query
fn sign(
    method: &str,
    url: &Url,
    params: &[(&str, &str)],
    consumer_secret: &str,
    token_secret: &str,
) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (encode(&k), encode(&v)))
        .chain(params.iter().map(|(k, v)| (encode(k), encode(v))))
        .collect();
    pairs.sort();
    let normalized = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    // Url::port() is None for the scheme's default port, which is left out
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let host = url.host_str().unwrap_or_default();
    let base_url = format!("{}://{host}{port}{}", url.scheme(), url.path());
    let base = format!("{method}&{}&{}", encode(&base_url), encode(&normalized));
    let key = format!("{}&{}", encode(consumer_secret), encode(token_secret));
    let mut mac =
        Hmac::<Sha1>::new_from_slice(key.as_bytes()).expect("HMAC takes keys of any length");
    mac.update(base.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// A token from Special:OAuth, as JSON (`key`/`secret`, or the
/// `oauth_token`/`oauth_token_secret` that newer versions send) or form data
fn parse_token(text: &str) -> QsResult<Token> {
    let fields: HashMap<String, String> = match serde_json::from_str(text) {
        Ok(Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
            .collect(),
        _ => form_urlencoded::parse(text.as_bytes())
            .into_owned()
            .collect(),
    };
    let field = |a: &str, b: &str| fields.get(a).or_else(|| fields.get(b)).cloned();
    match (
        field("key", "oauth_token"),
        field("secret", "oauth_token_secret"),
    ) {
        (Some(key), Some(secret)) => Ok(Token { key, secret }),
        _ => Err(QsError::StringError(format!(
            "Invalid response from Special:OAuth: {text}"
        ))),
    }
}

/// The `key = value` lines of an ini file without sections, like PHP's `parse_ini_file`
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

    // The worked example from Twitter's (X's) OAuth 1.0a documentation
    #[test]
    fn sign_matches_reference_signature() {
        let url =
            Url::parse("https://api.twitter.com/1.1/statuses/update.json?include_entities=true")
                .unwrap();
        let params = [
            (
                "status",
                "Hello Ladies + Gentlemen, a signed OAuth request!",
            ),
            ("oauth_consumer_key", "xvz1evFS4wEEPTGEFPHBog"),
            ("oauth_nonce", "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg"),
            ("oauth_signature_method", "HMAC-SHA1"),
            ("oauth_timestamp", "1318622958"),
            (
                "oauth_token",
                "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb",
            ),
            ("oauth_version", "1.0"),
        ];
        let signature = sign(
            "POST",
            &url,
            &params,
            "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw",
            "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE",
        );
        assert_eq!(signature, "hCtSmYh+iHYCEqBWrE7C7hYmtUk=");
    }

    #[test]
    fn parse_token_formats() {
        let token = Token {
            key: "k".into(),
            secret: "s".into(),
        };
        assert_eq!(parse_token(r#"{"key":"k","secret":"s"}"#).unwrap(), token);
        assert_eq!(
            parse_token(r#"{"oauth_token":"k","oauth_token_secret":"s"}"#).unwrap(),
            token
        );
        assert_eq!(
            parse_token("oauth_token=k&oauth_token_secret=s&oauth_callback_confirmed=true")
                .unwrap(),
            token
        );
        assert!(parse_token(r#"{"error":"mwoauth-invalid-authorization"}"#).is_err());
    }

    #[test]
    fn parse_ini_reads_keys() {
        let ini = parse_ini("; comment\n[section]\nagent = QS test\nconsumerKey = \"abc\"\n");
        assert_eq!(ini["agent"], "QS test");
        assert_eq!(ini["consumerKey"], "abc");
        assert_eq!(ini.len(), 2);
    }

    #[test]
    fn from_site_without_oauth_is_none() {
        assert!(OAuth::from_site(&json!({}), "https://qs.example/")
            .unwrap()
            .is_none());
    }

    #[test]
    fn authorize_url_and_credentials() {
        let site = json!({"oauth":{"mwOAuthUrl":"https://wiki.example/w/index.php?title=Special:OAuth","ini_file":"/nonexistent"}});
        // Without the environment variables, the missing ini file is an error
        if std::env::var("MW_CONSUMER_TOKEN").is_err() {
            assert!(OAuth::from_site(&site, "https://qs.example/").is_err());
        }
        let oauth = OAuth {
            consumer: Token {
                key: "ck".into(),
                secret: "cs".into(),
            },
            user_agent: "ua".into(),
            url: "u".into(),
            public_url: "u".into(),
            callback: "oob".into(),
            http: reqwest::Client::new(),
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
