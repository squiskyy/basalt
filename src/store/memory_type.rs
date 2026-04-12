use serde::{Deserialize, Serialize};
use std::fmt;

/// Classification of memory entries, inspired by human memory systems.
///
/// - **Episodic**: Short-lived, event-based memories (auto-expire).
/// - **Semantic**: Long-lived factual knowledge (persistent until deleted).
/// - **Procedural**: Long-lived skill/behavior data (persistent until deleted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    Episodic,
    Semantic,
    Procedural,
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryType::Episodic => write!(f, "episodic"),
            MemoryType::Semantic => write!(f, "semantic"),
            MemoryType::Procedural => write!(f, "procedural"),
        }
    }
}

impl MemoryType {
    /// Returns the default TTL in milliseconds for this memory type.
    ///
    /// - Episodic: 3_600_000 ms (1 hour) -- short-lived event memory
    /// - Semantic: no expiry (None) -- persistent factual knowledge
    /// - Procedural: no expiry (None) -- persistent skill data
    pub fn default_ttl_ms(&self) -> Option<u64> {
        match self {
            MemoryType::Episodic => Some(3_600_000),
            MemoryType::Semantic => None,
            MemoryType::Procedural => None,
        }
    }

    /// Convert this MemoryType to a u8 for binary serialization.
    ///
    /// Episodic=0, Semantic=1, Procedural=2.
    pub fn to_u8(self) -> u8 {
        match self {
            MemoryType::Episodic => 0,
            MemoryType::Semantic => 1,
            MemoryType::Procedural => 2,
        }
    }

    /// Convert a u8 to a MemoryType for binary deserialization.
    ///
    /// Returns None if the value does not map to a valid MemoryType.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(MemoryType::Episodic),
            1 => Some(MemoryType::Semantic),
            2 => Some(MemoryType::Procedural),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", MemoryType::Episodic), "episodic");
        assert_eq!(format!("{}", MemoryType::Semantic), "semantic");
        assert_eq!(format!("{}", MemoryType::Procedural), "procedural");
    }

    #[test]
    fn test_serde_roundtrip() {
        for mt in [MemoryType::Episodic, MemoryType::Semantic, MemoryType::Procedural] {
            let json = serde_json::to_string(&mt).unwrap();
            let back: MemoryType = serde_json::from_str(&json).unwrap();
            assert_eq!(mt, back);
        }
    }

    #[test]
    fn test_default_ttl() {
        assert_eq!(MemoryType::Episodic.default_ttl_ms(), Some(3_600_000));
        assert_eq!(MemoryType::Semantic.default_ttl_ms(), None);
        assert_eq!(MemoryType::Procedural.default_ttl_ms(), None);
    }

    #[test]
    fn test_to_u8_from_u8_roundtrip() {
        for (mt, expected_byte) in [
            (MemoryType::Episodic, 0u8),
            (MemoryType::Semantic, 1u8),
            (MemoryType::Procedural, 2u8),
        ] {
            assert_eq!(mt.to_u8(), expected_byte);
            assert_eq!(MemoryType::from_u8(expected_byte), Some(mt));
        }
        assert_eq!(MemoryType::from_u8(3), None);
        assert_eq!(MemoryType::from_u8(255), None);
    }
}
