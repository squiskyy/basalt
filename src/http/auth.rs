/// Bearer token authentication for Basalt.
///
/// Tokens are configured at startup via CLI args or a config file.
/// Each token can be scoped to specific namespaces, or `["*"]` for full access.
///
/// HTTP: `Authorization: Bearer *** header on all `/store/*` endpoints.
/// RESP: `AUTH <token>` command (Redis-compatible).
///
/// Only `/store/*` paths require auth; all other paths are allowed through.
use std::sync::Arc;

use axum::Json;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use papaya::HashMap as ConcurrentHashMap;

use super::models::SimpleResponse;

/// A single auth token with its namespace permissions.
#[derive(Debug, Clone)]
pub struct Token {
    /// The token string (e.g. "bsk-abc123")
    pub value: String,
    /// Namespaces this token can access. `["*"]` means all.
    pub namespaces: Vec<String>,
}

/// The auth store - holds all tokens in a concurrent HashMap for fast lookup.
#[derive(Debug, Clone, Default)]
pub struct AuthStore {
    /// token_value -> Token
    tokens: Arc<ConcurrentHashMap<String, Token>>,
}

impl AuthStore {
    /// Create an empty auth store (no tokens = no auth required).
    pub fn new() -> Self {
        Self {
            tokens: Arc::new(ConcurrentHashMap::new()),
        }
    }

    /// Create an auth store from a list of (token, namespaces) pairs.
    pub fn from_list(list: Vec<(String, Vec<String>)>) -> Self {
        let store = Self::new();
        for (value, namespaces) in list {
            store.add_token(value, namespaces);
        }
        store
    }

    /// Add a token.
    pub fn add_token(&self, value: String, namespaces: Vec<String>) {
        let token = Token {
            value: value.clone(),
            namespaces,
        };
        self.tokens.pin().insert(value, token);
    }

    /// Remove a token. Returns true if it existed.
    pub fn remove_token(&self, value: &str) -> bool {
        self.tokens.pin().remove(value).is_some()
    }

    /// Check if a token exists and is authorized for a given namespace.
    /// If no tokens are configured (auth disabled), always returns true.
    pub fn is_authorized(&self, token_value: &str, namespace: &str) -> bool {
        let pinned = self.tokens.pin();
        if pinned.is_empty() {
            // No tokens configured = auth disabled, allow everything
            return true;
        }
        pinned
            .get(token_value)
            .map(|token| {
                token
                    .namespaces
                    .iter()
                    .any(|ns| ns == "*" || ns == namespace)
            })
            .unwrap_or(false)
    }

    /// Check if a token exists in the store (valid token, regardless of namespace).
    /// Returns true if auth is disabled (no tokens configured) or the token exists.
    pub fn token_exists(&self, token_value: &str) -> bool {
        let pinned = self.tokens.pin();
        if pinned.is_empty() {
            return true;
        }
        pinned.get(token_value).is_some()
    }

    /// Check if auth is enabled (any tokens configured).
    pub fn is_enabled(&self) -> bool {
        !self.tokens.pin().is_empty()
    }

    /// List all tokens (for AUTH INFO command).
    pub fn list_tokens(&self) -> Vec<(String, Vec<String>)> {
        self.tokens
            .pin()
            .iter()
            .map(|(_, token)| (token.value.clone(), token.namespaces.clone()))
            .collect()
    }
}

/// Extract bearer token from Authorization header.
fn extract_bearer(request: &Request) -> Option<&str> {
    request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Axum middleware for bearer token auth on /store/* routes.
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<crate::http::server::AppState>,
    request: Request,
    next: Next,
) -> axum::response::Response {
    let auth_store = &state.auth;

    // If auth is not configured, allow everything
    if !auth_store.is_enabled() {
        return next.run(request).await;
    }

    let path = request.uri().path();

    // Only /store/* paths require auth; all other paths are allowed through
    if !path.starts_with("/store/") {
        return next.run(request).await;
    }

    // Extract namespace from the path: /store/{namespace}/...
    let namespace = extract_namespace_from_path(path);

    // Extract bearer token
    let token = match extract_bearer(&request) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(SimpleResponse {
                    ok: false,
                    deleted: None,
                }),
            )
                .into_response();
        }
    };

    // Check authorization
    if let Some(ns) = namespace {
        if auth_store.is_authorized(token, ns) {
            return next.run(request).await;
        }
    } else {
        // No namespace in path (e.g. bare /store/), check with empty namespace
        if auth_store.is_authorized(token, "") {
            return next.run(request).await;
        }
    }

    (
        StatusCode::FORBIDDEN,
        Json(SimpleResponse {
            ok: false,
            deleted: None,
        }),
    )
        .into_response()
}

/// Extract the namespace from a URL path like /store/{namespace}/...
fn extract_namespace_from_path(path: &str) -> Option<&str> {
    let stripped = path.strip_prefix("/store/")?;
    let end = stripped.find('/').unwrap_or(stripped.len());
    Some(&stripped[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_disabled() {
        let store = AuthStore::new();
        assert!(!store.is_enabled());
        // No tokens = everything allowed
        assert!(store.is_authorized("any-token", "any-ns"));
    }

    #[test]
    fn test_auth_wildcard() {
        let store = AuthStore::from_list(vec![("bsk-admin".to_string(), vec!["*".to_string()])]);
        assert!(store.is_enabled());
        assert!(store.is_authorized("bsk-admin", "agent-1"));
        assert!(store.is_authorized("bsk-admin", "anything"));
        assert!(!store.is_authorized("bsk-wrong", "agent-1"));
    }

    #[test]
    fn test_auth_scoped() {
        let store = AuthStore::from_list(vec![
            ("bsk-agent1".to_string(), vec!["agent-1".to_string()]),
            (
                "bsk-agent2".to_string(),
                vec!["agent-2".to_string(), "shared".to_string()],
            ),
        ]);
        assert!(store.is_authorized("bsk-agent1", "agent-1"));
        assert!(!store.is_authorized("bsk-agent1", "agent-2"));
        assert!(!store.is_authorized("bsk-agent1", "shared"));
        assert!(store.is_authorized("bsk-agent2", "agent-2"));
        assert!(store.is_authorized("bsk-agent2", "shared"));
        assert!(!store.is_authorized("bsk-agent2", "agent-1"));
    }

    #[test]
    fn test_add_remove_token() {
        let store =
            AuthStore::from_list(vec![("bsk-other".to_string(), vec!["other".to_string()])]);
        store.add_token("bsk-new".to_string(), vec!["ns-1".to_string()]);
        assert!(store.is_authorized("bsk-new", "ns-1"));
        assert!(store.remove_token("bsk-new"));
        assert!(!store.is_authorized("bsk-new", "ns-1"));
        // Other token still exists so auth is still enabled
        assert!(store.is_enabled());
    }

    #[test]
    fn test_extract_namespace() {
        assert_eq!(
            extract_namespace_from_path("/store/agent-1/mem:1"),
            Some("agent-1")
        );
        assert_eq!(
            extract_namespace_from_path("/store/agent-1"),
            Some("agent-1")
        );
        assert_eq!(
            extract_namespace_from_path("/store/agent-1/batch"),
            Some("agent-1")
        );
        assert_eq!(extract_namespace_from_path("/health"), None);
        assert_eq!(extract_namespace_from_path("/info"), None);
    }
}
