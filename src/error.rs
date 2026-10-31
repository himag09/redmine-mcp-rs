//! Error types used across the server.
//!
//! Error messages are shown to the language model inside tool results, so they
//! must stay actionable and must never contain the API key.

use std::error::Error as StdError;
use std::fmt;

/// Convenient result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Every failure the server can produce.
#[derive(Debug)]
pub enum Error {
    /// Invalid server configuration (environment variables, URL, ...).
    Config(String),
    /// Invalid tool arguments supplied by the client.
    Validation(String),
    /// JSON-RPC protocol violation.
    Protocol(String),
    /// A non-successful HTTP response returned by Redmine.
    Api { status: u16, message: String },
    /// Transport-level failure (DNS, TLS, timeout, connection reset, ...).
    Http(reqwest::Error),
    /// JSON (de)serialization failure.
    Json(serde_json::Error),
    /// Standard I/O failure.
    Io(std::io::Error),
}

impl Error {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub fn api(status: u16, message: impl Into<String>) -> Self {
        Self::Api {
            status,
            message: message.into(),
        }
    }

    /// Whether this failure is worth retrying. Only idempotent requests are
    /// retried, and only for transient failures.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Api { status, .. } => matches!(status, 429 | 502 | 503 | 504),
            Self::Http(err) => err.is_timeout() || err.is_connect(),
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "configuration error: {message}"),
            Self::Validation(message) => f.write_str(message),
            Self::Protocol(message) => write!(f, "protocol error: {message}"),
            Self::Api { status, message } => {
                write!(f, "Redmine API error (HTTP {status}): {message}")
            }
            Self::Http(err) => {
                write!(f, "Redmine request failed: {err}")?;
                // reqwest's Display is often the generic "error sending
                // request"; the source chain carries the useful detail
                // (connection refused, certificate error, ...).
                let mut source = StdError::source(err);
                while let Some(cause) = source {
                    write!(f, ": {cause}")?;
                    source = cause.source();
                }
                Ok(())
            }
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
