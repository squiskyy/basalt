# LLM Inference

Basalt can optionally integrate with LLM providers for background inference tasks like memory summarization, consolidation, and relevance scoring.

## When LLM is Needed

LLM inference is used by these features:
- **Memory consolidation** (#43): Compress multiple episodic memories into a single semantic summary.
- **Summarization triggers** (#44): Automatically summarize memories when conditions are met.
- **Relevance scoring** (#42): Classify memory importance for relevance decay.

When no LLM is configured, these features are disabled and Basalt operates as a pure KV store. The `LlmClient::disabled()` state returns `LlmError::NotConfigured` for all calls, allowing graceful handling without feature-gating.

## Configuration

### TOML Config File

```toml
[llm]
# Required: provider name. "openai", "anthropic", or "custom".
# Empty or omitted = LLM disabled.
provider = "openai"

# Required: API key for authentication.
api_key = "sk-..."

# Optional: model identifier (provider defaults apply if omitted).
# OpenAI default: gpt-4o-mini
# Anthropic default: claude-sonnet-4-20250514
model = "gpt-4o"

# Optional: base URL override for OpenAI-compatible endpoints.
# Use with provider = "custom" for Ollama, vLLM, LiteLLM, etc.
# base_url = "http://localhost:11434/v1"

# Optional: API version header (Anthropic only, default: "2023-06-01").
# api_version = "2023-06-01"

# Optional: request timeout in milliseconds (default: 30000).
# timeout_ms = 30000

# Optional: default max tokens for completions (default: 1024).
# default_max_tokens = 1024

# Optional: default temperature for completions (default: 0.3).
# default_temperature = 0.3
```

### CLI Flags

```bash
basalt --llm-provider openai --llm-api-key "sk-..." --llm-model "gpt-4o"
basalt --llm-provider custom --llm-api-key "ollama" --llm-base-url "http://localhost:11434/v1" --llm-model "llama3"
basalt --llm-provider anthropic --llm-api-key "sk-ant-..." --llm-model "claude-sonnet-4-20250514"
```

CLI flags override config file values.

## Providers

### OpenAI (and compatible)

The `openai` provider uses the OpenAI Chat Completions API (`/v1/chat/completions`). This also works with any OpenAI-compatible endpoint.

**Direct OpenAI:**

```toml
[llm]
provider = "openai"
api_key = "sk-..."
model = "gpt-4o-mini"
```

**Ollama (local inference):**

```toml
[llm]
provider = "custom"
api_key = "ollama"
base_url = "http://localhost:11434/v1"
model = "llama3"
```

**vLLM:**

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

Uses the Anthropic Messages API (`/v1/messages`). System messages are extracted from the request and sent in the top-level `system` field per Anthropic's API spec.

## Architecture

### Module Structure

```
src/llm/
  mod.rs          - Re-exports
  error.rs        - LlmError enum, LlmResult type alias
  provider.rs     - LlmProvider trait, LlmRequest, LlmResponse, LlmMessage
  openai.rs       - OpenAiProvider (OpenAI Chat Completions API)
  anthropic.rs    - AnthropicProvider (Anthropic Messages API)
  client.rs       - LlmClient facade (disabled mode, convenience methods)
```

### LlmClient API

The `LlmClient` is the main interface used by the rest of Basalt:

- `is_enabled()` - Check if LLM is configured
- `complete(request)` - Raw chat completion
- `summarize(entries, context)` - Summarize multiple memory entries
- `classify_relevance(key, value, type)` - Score memory importance (0.0-1.0)

When disabled, all methods return `LlmError::NotConfigured`.

### Integration Points

The `Arc<LlmClient>` is constructed in `main.rs` and will be passed to:
- Background consolidation tasks (issue #43)
- Summarization trigger service (issue #44)
- Relevance decay scoring (issue #42)

Currently the client is created and logged at startup but not yet wired into any features, as those features are still in development.

## Performance Considerations

- LLM calls are **async** and **non-blocking** - they run on the tokio runtime.
- Timeouts prevent hanging: default 30s, configurable via `timeout_ms`.
- Background tasks (consolidation, summarization) will rate-limit their LLM calls.
- The KV engine's read path is completely unaffected by LLM operations.
- The `LlmClient::disabled()` check is a simple `Option::is_some()` - zero overhead when not configured.
