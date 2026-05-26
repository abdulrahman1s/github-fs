use thiserror::Error;

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("failed to build HTTP client: {0}")]
    Build(#[source] reqwest::Error),

    #[error("HTTP request failed: {0}")]
    Request(#[source] reqwest::Error),

    #[error("failed to decode response body: {0}")]
    Decode(#[source] reqwest::Error),

    #[error("unauthorized — GitHub token is missing, invalid, or expired (401)")]
    Unauthorized,

    #[error("forbidden (403): {0}")]
    Forbidden(String),

    #[error("rate limited (403); reset at unix={reset_unix:?}")]
    RateLimited { reset_unix: Option<u64> },

    #[error("not found (404)")]
    NotFound,

    #[error("unexpected status {status}: {body}")]
    Unexpected { status: u16, body: String },
}

impl GithubError {
    /// True when the error represents a transient failure where a retry could
    /// plausibly succeed without user intervention.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            GithubError::Request(_) | GithubError::Decode(_) | GithubError::RateLimited { .. }
        )
    }
}
