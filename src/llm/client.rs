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
        let score: f32 = response.content.trim().parse().unwrap_or(0.5);
        Ok(score.clamp(0.0, 1.0))
    }
}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.provider {
            Some(p) => write!(f, "LlmClient({}/{})", p.name(), p.model()),
            None => write!(f, "LlmClient(disabled)"),
        }
    }
}

// Safety: LlmClient only holds Arc<dyn LlmProvider> which is Send + Sync.
// The LlmClient itself is safe to send across threads and share references.
unsafe impl Send for LlmClient {}
unsafe impl Sync for LlmClient {}
