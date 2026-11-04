use std::path::PathBuf;
use thiserror::Error;
use wikimisc::mysql_async;
use wikimisc::oauth1;
use wikimisc::toolforge;
use wikimisc::wikibase;

/// Unified error type for the quickstatements crate.
#[derive(Debug, Error)]
pub enum QsError {
    /// MediaWiki API returned an error response.
    #[error("API error {code}: {info}")]
    ApiError { code: String, info: String },

    /// The API response did not contain the expected result.
    #[error("API result error: {0}")]
    ApiResultError(String),

    /// MediaWiki API returned non-JSON (e.g. HTML rate-limit page).
    #[error("Non-JSON API response: {0}")]
    NonJsonResponse(String),

    /// Rate-limited or throttled by the API.
    #[error("Rate limited ({code})")]
    RateLimited { code: String },

    /// A required entity (item, property, lexeme) was not found.
    #[error("Entity not found: {0}")]
    EntityNotFound(String),

    /// An entity could not be loaded from the wiki.
    #[error("Error while loading entity {entity_id} (revision {revision:?}): {source}")]
    EntityLoadError {
        entity_id: String,
        revision: Option<usize>,
        #[source]
        source: wikibase::WikibaseError,
    },

    /// Command parsing failed.
    #[error("Parse error: {0}")]
    ParseError(String),

    /// The command JSON is invalid or incomplete.
    #[error("Invalid command: {0}")]
    InvalidCommand(String),

    /// A command was executed before and is flagged as already done.
    #[error("Command is already done (duplicate)")]
    AlreadyDone,

    /// A command parameter value is not a string.
    #[error("API parameter '{name}' is not a string: {value}")]
    InvalidApiParam { name: String, value: String },

    /// No entity (or LAST entity) is available to operate on.
    #[error("No (last) item available")]
    NoItemAvailable,

    /// The bot has no MediaWiki API client.
    #[error("No MediaWiki API client configured")]
    NoMwApi,

    /// The user is blocked and cannot edit.
    #[error("User is blocked")]
    UserBlocked,

    /// The bot's user could not be found.
    #[error("User #{0} not found")]
    UserNotFound(i64),

    /// Too many throttle retries for a command.
    #[error("Too many throttle retries ({retries}) for command #{command_id}")]
    TooManyThrottleRetries { retries: usize, command_id: i64 },

    /// Database operation failed.
    #[error("Database error: {0}")]
    DatabaseError(String),

    /// Configuration or setup error.
    #[error("Config error: {0}")]
    ConfigError(String),

    /// The bot was started without a batch ID.
    #[error("No batch ID set")]
    NoBatchId,

    /// The batch has no site/API URL, so the bot cannot edit.
    #[error("No site/API info available for batch #{0}")]
    NoApiInfo(i64),

    /// The batch could not be (re)started (e.g. it was stopped).
    #[error("Cannot (re)start batch #{0}")]
    CannotRestartBatch(i64),

    /// Batch status error
    #[error("Batch #{0} is not RUN or INIT")]
    BatchStatusError(i64),

    /// A batch changed while it was being archived.
    #[error("Batch #{0} changed while archiving; its commands were kept live")]
    BatchChangedDuringArchive(i64),

    /// An archive file did not read back as written.
    #[error("Archive file {} does not read back correctly", .path.display())]
    ArchiveVerifyError { path: PathBuf },

    /// An archive read task failed.
    #[error("Archive read failed: {0}")]
    ArchiveReadError(String),

    /// The LAST state could not be persisted.
    #[error("Cannot persist LAST state for batch #{0}")]
    LastStateUpdateFailed(i64),

    /// The command status could not be persisted.
    #[error("Cannot update status for command #{command_id} in batch #{batch_id}")]
    CommandStatusUpdateFailed { batch_id: i64, command_id: i64 },

    /// OAuth request failed.
    #[error("OAuth request failed: {0}")]
    OAuthError(#[from] oauth1::OAuthError),

    /// Database driver error.
    #[error("mysql_async error: {0}")]
    MysqlAsyncError(mysql_async::Error),

    /// Error from the mediawiki crate.
    #[error("MediaWiki error: {0}")]
    MediaWikiError(wikimisc::mediawiki::MediaWikiError),

    /// Error from the wikibase crate.
    #[error("Wikibase error: {0}")]
    WikibaseError(#[from] wikibase::WikibaseError),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}

impl From<mysql_async::Error> for QsError {
    fn from(e: mysql_async::Error) -> Self {
        QsError::MysqlAsyncError(e)
    }
}

impl From<toolforge::Error> for QsError {
    fn from(e: toolforge::Error) -> Self {
        match e {
            // Kept apart, as the bot treats MySQL errors as transient
            toolforge::Error::MysqlError(e) => QsError::MysqlAsyncError(e),
            e => QsError::DatabaseError(e.to_string()),
        }
    }
}

impl From<wikimisc::mediawiki::MediaWikiError> for QsError {
    fn from(e: wikimisc::mediawiki::MediaWikiError) -> Self {
        QsError::MediaWikiError(e)
    }
}

/// Convenience alias used throughout the crate.
pub type QsResult<T> = Result<T, QsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolforge_mysql_errors_stay_mysql_errors() {
        // The bot retries on MySQL errors, so they must not become DatabaseError
        let e =
            toolforge::Error::MysqlError(mysql_async::Error::Url(mysql_async::UrlError::Invalid));
        assert!(matches!(QsError::from(e), QsError::MysqlAsyncError(_)));
        let e = toolforge::Error::InvalidPoolMax(0);
        assert!(matches!(QsError::from(e), QsError::DatabaseError(_)));
    }
}
