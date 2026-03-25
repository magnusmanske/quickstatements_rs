use thiserror::Error;

/// Unified error type for the quickstatements crate.
///
/// Start with `StringError` as a catch-all so existing `Result<_, String>` code
/// can migrate incrementally: convert a module at a time and carve out typed
/// variants as you go.
#[derive(Debug, Error)]
pub enum QsError {
    /// Catch-all for legacy `String` errors — migrate away from this over time.
    #[error("{0}")]
    StringError(String),

    /// MediaWiki API returned an error response.
    #[error("API error {code}: {info}")]
    ApiError { code: String, info: String },

    /// MediaWiki API returned non-JSON (e.g. HTML rate-limit page).
    #[error("Non-JSON API response: {0}")]
    NonJsonResponse(String),

    /// Rate-limited or throttled by the API.
    #[error("Rate limited ({code})")]
    RateLimited { code: String },

    /// A required entity (item, property, lexeme) was not found.
    #[error("Entity not found: {0}")]
    EntityNotFound(String),

    /// Command parsing failed.
    #[error("Parse error: {0}")]
    ParseError(String),

    /// Database operation failed.
    #[error("Database error: {0}")]
    DatabaseError(String),

    /// Configuration or setup error.
    #[error("Config error: {0}")]
    ConfigError(String),

    /// Configuration or setup error.
    #[error("mysql_async error: {0}")]
    MysqlAsyncError(mysql_async::Error),

    /// Error from the mediawiki crate.
    #[error("MediaWiki error: {0}")]
    MediaWikiError(wikibase::mediawiki::MediaWikiError),

    /// Batch status error
    #[error("Batch #{0} is not RUN or INIT")]
    BatchStatusError(i64),

    #[error("No match ID set")]
    NoMatchSetError,
}

impl From<String> for QsError {
    fn from(s: String) -> Self {
        QsError::StringError(s)
    }
}

impl From<mysql_async::Error> for QsError {
    fn from(e: mysql_async::Error) -> Self {
        QsError::MysqlAsyncError(e)
    }
}

impl From<wikibase::mediawiki::MediaWikiError> for QsError {
    fn from(e: wikibase::mediawiki::MediaWikiError) -> Self {
        QsError::MediaWikiError(e)
    }
}

impl From<&str> for QsError {
    fn from(s: &str) -> Self {
        QsError::StringError(s.to_string())
    }
}

/// Convenience alias used throughout the crate.
pub type QsResult<T> = Result<T, QsError>;
