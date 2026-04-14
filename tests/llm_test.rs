use basalt::config::Config;
use basalt::llm::{
    AnthropicConfig, AnthropicProvider, LlmClient, LlmError, LlmRequest, OpenAiConfig,
    OpenAiProvider,
};

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
        e => panic!("expected ConfigError, got: {e}"),
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
        e => panic!("expected ConfigError, got: {e}"),
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
    let request = LlmRequest::from_prompt("test");
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
    let request = LlmRequest::from_prompt("hello");
    let body = provider.build_request_body(request);

    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["max_tokens"], 512);
    assert!(body["temperature"].as_f64().unwrap() - 0.1 < 0.001);
    assert!(body["messages"].is_array());
}

#[test]
fn test_llm_request_builders() {
    let req = LlmRequest::from_prompt("test prompt");
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, "user");

    let req = LlmRequest::from_system_and_user("system msg", "user msg");
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role, "system");
    assert_eq!(req.messages[1].role, "user");

    let req = LlmRequest::from_prompt("test")
        .with_max_tokens(100)
        .with_temperature(0.7);
    assert_eq!(req.max_tokens, Some(100));
    assert_eq!(req.temperature, Some(0.7));
}

#[test]
fn test_llm_config_anthropic_from_toml() {
    let toml = r#"
[llm]
provider = "anthropic"
api_key = "sk-ant-test"
model = "claude-haiku-4-20250414"
api_version = "2024-01-01"
"#;
    let dir = std::env::temp_dir().join("basalt_test_llm_anthropic");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("basalt.toml");
    std::fs::write(&path, toml).unwrap();

    let config = Config::from_file(&path).unwrap();
    assert!(config.llm.is_enabled());
    assert_eq!(config.llm.provider, "anthropic");
    assert_eq!(config.llm.api_key, "sk-ant-test");
    assert_eq!(config.llm.model, "claude-haiku-4-20250414");
    assert_eq!(config.llm.api_version, "2024-01-01");

    std::fs::remove_dir_all(&dir).ok();
}
