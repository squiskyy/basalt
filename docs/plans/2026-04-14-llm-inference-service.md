# LLM Inference Service Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add a configurable LLM inference service to Basalt that can call external LLM APIs (OpenAI, Anthropic, custom endpoints) for background tasks like memory summarization, consolidation, and relevance scoring.

**Architecture:** A new `src/llm/` module with a trait-based provider abstraction. Configurable via TOML and CLI. Runs as a background tokio task, not on the request path. The service provides an `LlmClient` that the consolidation/relevance/summarization features (issues #42-44) will call into.

**Tech Stack:** `reqwest` (already in dev-deps, move to main), `serde`/`serde_json` (already in), `tokio` (already in), trait objects for provider abstraction.

---

### Task 1: Add reqwest as a runtime dependency

**Objective:** Move reqwest from dev-deps to main dependencies so the LLM client can make HTTP calls.

**Files:**
- Modify: `Cargo.toml`

**Step 1: Edit Cargo.toml**

Add reqwest to `[dependencies]` with the `json` feature:

```toml
[dependencies]
# ... existing deps ...
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
```

Keep the existing dev-dependency entry for reqwest (it will use the same version). Make sure `default-features = false` and `rustls-tls` feature so we don't pull in OpenSSL.

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: compiles without errors

**Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "feat: add reqwest as runtime dependency for LLM inference"
```

---

### Task 2: Create the LLM module skeleton with LlmProvider trait

**Objective:** Define the provider trait and error types that all LLM backends will implement.

**Files:**
- Create: `src/llm/mod.rs`
- Create: `src/llm/provider.rs`
- Create: `src/llm/error.rs`
- Modify: `src/lib.rs`

**Step 1: Create `src/llm/error.rs`**

```rust
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
```

**Step 2: Create `src/llm/provider.rs`**

```rust
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
```

**Step 3: Add async_trait dependency to Cargo.toml**

```toml
async-trait = "0.1"
```

**Step 4: Create `src/llm/mod.rs`**

```rust
pub mod error;
pub mod provider;

pub use error::{LlmError, LlmResult};
pub use provider::{LlmMessage, LlmProvider, LlmRequest, LlmResponse, TokenUsage};
```

**Step 5: Modify `src/lib.rs`**

Add `pub mod llm;` after the existing module declarations.

**Step 6: Verify it compiles**

Run: `cargo check`
Expected: compiles without errors

**Step 7: Commit**

```bash
git add src/llm/ src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat: add LLM module with LlmProvider trait and error types"
```

---

### Task 3: Implement the OpenAI-compatible provider

**Objective:** Implement an LlmProvider for OpenAI's Chat Completions API (also works with any OpenAI-compatible endpoint like Ollama, vLLM, LiteLLM, etc.).

**Files:**
- Create: `src/llm/openai.rs`
- Modify: `src/llm/mod.rs`

**Step 1: Create `src/llm/openai.rs`**

```rust
use async_trait::async_trait;
use crate::llm::error::{LlmError, LlmResult};
use crate::llm::provider::{LlmProvider, LlmRequest, LlmResponse, TokenUsage};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Configuration for an OpenAI-compatible LLM provider.
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

    fn build_request_body(&self, request: LlmRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": request.messages,
        });
        let max_tokens = request
            .max_tokens
            .unwrap_or(self.config.default_max_tokens);
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
        let url = format!("{}/chat/completions", self.config.base_url.trim_end_matches('/'));
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
            let error_body = response.text().await.unwrap_or_else(|_| "unknown error".into());
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

// OpenAI API response types (not exported, only used internally)

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}
```

**Step 2: Update `src/llm/mod.rs`**

Add the openai module and re-export:

```rust
pub mod error;
pub mod openai;
pub mod provider;

pub use error::{LlmError, LlmResult};
pub use openai::{OpenAiConfig, OpenAiProvider};
pub use provider::{LlmMessage, LlmProvider, LlmRequest, LlmResponse, TokenUsage};
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles without errors

**Step 4: Commit**

```bash
git add src/llm/
git commit -m "feat: add OpenAI-compatible LLM provider"
```

---

### Task 4: Implement the Anthropic provider

**Objective:** Implement an LlmProvider for Anthropic's Messages API.

**Files:**
- Create: `src/llm/anthropic.rs`
- Modify: `src/llm/mod.rs`

**Step 1: Create `src/llm/anthropic.rs`**

```rust
use async_trait::async_trait;
use crate::llm::error::{LlmError, LlmResult};
use crate::llm::provider::{LlmMessage, LlmProvider, LlmRequest, LlmResponse, TokenUsage};
use serde::Deserialize;
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
        let url = format!(
            "{}/v1/messages",
            self.config.base_url.trim_end_matches('/')
        );

        // Anthropic requires max_tokens. Split system message if present.
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

        let max_tokens = request
            .max_tokens
            .unwrap_or(self.config.default_max_tokens);
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
            let error_body = response.text().await.unwrap_or_else(|_| "unknown error".into());
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

// Anthropic API response types

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}
```

**Step 2: Update `src/llm/mod.rs`**

```rust
pub mod anthropic;
pub mod error;
pub mod openai;
pub mod provider;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use error::{LlmError, LlmResult};
pub use openai::{OpenAiConfig, OpenAiProvider};
pub use provider::{LlmMessage, LlmProvider, LlmRequest, LlmResponse, TokenUsage};
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles without errors

**Step 4: Commit**

```bash
git add src/llm/
git commit -m "feat: add Anthropic LLM provider"
```

---

### Task 5: Create the LlmClient facade

**Objective:** Create a high-level `LlmClient` that wraps an `Arc<dyn LlmProvider>`, provides convenience methods (summarize, classify), and handles the "not configured" case gracefully.

**Files:**
- Create: `src/llm/client.rs`
- Modify: `src/llm/mod.rs`

**Step 1: Create `src/llm/client.rs`**

```rust
use crate::llm::error::{LlmError, LlmResult};
use crate::llm::provider::{LlmProvider, LlmRequest, LlmResponse};
use std::sync::Arc;

/// High-level LLM client that wraps a provider.
///
/// When no LLM is configured, the client is in a "disabled" state and all
/// calls return `LlmError::NotConfigured`. This lets the rest of the codebase
/// call LLM functions without gating on config checks everywhere.
pub struct LlmClient {
    provider: Option<Arc<dyn LlmProvider>>,
}

impl LlmClient {
    /// Create a disabled client (no LLM configured).
    pub fn disabled() -> Self {
        Self { provider: None }
    }

    /// Create a client with a specific provider.
    pub fn with_provider(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider: Some(provider),
        }
    }

    /// Check if the LLM client is configured and enabled.
    pub fn is_enabled(&self) -> bool {
        self.provider.is_some()
    }

    /// Get the provider name, or "disabled" if not configured.
    pub fn provider_name(&self) -> &str {
        match &self.provider {
            Some(p) => p.name(),
            None => "disabled",
        }
    }

    /// Get the model name, or "none" if not configured.
    pub fn model_name(&self) -> &str {
        match &self.provider {
            Some(p) => p.model(),
            None => "none",
        }
    }

    /// Send a chat completion request.
    pub async fn complete(&self, request: LlmRequest) -> LlmResult<LlmResponse> {
        match &self.provider {
            Some(p) => p.complete(request).await,
            None => Err(LlmError::NotConfigured),
        }
    }

    /// Summarize a list of text entries into a single summary.
    ///
    /// This is the core operation for memory consolidation (issue #43) and
    /// summarization triggers (issue #44). It takes multiple memory values
    /// and produces a condensed summary.
    pub async fn summarize(&self, entries: &[String], context: &str) -> LlmResult<String> {
        let combined = entries.join("\n---\n");
        let system = format!(
            "You are a memory consolidation engine. Your task is to summarize \
             multiple related memories into a single, concise, dense summary. \
             Preserve all important facts, preferences, and patterns. \
             Remove redundancy. Output only the summary, no meta-commentary.\n\n\
             Context: {context}"
        );
        let user = format!(
            "Summarize the following {count} memory entries into a single concise summary:\n\n\
             {combined}",
            count = entries.len()
        );

        let request = LlmRequest::from_system_and_user(&system, &user)
            .with_max_tokens(512)
            .with_temperature(0.2);

        let response = self.complete(request).await?;
        Ok(response.content.trim().to_string())
    }

    /// Classify a memory entry's relevance or type.
    ///
    /// Used by relevance decay (issue #42) to score memory importance
    /// and by consolidation to determine memory type.
    pub async fn classify_relevance(
        &self,
        memory_key: &str,
        memory_value: &str,
        memory_type: &str,
    ) -> LlmResult<f32> {
        let system = "You are a memory relevance classifier. Given a memory entry, \
                      score its long-term relevance from 0.0 to 1.0. \
                      Permanent facts and skills score high (0.8-1.0). \
                      Transient observations score low (0.1-0.4). \
                      Patterns and preferences score medium (0.5-0.7). \
                      Output ONLY a single float number, nothing else.";

        let user = format!(
            "Key: {key}\nValue: {value}\nType: {mtype}\n\nRelevance score:",
            key = memory_key,
            value = memory_value,
            mtype = memory_type
        );

        let request = LlmRequest::from_system_and_user(system, &user)
            .with_max_tokens(10)
            .with_temperature(0.0);

        let response = self.complete(request).await?;
        let score: f32 = response
            .content
            .trim()
            .parse()
            .unwrap_or(0.5);
        Ok(score.clamp(0.0, 1.0))
    }
}

// LlmClient needs to be Send + Sync for use in Arc<> across the codebase.
// Since the provider is Arc<dyn LlmProvider + Send + Sync>, this is satisfied.
unsafe impl Send for LlmClient {}
unsafe impl Sync for LlmClient {}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.provider {
            Some(p) => write!(f, "LlmClient({}/{})", p.name(), p.model()),
            None => write!(f, "LlmClient(disabled)"),
        }
    }
}
```

**Step 2: Update `src/llm/mod.rs`**

```rust
pub mod anthropic;
pub mod client;
pub mod error;
pub mod openai;
pub mod provider;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use client::LlmClient;
pub use error::{LlmError, LlmResult};
pub use openai::{OpenAiConfig, OpenAiProvider};
pub use provider::{LlmMessage, LlmProvider, LlmRequest, LlmResponse, TokenUsage};
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles without errors

**Step 4: Commit**

```bash
git add src/llm/
git commit -m "feat: add LlmClient facade with summarize and classify_relevance"
```

---

### Task 6: Add LLM configuration to Config

**Objective:** Add LLM configuration section to the existing Config struct, TOML file format, and CLI args.

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs`

**Step 1: Add LlmConfig to `src/config.rs`**

After the `AuthConfig` struct, add:

```rust
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// LLM provider: "openai", "anthropic", or "custom".
    /// Empty = LLM disabled.
    pub provider: String,
    /// API key for the LLM provider.
    pub api_key: String,
    /// Model to use (provider-specific).
    pub model: String,
    /// Base URL override (for OpenAI-compatible endpoints like Ollama, vLLM).
    /// Empty = use provider default.
    pub base_url: String,
    /// API version header (Anthropic only).
    pub api_version: String,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Default max tokens for completions.
    pub default_max_tokens: u32,
    /// Default temperature for completions.
    pub default_temperature: f32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: String::new(), // empty = disabled
            api_key: String::new(),
            model: String::new(),
            base_url: String::new(),
            api_version: "2023-06-01".into(),
            timeout_ms: 30_000,
            default_max_tokens: 1024,
            default_temperature: 0.3,
        }
    }
}

impl LlmConfig {
    /// Check if LLM is enabled (provider is set).
    pub fn is_enabled(&self) -> bool {
        !self.provider.is_empty()
    }
}
```

Add `pub llm: LlmConfig` to the `Config` struct:

```rust
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub llm: LlmConfig,
}
```

Update `Config::default()`:

```rust
impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            llm: LlmConfig::default(),
        }
    }
}
```

Add `LlmFile` struct for TOML deserialization:

```rust
#[derive(Debug, Clone, serde::Deserialize, Default)]
struct LlmFile {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_version: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    default_max_tokens: Option<u32>,
    #[serde(default)]
    default_temperature: Option<f32>,
}
```

Add `llm: LlmFile` to `ConfigFile`:

```rust
struct ConfigFile {
    #[serde(default)]
    server: ServerFile,
    #[serde(default)]
    auth: AuthFile,
    #[serde(default)]
    llm: LlmFile,
}
```

In `Config::from_file()`, after the auth block, add LLM config construction:

```rust
let llm_defaults = LlmConfig::default();
let llm = LlmConfig {
    provider: file.llm.provider.unwrap_or_default(),
    api_key: file.llm.api_key.unwrap_or_default(),
    model: file.llm.model.unwrap_or_default(),
    base_url: file.llm.base_url.unwrap_or_default(),
    api_version: file.llm.api_version.unwrap_or(llm_defaults.api_version),
    timeout_ms: file.llm.timeout_ms.unwrap_or(llm_defaults.timeout_ms),
    default_max_tokens: file.llm.default_max_tokens.unwrap_or(llm_defaults.default_max_tokens),
    default_temperature: file.llm.default_temperature.unwrap_or(llm_defaults.default_temperature),
};
```

Update the return to include `llm`:

```rust
Ok(Config { server, auth, llm })
```

Add LLM CLI overrides to `apply_cli_overrides`:

```rust
pub fn apply_cli_overrides(
    &mut self,
    // ... existing params ...
    llm_provider: Option<String>,
    llm_api_key: Option<String>,
    llm_model: Option<String>,
    llm_base_url: Option<String>,
) {
    // ... existing overrides ...
    if let Some(v) = llm_provider {
        self.llm.provider = v;
    }
    if let Some(v) = llm_api_key {
        self.llm.api_key = v;
    }
    if let Some(v) = llm_model {
        self.llm.model = v;
    }
    if let Some(v) = llm_base_url {
        self.llm.base_url = v;
    }
}
```

**Step 2: Add CLI args to `src/main.rs`**

Add these to the `Args` struct:

```rust
    /// LLM provider: "openai", "anthropic", or "custom" (overrides config file, empty = disabled)
    #[arg(long, value_name = "PROVIDER")]
    llm_provider: Option<String>,

    /// LLM API key (overrides config file)
    #[arg(long, value_name = "KEY")]
    llm_api_key: Option<String>,

    /// LLM model to use (overrides config file)
    #[arg(long, value_name = "MODEL")]
    llm_model: Option<String>,

    /// LLM base URL override for OpenAI-compatible endpoints (overrides config file)
    #[arg(long, value_name = "URL")]
    llm_base_url: Option<String>,
```

Update the `apply_cli_overrides` call in main to include the new args.

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles without errors (there will be warnings about unused llm fields - that's fine)

**Step 4: Run existing tests**

Run: `cargo test`
Expected: all existing tests pass

**Step 5: Commit**

```bash
git add src/config.rs src/main.rs
git commit -m "feat: add LLM configuration to Config, CLI, and TOML"
```

---

### Task 7: Wire up LlmClient in main.rs

**Objective:** Construct the LlmClient from config in main.rs and log its status on startup. Pass it through to subsystems that will need it later.

**Files:**
- Modify: `src/main.rs`

**Step 1: Add LlmClient construction in main.rs**

After the auth store construction and before the engine creation, add:

```rust
use basalt::llm::LlmClient;

// ... in main(), after auth construction ...

// Construct LLM client from config
let llm_client = if cfg.llm.is_enabled() {
    match cfg.llm.provider.as_str() {
        "openai" | "custom" => {
            let mut openai_config = basalt::llm::OpenAiConfig {
                api_key: cfg.llm.api_key.clone(),
                model: if cfg.llm.model.is_empty() {
                    "gpt-4o-mini".into()
                } else {
                    cfg.llm.model.clone()
                },
                base_url: if cfg.llm.base_url.is_empty() {
                    "https://api.openai.com/v1".into()
                } else {
                    cfg.llm.base_url.clone()
                },
                timeout_ms: cfg.llm.timeout_ms,
                default_max_tokens: cfg.llm.default_max_tokens,
                default_temperature: cfg.llm.default_temperature,
            };
            if cfg.llm.provider == "custom" && openai_config.base_url == "https://api.openai.com/v1" {
                eprintln!("error: LLM provider is 'custom' but no base_url configured");
                std::process::exit(1);
            }
            match basalt::llm::OpenAiProvider::new(openai_config) {
                Ok(provider) => {
                    info!(
                        "LLM: enabled (provider=openai, model={}, base_url={})",
                        cfg.llm.model, cfg.llm.base_url
                    );
                    LlmClient::with_provider(Arc::new(provider))
                }
                Err(e) => {
                    eprintln!("error: failed to create LLM client: {e}");
                    std::process::exit(1);
                }
            }
        }
        "anthropic" => {
            let anthropic_config = basalt::llm::AnthropicConfig {
                api_key: cfg.llm.api_key.clone(),
                model: if cfg.llm.model.is_empty() {
                    "claude-sonnet-4-20250514".into()
                } else {
                    cfg.llm.model.clone()
                },
                base_url: if cfg.llm.base_url.is_empty() {
                    "https://api.anthropic.com".into()
                } else {
                    cfg.llm.base_url.clone()
                },
                api_version: cfg.llm.api_version.clone(),
                timeout_ms: cfg.llm.timeout_ms,
                default_max_tokens: cfg.llm.default_max_tokens,
                default_temperature: cfg.llm.default_temperature,
            };
            match basalt::llm::AnthropicProvider::new(anthropic_config) {
                Ok(provider) => {
                    info!(
                        "LLM: enabled (provider=anthropic, model={})",
                        cfg.llm.model
                    );
                    LlmClient::with_provider(Arc::new(provider))
                }
                Err(e) => {
                    eprintln!("error: failed to create LLM client: {e}");
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("error: unknown LLM provider: '{other}' (use 'openai', 'anthropic', or 'custom')");
            std::process::exit(1);
        }
    }
} else {
    info!("LLM: disabled (no provider configured)");
    LlmClient::disabled()
};
let llm_client = Arc::new(llm_client);
```

**Step 2: Verify it compiles**

Run: `cargo check`
Expected: compiles without errors

**Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire up LlmClient construction in main.rs"
```

---

### Task 8: Update the example TOML config file

**Objective:** Document the new LLM configuration options in the example config.

**Files:**
- Modify: `basalt.example.toml`

**Step 1: Add LLM section**

After the `[auth]` section, add:

```toml
[llm]
# LLM provider: "openai", "anthropic", or "custom".
# "custom" uses the OpenAI-compatible API format with a custom base_url.
# Empty or omitted = LLM features disabled.
# provider = "openai"

# API key for the LLM provider.
# api_key = "sk-..."

# Model to use (provider-specific defaults apply if omitted).
# OpenAI: "gpt-4o-mini" (default), "gpt-4o", "gpt-3.5-turbo"
# Anthropic: "claude-sonnet-4-20250514" (default), "claude-haiku-4-20250414"
# model = "gpt-4o-mini"

# Base URL override (for OpenAI-compatible endpoints).
# Use this with provider = "custom" to point at Ollama, vLLM, LiteLLM, etc.
# OpenAI default: https://api.openai.com/v1
# Anthropic default: https://api.anthropic.com
# base_url = "http://localhost:11434/v1"

# API version header (Anthropic only, default: "2023-06-01").
# api_version = "2023-06-01"

# Request timeout in milliseconds (default: 30000).
# timeout_ms = 30000

# Default max tokens for completions (default: 1024).
# default_max_tokens = 1024

# Default temperature for completions (default: 0.3).
# default_temperature = 0.3
```

**Step 2: Commit**

```bash
git add basalt.example.toml
git commit -m "docs: add LLM configuration section to example config"
```

---

### Task 9: Write unit tests for the LLM module

**Objective:** Test the LLM config parsing, client construction, and error handling. No live API calls in tests.

**Files:**
- Create: `src/llm/error.rs` (add tests at bottom)
- Create: `tests/llm_test.rs`

**Step 1: Add tests to `src/llm/error.rs`**

```rust
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
```

**Step 2: Create `tests/llm_test.rs`**

Integration tests for LLM config parsing and client construction:

```rust
use basalt::config::Config;
use basalt::llm::{LlmClient, LlmError, OpenAiConfig, OpenAiProvider, AnthropicConfig, AnthropicProvider};

#[test]
fn test_llm_config_default_is_disabled() {
    let config = Config::default();
    assert!(!config.llm.is_enabled());
    assert!(config.llm.provider.is_empty());
}

#[test]
fn test_llm_config_from_toml() {
    let toml = r#"
[server]
http_port = 7380

[llm]
provider = "openai"
api_key = "sk-test-key-123"
model = "gpt-4o"
timeout_ms = 60000
default_max_tokens = 2048
default_temperature = 0.5
"#;
    let dir = std::env::temp_dir().join("basalt_test_llm_config");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("basalt.toml");
    std::fs::write(&path, toml).unwrap();

    let config = Config::from_file(&path).unwrap();
    assert!(config.llm.is_enabled());
    assert_eq!(config.llm.provider, "openai");
    assert_eq!(config.llm.api_key, "sk-test-key-123");
    assert_eq!(config.llm.model, "gpt-4o");
    assert_eq!(config.llm.timeout_ms, 60000);
    assert_eq!(config.llm.default_max_tokens, 2048);
    assert!((config.llm.default_temperature - 0.5).abs() < f32::EPSILON);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_llm_config_custom_provider_with_base_url() {
    let toml = r#"
[llm]
provider = "custom"
api_key = "ollama"
base_url = "http://localhost:11434/v1"
model = "llama3"
"#;
    let dir = std::env::temp_dir().join("basalt_test_llm_custom");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("basalt.toml");
    std::fs::write(&path, toml).unwrap();

    let config = Config::from_file(&path).unwrap();
    assert!(config.llm.is_enabled());
    assert_eq!(config.llm.provider, "custom");
    assert_eq!(config.llm.base_url, "http://localhost:11434/v1");
    assert_eq!(config.llm.model, "llama3");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_openai_provider_requires_api_key() {
    let config = OpenAiConfig {
        api_key: String::new(),
        ..Default::default()
    };
    let result = OpenAiProvider::new(config);
    assert!(result.is_err());
    match result.unwrap_err() {
        LlmError::ConfigError(msg) => assert!(msg.contains("api_key")),
        _ => panic!("expected ConfigError"),
    }
}

#[test]
fn test_anthropic_provider_requires_api_key() {
    let config = AnthropicConfig {
        api_key: String::new(),
        ..Default::default()
    };
    let result = AnthropicProvider::new(config);
    assert!(result.is_err());
    match result.unwrap_err() {
        LlmError::ConfigError(msg) => assert!(msg.contains("api_key")),
        _ => panic!("expected ConfigError"),
    }
}

#[test]
fn test_llm_client_disabled() {
    let client = LlmClient::disabled();
    assert!(!client.is_enabled());
    assert_eq!(client.provider_name(), "disabled");
    assert_eq!(client.model_name(), "none");
}

#[tokio::test]
async fn test_disabled_client_returns_not_configured() {
    let client = LlmClient::disabled();
    let request = basalt::llm::LlmRequest::from_prompt("test");
    let result = client.complete(request).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        LlmError::NotConfigured => {}
        e => panic!("expected NotConfigured, got: {e}"),
    }
}

#[test]
fn test_openai_request_body() {
    let config = OpenAiConfig {
        api_key: "sk-test".into(),
        model: "gpt-4o-mini".into(),
        base_url: "https://api.openai.com/v1".into(),
        timeout_ms: 30000,
        default_max_tokens: 512,
        default_temperature: 0.1,
    };
    let provider = OpenAiProvider::new(config).unwrap();
    let request = basalt::llm::LlmRequest::from_prompt("hello");
    let body = provider.build_request_body(request);

    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(body["temperature"], 0.1);
    assert!(body["messages"].is_array());
}

#[test]
fn test_llm_request_builders() {
    let req = basalt::llm::LlmRequest::from_prompt("test prompt");
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, "user");

    let req = basalt::llm::LlmRequest::from_system_and_user("system msg", "user msg");
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role, "system");
    assert_eq!(req.messages[1].role, "user");

    let req = basalt::llm::LlmRequest::from_prompt("test")
        .with_max_tokens(100)
        .with_temperature(0.7);
    assert_eq!(req.max_tokens, Some(100));
    assert_eq!(req.temperature, Some(0.7));
}
```

Note: `build_request_body` on `OpenAiProvider` needs to be made `pub(crate)` or `pub` for the test. Add `#[cfg(test)]` visibility or just make it `pub` for now.

**Step 3: Make `build_request_body` accessible for testing**

In `src/llm/openai.rs`, change:

```rust
fn build_request_body(&self, request: LlmRequest) -> serde_json::Value {
```

to:

```rust
pub fn build_request_body(&self, request: LlmRequest) -> serde_json::Value {
```

**Step 4: Run the tests**

Run: `cargo test llm`
Expected: all tests pass

**Step 5: Run full test suite**

Run: `cargo test`
Expected: all tests pass

**Step 6: Commit**

```bash
git add src/llm/ tests/llm_test.rs
git commit -m "test: add LLM module tests (config, client, providers)"
```

---

### Task 10: Run clippy and fmt, fix any issues

**Objective:** Ensure code passes clippy and fmt checks before finalizing.

**Files:**
- Any files modified in previous tasks

**Step 1: Run cargo fmt**

Run: `cargo fmt`

**Step 2: Run cargo clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Run: `cargo clippy --all-targets --features io-uring -- -D warnings`

**Step 3: Fix any clippy warnings**

Common issues to watch for:
- `dead_code` warnings on LlmConfig fields (they'll be used by consolidation later)
- `too_many_arguments` on `apply_cli_overrides` (add `#[allow(clippy::too_many_arguments)]`)
- Make sure `#[cfg(feature = "io-uring")]` gating still works with the new `apply_cli_overrides` params

**Step 4: Run full test suite one final time**

Run: `cargo test`
Expected: all tests pass

**Step 5: Commit any fixes**

```bash
git add -A
git commit -m "fix: clippy and fmt fixes for LLM module"
```

---

### Task 11: Update docs

**Objective:** Add LLM configuration documentation.

**Files:**
- Create: `docs/llm.md`
- Modify: `docs/configuration.md` (add LLM section reference)
- Modify: `README.md` (add LLM feature mention)

**Step 1: Create `docs/llm.md`**

```markdown
# LLM Inference

Basalt can optionally integrate with LLM providers for background inference tasks like memory summarization, consolidation, and relevance scoring.

## When LLM is Needed

LLM inference is used by these features:
- **Memory consolidation** (#43): Compress multiple episodic memories into a single semantic summary.
- **Summarization triggers** (#44): Automatically summarize memories when conditions are met.
- **Relevance scoring** (#42): Classify memory importance for relevance decay.

When no LLM is configured, these features are disabled and Basalt operates as a pure KV store.

## Configuration

### TOML

```toml
[llm]
provider = "openai"           # "openai", "anthropic", or "custom"
api_key = "sk-..."            # API key
model = "gpt-4o-mini"         # Model identifier
base_url = ""                 # Override for OpenAI-compatible endpoints
timeout_ms = 30000            # Request timeout
default_max_tokens = 1024     # Max tokens for completions
default_temperature = 0.3     # Temperature for completions
```

### CLI

```bash
basalt --llm-provider openai --llm-api-key "sk-..." --llm-model "gpt-4o-mini"
```

### Environment Variables

API keys can also be provided via environment variables:

```bash
export BASALT_LLM_API_KEY="sk-..."
```

CLI flags override config file values. Config file values override defaults.

## Providers

### OpenAI (and compatible)

The `openai` provider uses the OpenAI Chat Completions API. This also works with any OpenAI-compatible endpoint:

```toml
[llm]
provider = "openai"
api_key = "sk-..."
model = "gpt-4o-mini"
```

**Ollama** (local inference):

```toml
[llm]
provider = "custom"
api_key = "ollama"
base_url = "http://localhost:11434/v1"
model = "llama3"
```

**vLLM**:

```toml
[llm]
provider = "custom"
api_key = "vllm"
base_url = "http://localhost:8000/v1"
model = "meta-llama/Meta-Llama-3-8B-Instruct"
```

### Anthropic

```toml
[llm]
provider = "anthropic"
api_key = "sk-ant-..."
model = "claude-sonnet-4-20250514"
```

## Architecture

The LLM service runs as a background tokio task. It never blocks request handling. The `LlmClient` provides:

- `complete()`: Raw chat completion
- `summarize()`: Summarize multiple memory entries
- `classify_relevance()`: Score a memory's long-term relevance (0.0-1.0)

When no LLM is configured, `LlmClient::disabled()` returns `LlmError::NotConfigured` for all calls, allowing the rest of the codebase to handle the "no LLM" case gracefully.

## Performance Considerations

- LLM calls are **async** and **non-blocking** - they run on the tokio runtime.
- Timeouts prevent hanging: default 30s, configurable via `timeout_ms`.
- Background tasks (consolidation, summarization) rate-limit their LLM calls.
- The KV engine's read path is completely unaffected by LLM operations.
```

**Step 2: Add LLM reference to `docs/configuration.md`**

Add a section linking to the LLM docs after the auth section.

**Step 3: Add LLM feature to `README.md`**

Add to the Features section:

```markdown
- **LLM integration** - Optional background LLM inference for summarization, consolidation, and relevance scoring (OpenAI, Anthropic, or custom endpoints)
```

**Step 4: Commit**

```bash
git add docs/ README.md
git commit -m "docs: add LLM inference documentation"
```

---

### Task 12: Push and verify CI

**Objective:** Push all changes and verify CI passes.

**Step 1: Push to remote**

```bash
git push
```

**Step 2: Check CI status**

```bash
gh run list --limit 3
```

**Step 3: If CI fails, fix and push**

Run clippy and tests locally with feature flags:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features io-uring -- -D warnings
cargo test
cargo test --features io-uring
```

Fix any issues and push again.
