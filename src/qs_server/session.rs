//! The login session: OAuth tokens in encrypted cookies, so sessions survive
//! restarts without server-side storage.

use super::oauth::Token;
use super::AppState;
use crate::error::QsResult;
use axum_extra::extract::cookie::{Cookie, PrivateCookieJar, SameSite};
use serde_json::Value;

/// The access token of the logged-in user
const ACCESS_COOKIE: &str = "qs_access";
/// The request token of a login in progress
const REQUEST_COOKIE: &str = "qs_request";
/// As in PHP
const ACCESS_DAYS: i64 = 90;
const REQUEST_MINUTES: i64 = 30;

pub struct User {
    pub id: i64,
    pub name: String,
    pub token: Token,
    /// MediaWiki's `userinfo`, with groups, rights and block
    pub userinfo: Value,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.userinfo["groups"]
            .as_array()
            .is_some_and(|groups| groups.iter().any(|g| g == "sysop"))
    }
}

/// The logged-in user, if the session has a valid access token
pub async fn current_user(state: &AppState, jar: &PrivateCookieJar) -> QsResult<Option<User>> {
    let (Some(oauth), Some(token)) = (&state.oauth, token(jar, ACCESS_COOKIE)) else {
        return Ok(None);
    };
    let Some(userinfo) = oauth.userinfo(&state.default_api, &token).await? else {
        return Ok(None);
    };
    Ok(Some(User {
        id: userinfo["id"].as_i64().unwrap_or_default(),
        name: userinfo["name"].as_str().unwrap_or_default().to_string(),
        token,
        userinfo,
    }))
}

pub fn request_token(jar: &PrivateCookieJar) -> Option<Token> {
    token(jar, REQUEST_COOKIE)
}

pub fn start_login(jar: PrivateCookieJar, request: &Token, secure: bool) -> PrivateCookieJar {
    let cookie = cookie(
        REQUEST_COOKIE,
        request,
        cookie::time::Duration::minutes(REQUEST_MINUTES),
        secure,
    );
    jar.add(cookie)
}

pub fn finish_login(jar: PrivateCookieJar, access: &Token, secure: bool) -> PrivateCookieJar {
    let cookie = cookie(
        ACCESS_COOKIE,
        access,
        cookie::time::Duration::days(ACCESS_DAYS),
        secure,
    );
    jar.remove(removal(REQUEST_COOKIE)).add(cookie)
}

pub fn logout(jar: PrivateCookieJar) -> PrivateCookieJar {
    jar.remove(removal(ACCESS_COOKIE))
        .remove(removal(REQUEST_COOKIE))
}

fn token(jar: &PrivateCookieJar, name: &str) -> Option<Token> {
    serde_json::from_str(jar.get(name)?.value()).ok()
}

fn cookie(
    name: &'static str,
    token: &Token,
    max_age: cookie::time::Duration,
    secure: bool,
) -> Cookie<'static> {
    let value = serde_json::to_string(token).expect("a token serializes");
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .secure(secure)
        // Lax: sent when MediaWiki redirects back after authorization, but
        // not with requests from other sites
        .same_site(SameSite::Lax)
        .max_age(max_age)
        .build()
}

fn removal(name: &'static str) -> Cookie<'static> {
    Cookie::build(name).path("/").build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_extra::extract::cookie::Key;
    use serde_json::json;

    #[test]
    fn login_and_logout_round_trip() {
        let jar = PrivateCookieJar::new(Key::generate());
        let request = Token {
            key: "rk".into(),
            secret: "rs".into(),
        };
        let jar = start_login(jar, &request, true);
        assert_eq!(request_token(&jar), Some(request));

        let access = Token {
            key: "ak".into(),
            secret: "as".into(),
        };
        let jar = finish_login(jar, &access, true);
        assert_eq!(request_token(&jar), None);
        assert_eq!(token(&jar, ACCESS_COOKIE), Some(access));
        let cookie = jar.get(ACCESS_COOKIE).unwrap();
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(cookie.secure(), Some(true));

        let jar = logout(jar);
        assert_eq!(token(&jar, ACCESS_COOKIE), None);
    }

    #[test]
    fn admins_are_sysops() {
        let user = |groups| User {
            id: 1,
            name: "U".into(),
            token: Token {
                key: String::new(),
                secret: String::new(),
            },
            userinfo: json!({ "groups": groups }),
        };
        assert!(user(json!(["*", "sysop"])).is_admin());
        assert!(!user(json!(["*", "user"])).is_admin());
    }
}
