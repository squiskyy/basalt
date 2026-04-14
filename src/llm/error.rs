use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("LLM service not configured")]
    NotConfigured,

    #[error("LLM request failed: {0}")]
    RequestFailed(String),

    #[error("LLM response parse error: {0}")]
    ParseError(String),

    #[error("LLM API error: status {status} - {message}")]
    ApiError { status: u16, message: String },

    #[error("LLM request timed out after {0}ms")]
    Timeout(u64),

    #[error("configuration error: {0}")]
    ConfigError(String),
}

pub type LlmResult<T> = Result<T, LlmError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = LlmError::NotConfigured;
        assert_eq!(format!("{err}"), "LLM service not configured");

        let err = LlmError::Timeout(5000);
        assert_eq!(format!("{err}"), "LLM request timed out after 5000ms");

        let err = LlmError::ApiError {
            status: 429,
            message: "rate limited".into(),
        };
        assert_eq!(format!("{err}"), "LLM API error: status 429 - rate limited");
    }
}
