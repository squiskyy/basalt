use crate::llm::error::{LlmError, LlmResult};
use crate::llm::provider::{LlmProvider, LlmRequest, LlmResponse, TokenUsage};
use async_trait::async_trait;
use std::time::Duration;

/// Configuration for the Anthropic LLM provider.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// API key for authentication.
    pub api_key: String,
    /// Model to use (e.g., "claude-sonnet-4-20250514", "claude-haiku-4-20250414").
    pub model: String,
    /// Base URL (default: "https://api.anthropic.com").
    pub base_url: String,
    /// API version header.
    pub api_version: String,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Default max tokens for completions.
    pub default_max_tokens: u32,
    /// Default temperature.
    pub default_temperature: f32,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "claude-sonnet-4-20250514".into(),
            base_url: "https://api.anthropic.com".into(),
            api_version: "2023-06-01".into(),
            timeout_ms: 30_000,
            default_max_tokens: 1024,
            default_temperature: 0.3,
        }
    }
}

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Result<Self, LlmError> {
        if config.api_key.is_empty() {
            return Err(LlmError::ConfigError("api_key is required".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|e| LlmError::ConfigError(format!("failed to create HTTP client: {e}")))?;
        Ok(Self { config, client })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, request: LlmRequest) -> LlmResult<LlmResponse> {
        let url = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));

        // Anthropic requires max_tokens and separates system messages.
        let mut system_content: Option<String> = None;
        let mut messages = Vec::new();
        for msg in request.messages {
            if msg.role == "system" {
                system_content = Some(msg.content);
            } else {
                messages.push(serde_json::json!({
                    "role": msg.role,
                    "content": msg.content,
                }));
            }
        }

        let max_tokens = request.max_tokens.unwrap_or(self.config.default_max_tokens);
        let temperature = request
            .temperature
            .unwrap_or(self.config.default_temperature);

        let mut body = serde_json::json!({
            "model": self.config.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "temperature": temperature,
        });
        if let Some(ref system) = system_content {
            body["system"] = serde_json::json!(system);
        }

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", &self.config.api_version)
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

        let resp_body: AnthropicResponse = response
            .json()
            .await
            .map_err(|e| LlmError::ParseError(format!("failed to parse response: {e}")))?;

        let content = resp_body
            .content
            .first()
            .and_then(|block| {
                if block.block_type == "text" {
                    block.text.clone()
                } else {
                    None
                }
            })
            .unwrap_or_default();

        Ok(LlmResponse {
            content,
            usage: resp_body.usage.map(|u| TokenUsage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.input_tokens + u.output_tokens,
            }),
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.config.model
    }
}

// Anthropic API response types (internal only)

#[derive(Debug, serde::Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, serde::Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}
