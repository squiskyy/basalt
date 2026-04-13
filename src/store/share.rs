use serde::{Deserialize, Serialize};

/// Permission level for a sharing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SharePermission {
    Read,
    ReadWrite,
}

impl std::fmt::Display for SharePermission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SharePermission::Read => write!(f, "read"),
            SharePermission::ReadWrite => write!(f, "read-write"),
        }
    }
}

/// A sharing policy granting access from one namespace to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharePolicy {
    /// The namespace that owns the data being shared.
    pub source_namespace: String,
    /// The namespace that receives access.
    pub target_namespace: String,
    /// Permission level.
    pub permission: SharePermission,
    /// Optional key prefix to scope the share (e.g., "shared/").
    /// If None, all keys in the source namespace are accessible.
    pub key_prefix: Option<String>,
    /// Timestamp when the policy was created (epoch millis).
    pub created_at: u64,
}

/// Thread-safe store for sharing policies.
pub struct ShareStore {
    /// Key: source_namespace, Value: list of policies from that namespace.
    policies: papaya::HashMap<String, Vec<SharePolicy>>,
}

impl ShareStore {
    pub fn new() -> Self {
        Self {
            policies: papaya::HashMap::new(),
        }
    }

    /// Grant a sharing policy. Replaces any existing policy for the same
    /// (source, target, prefix) tuple.
    pub fn grant(&self, policy: SharePolicy) {
        let policies = self.policies.pin();
        let key = policy.source_namespace.clone();
        let mut existing = policies.get(&key).cloned().unwrap_or_default();

        // Remove any existing policy with the same (source, target, prefix)
        existing.retain(|p| {
            p.target_namespace != policy.target_namespace || p.key_prefix != policy.key_prefix
        });

        existing.push(policy);
        policies.insert(key, existing);
    }

    /// Revoke a specific sharing policy.
    /// Returns true if a policy was removed.
    pub fn revoke(
        &self,
        source_namespace: &str,
        target_namespace: &str,
        key_prefix: Option<&str>,
    ) -> bool {
        let policies = self.policies.pin();
        if let Some(mut existing) = policies.get(source_namespace).cloned() {
            let before = existing.len();
            existing.retain(|p| {
                p.target_namespace != target_namespace || p.key_prefix.as_deref() != key_prefix
            });
            let removed = before != existing.len();
            if existing.is_empty() {
                policies.remove(source_namespace);
            } else {
                policies.insert(source_namespace.to_string(), existing);
            }
            removed
        } else {
            false
        }
    }

    /// Remove all sharing policies from a namespace.
    pub fn revoke_all(&self, source_namespace: &str) {
        self.policies.pin().remove(source_namespace);
    }

    /// List all policies granted by a namespace.
    pub fn policies_for(&self, source_namespace: &str) -> Vec<SharePolicy> {
        self.policies
            .pin()
            .get(source_namespace)
            .cloned()
            .unwrap_or_default()
    }

    /// List all policies granted to a namespace.
    pub fn shared_with(&self, target_namespace: &str) -> Vec<SharePolicy> {
        let mut result = Vec::new();
        for policies in self.policies.pin().values() {
            for p in policies {
                if p.target_namespace == target_namespace {
                    result.push(p.clone());
                }
            }
        }
        result
    }

    /// Check if target_namespace has access to source_namespace for a given key and operation.
    /// `write` is true for write operations (POST, DELETE), false for read (GET).
    /// An empty `key` means a listing operation (namespace-level access check).
    pub fn check_access(
        &self,
        target_namespace: &str,
        source_namespace: &str,
        key: &str,
        write: bool,
    ) -> bool {
        if let Some(policies) = self.policies.pin().get(source_namespace) {
            for p in policies {
                if p.target_namespace == target_namespace {
                    // For listing operations (empty key), allow if any policy matches
                    // regardless of prefix - the listing handler will filter
                    if key.is_empty() {
                        match p.permission {
                            SharePermission::Read => {
                                if !write {
                                    return true;
                                }
                            }
                            SharePermission::ReadWrite => {
                                return true;
                            }
                        }
                        continue;
                    }
                    // Check key prefix match for specific key access
                    if let Some(ref prefix) = p.key_prefix
                        && !key.starts_with(prefix)
                    {
                        continue;
                    }
                    match p.permission {
                        SharePermission::Read => {
                            if !write {
                                return true;
                            }
                        }
                        SharePermission::ReadWrite => {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

impl Default for ShareStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn test_grant_and_check_read_access() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: None,
            created_at: now_ms(),
        });

        assert!(store.check_access("agent-b", "agent-a", "any-key", false));
        assert!(!store.check_access("agent-b", "agent-a", "any-key", true));
        assert!(!store.check_access("agent-c", "agent-a", "any-key", false));
    }

    #[test]
    fn test_grant_and_check_readwrite_access() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::ReadWrite,
            key_prefix: None,
            created_at: now_ms(),
        });

        assert!(store.check_access("agent-b", "agent-a", "any-key", false));
        assert!(store.check_access("agent-b", "agent-a", "any-key", true));
    }

    #[test]
    fn test_key_prefix_scoping() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: Some("shared/".into()),
            created_at: now_ms(),
        });

        assert!(store.check_access("agent-b", "agent-a", "shared/data", false));
        assert!(!store.check_access("agent-b", "agent-a", "private/data", false));
    }

    #[test]
    fn test_revoke() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: None,
            created_at: now_ms(),
        });

        assert!(store.check_access("agent-b", "agent-a", "key", false));
        assert!(store.revoke("agent-a", "agent-b", None));
        assert!(!store.check_access("agent-b", "agent-a", "key", false));
    }

    #[test]
    fn test_revoke_prefix_specific() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: Some("shared/".into()),
            created_at: now_ms(),
        });
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: Some("public/".into()),
            created_at: now_ms(),
        });

        // Revoke only the shared/ prefix
        assert!(store.revoke("agent-a", "agent-b", Some("shared/")));

        assert!(!store.check_access("agent-b", "agent-a", "shared/data", false));
        assert!(store.check_access("agent-b", "agent-a", "public/data", false));
    }

    #[test]
    fn test_grant_replaces_existing() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: None,
            created_at: now_ms(),
        });

        // Upgrade to read-write
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::ReadWrite,
            key_prefix: None,
            created_at: now_ms(),
        });

        assert!(store.check_access("agent-b", "agent-a", "key", true));
        // Should only have one policy
        assert_eq!(store.policies_for("agent-a").len(), 1);
    }

    #[test]
    fn test_policies_for_and_shared_with() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: None,
            created_at: now_ms(),
        });
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-c".into(),
            permission: SharePermission::ReadWrite,
            key_prefix: None,
            created_at: now_ms(),
        });

        let granted = store.policies_for("agent-a");
        assert_eq!(granted.len(), 2);

        let shared_with_b = store.shared_with("agent-b");
        assert_eq!(shared_with_b.len(), 1);
        assert_eq!(shared_with_b[0].source_namespace, "agent-a");
    }

    #[test]
    fn test_empty_key_list_access() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: Some("shared/".into()),
            created_at: now_ms(),
        });

        // Empty key (listing) should allow read access even with prefix
        assert!(store.check_access("agent-b", "agent-a", "", false));
        // But not write access
        assert!(!store.check_access("agent-b", "agent-a", "", true));
    }

    #[test]
    fn test_revoke_all() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: None,
            created_at: now_ms(),
        });
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-c".into(),
            permission: SharePermission::ReadWrite,
            key_prefix: None,
            created_at: now_ms(),
        });

        store.revoke_all("agent-a");
        assert!(!store.check_access("agent-b", "agent-a", "key", false));
        assert!(!store.check_access("agent-c", "agent-a", "key", true));
        assert_eq!(store.policies_for("agent-a").len(), 0);
    }

    #[test]
    fn test_revoke_nonexistent() {
        let store = ShareStore::new();
        assert!(!store.revoke("agent-a", "agent-b", None));
    }

    #[test]
    fn test_no_self_share_check() {
        // check_access doesn't prevent self-access, that's handled at the API layer
        let store = ShareStore::new();
        assert!(!store.check_access("agent-a", "agent-a", "key", false));
    }

    #[test]
    fn test_multiple_targets() {
        let store = ShareStore::new();
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-b".into(),
            permission: SharePermission::Read,
            key_prefix: None,
            created_at: now_ms(),
        });
        store.grant(SharePolicy {
            source_namespace: "agent-a".into(),
            target_namespace: "agent-c".into(),
            permission: SharePermission::ReadWrite,
            key_prefix: None,
            created_at: now_ms(),
        });

        assert!(store.check_access("agent-b", "agent-a", "key", false));
        assert!(!store.check_access("agent-b", "agent-a", "key", true));
        assert!(store.check_access("agent-c", "agent-a", "key", false));
        assert!(store.check_access("agent-c", "agent-a", "key", true));
    }
}
