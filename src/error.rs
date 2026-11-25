//! Error type for the whole crate.

use thiserror::Error;

/// Every fallible operation in this crate returns this error.
#[derive(Debug, Error)]
pub enum Error {
    /// A required environment variable is not set. The startup log prints the name.
    #[error("missing required environment variable: {0}")]
    MissingEnv(String),
    /// An environment variable is set but its value is not valid.
    #[error("invalid environment variable {name}: {message}")]
    InvalidEnv { name: String, message: String },
    /// Transport-level HTTP failure.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// Home Connect answered with a non-success status code.
    #[error("home connect api error {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
    /// A response body did not parse.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Local socket or file failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Home Connect answered 429. The value is the Retry-After header in seconds.
    #[error("rate limited; retry after {retry_after_secs} seconds")]
    RateLimited { retry_after_secs: u64 },
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
