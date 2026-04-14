use crate::llm::error::LlmResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A single message in an LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

/// Request to an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub messages: Vec<LlmMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// Response from an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// The text content of the response.
    pub content: String,
    /// Token usage if reported by the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Trait for LLM providers. Each provider (OpenAI, Anthropic, custom) implements this.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Send a chat completion request and return the response.
    async fn complete(&self, request: LlmRequest) -> LlmResult<LlmResponse>;

    /// Return the provider name for logging.
    fn name(&self) -> &str;

    /// Return the model identifier being used.
    fn model(&self) -> &str;
}

impl LlmRequest {
    /// Create a simple single-message request.
    pub fn from_prompt(content: impl Into<String>) -> Self {
        Self {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: content.into(),
            }],
            max_tokens: None,
            temperature: None,
        }
    }

    /// Create a request with a system message and user message.
    pub fn from_system_and_user(system: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            messages: vec![
                LlmMessage {
                    role: "system".into(),
                    content: system.into(),
                },
                LlmMessage {
                    role: "user".into(),
                    content: user.into(),
                },
            ],
            max_tokens: None,
            temperature: None,
        }
    }

    /// Set max tokens.
    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }

    /// Set temperature.
    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }
}
