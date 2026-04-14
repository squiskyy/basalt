use crate::llm::error::{LlmError, LlmResult};
use crate::llm::provider::{LlmProvider, LlmRequest, LlmResponse, TokenUsage};
use async_trait::async_trait;
use std::time::Duration;

/// Configuration for an OpenAI-compatible LLM provider.
///
/// Works with OpenAI, Ollama, vLLM, LiteLLM, and any endpoint that
/// implements the OpenAI Chat Completions API.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// API key for authentication.
    pub api_key: String,
    /// Model to use (e.g., "gpt-4o-mini", "gpt-3.5-turbo").
    pub model: String,
    /// Base URL (default: "https://api.openai.com/v1").
    /// Override for OpenAI-compatible endpoints (Ollama, vLLM, etc.).
    pub base_url: String,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Default max tokens for completions.
    pub default_max_tokens: u32,
    /// Default temperature.
    pub default_temperature: f32,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            base_url: "https://api.openai.com/v1".into(),
            timeout_ms: 30_000,
            default_max_tokens: 1024,
            default_temperature: 0.3,
        }
    }
}

/// OpenAI-compatible LLM provider.
pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        if config.api_key.is_empty() {
            return Err(LlmError::ConfigError("api_key is required".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|e| LlmError::ConfigError(format!("failed to create HTTP client: {e}")))?;
        Ok(Self { config, client })
    }

    pub fn build_request_body(&self, request: LlmRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": request.messages,
        });
        let max_tokens = request.max_tokens.unwrap_or(self.config.default_max_tokens);
        if max_tokens > 0 {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }
        let temperature = request
            .temperature
            .unwrap_or(self.config.default_temperature);
        body["temperature"] = serde_json::json!(temperature);
        body
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, request: LlmRequest) -> LlmResult<LlmResponse> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = self.build_request_body(request);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    LlmError::Timeout(self.config.timeout_ms)
                } else {
                    LlmError::RequestFailed(e.to_string())
                }
            })?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "unknown error".into());
            return Err(LlmError::ApiError {
                status,
                message: error_body,
            });
        }

        let resp_body: OpenAiResponse = response
            .json()
            .await
            .map_err(|e| LlmError::ParseError(format!("failed to parse response: {e}")))?;

        let content = resp_body
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        Ok(LlmResponse {
            content,
            usage: resp_body.usage.map(|u| TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            }),
        })
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.config.model
    }
}

// OpenAI API response types (internal only)

#[derive(Debug, serde::Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiMessage {
    content: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}
