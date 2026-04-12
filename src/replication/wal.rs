use crate::store::memory_type::MemoryType;
use std::collections::VecDeque;

/// Operation type in the WAL.
#[derive(Debug, Clone, PartialEq)]
pub enum WalOp {
    Set = 0,
    Delete = 1,
    DeletePrefix = 2,
}

impl WalOp {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(WalOp::Set),
            1 => Some(WalOp::Delete),
            2 => Some(WalOp::DeletePrefix),
            _ => None,
        }
    }

    pub fn to_u8(&self) -> u8 {
        match self {
            WalOp::Set => 0,
            WalOp::Delete => 1,
            WalOp::DeletePrefix => 2,
        }
    }
}

/// A single WAL entry.
#[derive(Debug, Clone)]
pub struct WalEntry {
    pub op: WalOp,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub mem_type: MemoryType,
    /// TTL in milliseconds. 0 means no expiry, >0 means expire in N ms.
    pub ttl_ms: u64,
    pub timestamp_ms: u64,
    /// Optional embedding vector for semantic similarity search.
    pub embedding: Option<Vec<f32>>,
}

/// Write-Ahead Log: a circular buffer of recent write operations.
///
/// Each entry has a monotonically increasing sequence number.
/// When the buffer is full, the oldest entries are evicted.
pub struct Wal {
    /// Circular buffer of entries.
    entries: std::sync::Mutex<WalInner>,
    max_size: usize,
}

struct WalInner {
    buf: VecDeque<(u64, WalEntry)>,
    /// Next sequence number to assign.
    next_seq: u64,
}

impl Wal {
    /// Create a new WAL with the given maximum number of entries.
    pub fn new(max_size: usize) -> Self {
        Wal {
            entries: std::sync::Mutex::new(WalInner {
                buf: VecDeque::with_capacity(max_size.min(1024)),
                next_seq: 1,
            }),
            max_size: usize::max(max_size, 1),
        }
    }

    /// Append a new entry, returning its sequence number.
    pub fn append(&self, entry: WalEntry) -> u64 {
        let mut inner = self.entries.lock().unwrap();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.buf.len() >= self.max_size {
            inner.buf.pop_front();
        }
        inner.buf.push_back((seq, entry));
        seq
    }

    /// Get the current length of the WAL.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().buf.len()
    }

    /// Whether the WAL is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().buf.is_empty()
    }

    /// Get the current sequence number (next to be assigned).
    /// This is also one past the last assigned sequence number.
    pub fn current_seq(&self) -> u64 {
        self.entries.lock().unwrap().next_seq
    }

    /// Get all entries with sequence number >= `from_seq`.
    /// Returns a vector of (seq, entry) pairs.
    pub fn entries_from(&self, from_seq: u64) -> Vec<(u64, WalEntry)> {
        let inner = self.entries.lock().unwrap();
        inner
            .buf
            .iter()
            .filter(|(seq, _)| *seq >= from_seq)
            .cloned()
            .collect()
    }

    /// Get the oldest sequence number in the WAL, or None if empty.
    pub fn oldest_seq(&self) -> Option<u64> {
        self.entries.lock().unwrap().buf.front().map(|(seq, _)| *seq)
    }
}

/// Serialize a WAL entry to binary format:
/// seq: u64 LE, op: u8, timestamp: u64 LE, key_len: u32 LE, key: bytes,
/// val_len: u32 LE, value: bytes, mem_type: u8, ttl_ms: u64 LE,
/// embedding_flag: u8 (0 = no embedding, 1 = embedding present),
/// if embedding present: dim: u32 LE, then dim * f32 bytes (LE)
pub fn serialize_entry(entry: &WalEntry, seq: u64) -> Vec<u8> {
    let embedding_extra = match &entry.embedding {
        Some(emb) => 1 + 4 + emb.len() * 4, // flag + dim + f32s
        None => 1, // flag only
    };
    let mut buf = Vec::with_capacity(
        8 + 1 + 8 + 4 + entry.key.len() + 4 + entry.value.len() + 1 + 8 + embedding_extra,
    );
    // Sequence number first (u64 LE)
    buf.extend_from_slice(&seq.to_le_bytes());
    // op: u8
    buf.push(entry.op.to_u8());
    // timestamp: u64 LE
    buf.extend_from_slice(&entry.timestamp_ms.to_le_bytes());
    // key_len: u32 LE
    buf.extend_from_slice(&(entry.key.len() as u32).to_le_bytes());
    // key
    buf.extend_from_slice(&entry.key);
    // val_len: u32 LE
    buf.extend_from_slice(&(entry.value.len() as u32).to_le_bytes());
    // value
    buf.extend_from_slice(&entry.value);
    // mem_type: u8
    buf.push(entry.mem_type.to_u8());
    // ttl_ms: u64 LE (0 = no expiry, >0 = expire in N ms)
    buf.extend_from_slice(&entry.ttl_ms.to_le_bytes());
    // embedding flag: 0 = none, 1 = present
    match &entry.embedding {
        Some(emb) => {
            buf.push(1u8);
            // dim: u32 LE
            buf.extend_from_slice(&(emb.len() as u32).to_le_bytes());
            // f32 bytes (LE)
            for f in emb {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
        None => {
            buf.push(0u8);
        }
    }
    buf
}

/// Deserialize a WAL entry from binary format.
/// Returns (seq, entry, bytes_consumed) or error string.
pub fn deserialize_entry(data: &[u8]) -> Result<(u64, WalEntry, usize), String> {
    let min_len = 8 + 1 + 8 + 4 + 4 + 1 + 8 + 1; // seq + op + ts + key_len + val_len + mem_type + ttl + emb_flag
    if data.len() < min_len {
        return Err(format!("data too short: {} < {min_len}", data.len()));
    }

    let mut off = 0;

    // seq: u64 LE
    let seq = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
    off += 8;

    // op: u8
    let op = WalOp::from_u8(data[off]).ok_or_else(|| format!("invalid WAL op: {}", data[off]))?;
    off += 1;

    // timestamp: u64 LE
    let timestamp_ms = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
    off += 8;

    // key_len: u32 LE
    let key_len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
    off += 4;

    if data.len() < off + key_len {
        return Err(format!(
            "data too short for key: need {key_len} bytes at offset {off}"
        ));
    }
    let key = data[off..off + key_len].to_vec();
    off += key_len;

    // val_len: u32 LE
    if data.len() < off + 4 {
        return Err(format!("data too short for val_len at offset {off}"));
    }
    let val_len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
    off += 4;

    if data.len() < off + val_len {
        return Err(format!(
            "data too short for value: need {val_len} bytes at offset {off}"
        ));
    }
    let value = data[off..off + val_len].to_vec();
    off += val_len;

    // mem_type: u8
    if data.len() < off + 1 {
        return Err(format!("data too short for mem_type at offset {off}"));
    }
    let mem_type =
        MemoryType::from_u8(data[off]).ok_or_else(|| format!("invalid mem_type: {}", data[off]))?;
    off += 1;

    // ttl_ms: u64 LE (0 = no expiry, >0 = expire in N ms)
    if data.len() < off + 8 {
        return Err(format!("data too short for ttl_ms at offset {off}"));
    }
    let ttl_ms = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
    off += 8;

    // embedding flag: u8 (0 = none, 1 = present)
    if data.len() < off + 1 {
        return Err(format!("data too short for embedding flag at offset {off}"));
    }
    let emb_flag = data[off];
    off += 1;

    let embedding = if emb_flag == 1 {
        // dim: u32 LE
        if data.len() < off + 4 {
            return Err(format!("data too short for embedding dim at offset {off}"));
        }
        let dim = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        off += 4;

        let emb_bytes = dim * 4;
        if data.len() < off + emb_bytes {
            return Err(format!(
                "data too short for embedding data: need {emb_bytes} bytes at offset {off}"
            ));
        }
        let mut emb = Vec::with_capacity(dim);
        for i in 0..dim {
            let start = off + i * 4;
            let f = f32::from_le_bytes(data[start..start + 4].try_into().unwrap());
            emb.push(f);
        }
        off += emb_bytes;
        Some(emb)
    } else {
        None
    };

    let entry = WalEntry {
        op,
        key,
        value,
        mem_type,
        ttl_ms,
        timestamp_ms,
        embedding,
    };

    Ok((seq, entry, off))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wal_append_and_read() {
        let wal = Wal::new(100);
        let entry = WalEntry {
            op: WalOp::Set,
            key: b"mykey".to_vec(),
            value: b"myval".to_vec(),
            mem_type: MemoryType::Semantic,
            ttl_ms: 5000,
            timestamp_ms: 1000,
            embedding: None,
        };
        let seq = wal.append(entry);
        assert_eq!(seq, 1);
        assert_eq!(wal.len(), 1);

        let entries = wal.entries_from(1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 1);
        assert_eq!(entries[0].1.op, WalOp::Set);
        assert_eq!(entries[0].1.key, b"mykey");
        assert_eq!(entries[0].1.value, b"myval");
    }

    #[test]
    fn test_wal_circular() {
        let wal = Wal::new(3);
        for i in 0..5 {
            wal.append(WalEntry {
                op: WalOp::Set,
                key: format!("key{i}").into_bytes(),
                value: b"v".to_vec(),
                mem_type: MemoryType::Semantic,
                ttl_ms: 0,
                timestamp_ms: i as u64,
                embedding: None,
            });
        }
        assert_eq!(wal.len(), 3);
        let entries = wal.entries_from(1);
        // Only entries 3, 4, 5 should be present
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0, 3);
    }

    #[test]
    fn test_wal_serialize_deserialize() {
        let entry = WalEntry {
            op: WalOp::Set,
            key: b"hello".to_vec(),
            value: b"world".to_vec(),
            mem_type: MemoryType::Episodic,
            ttl_ms: 30000,
            timestamp_ms: 1234567890,
            embedding: None,
        };
        let data = serialize_entry(&entry, 42);
        let (seq, decoded, consumed) = deserialize_entry(&data).unwrap();
        assert_eq!(seq, 42);
        assert_eq!(consumed, data.len());
        assert_eq!(decoded.op, WalOp::Set);
        assert_eq!(decoded.key, b"hello");
        assert_eq!(decoded.value, b"world");
        assert_eq!(decoded.mem_type, MemoryType::Episodic);
        assert_eq!(decoded.ttl_ms, 30000);
        assert_eq!(decoded.timestamp_ms, 1234567890);
    }

    #[test]
    fn test_wal_serialize_delete() {
        let entry = WalEntry {
            op: WalOp::Delete,
            key: b"mykey".to_vec(),
            value: Vec::new(),
            mem_type: MemoryType::Semantic,
            ttl_ms: 0,
            timestamp_ms: 999,
            embedding: None,
        };
        let data = serialize_entry(&entry, 10);
        let (seq, decoded, _) = deserialize_entry(&data).unwrap();
        assert_eq!(seq, 10);
        assert_eq!(decoded.op, WalOp::Delete);
        assert!(decoded.value.is_empty());
    }

    #[test]
    fn test_wal_serialize_delete_prefix() {
        let entry = WalEntry {
            op: WalOp::DeletePrefix,
            key: b"ns:".to_vec(),
            value: Vec::new(),
            mem_type: MemoryType::Semantic,
            ttl_ms: 0,
            timestamp_ms: 100,
            embedding: None,
        };
        let data = serialize_entry(&entry, 7);
        let (seq, decoded, _) = deserialize_entry(&data).unwrap();
        assert_eq!(seq, 7);
        assert_eq!(decoded.op, WalOp::DeletePrefix);
        assert_eq!(decoded.key, b"ns:");
    }

    /// Round-trip test: ttl_ms = 0 means no expiry.
    #[test]
    fn test_wal_ttl_zero_means_no_expiry() {
        let entry = WalEntry {
            op: WalOp::Set,
            key: b"noexpiry".to_vec(),
            value: b"val".to_vec(),
            mem_type: MemoryType::Semantic,
            ttl_ms: 0, // no expiry
            timestamp_ms: 1000,
            embedding: None,
        };
        let data = serialize_entry(&entry, 1);
        let (_, decoded, _) = deserialize_entry(&data).unwrap();
        assert_eq!(decoded.ttl_ms, 0);
        // On the replica side, ttl_ms == 0 should be treated as None (no expiry)
        let ttl: Option<u64> = if decoded.ttl_ms == 0 { None } else { Some(decoded.ttl_ms) };
        assert_eq!(ttl, None, "ttl_ms=0 must round-trip as None (no expiry)");
    }

    /// Round-trip test: ttl_ms > 0 means actual TTL in ms.
    #[test]
    fn test_wal_ttl_positive_means_actual_ttl() {
        let entry = WalEntry {
            op: WalOp::Set,
            key: b"withttl".to_vec(),
            value: b"val".to_vec(),
            mem_type: MemoryType::Episodic,
            ttl_ms: 5000, // 5 second TTL
            timestamp_ms: 2000,
            embedding: None,
        };
        let data = serialize_entry(&entry, 2);
        let (_, decoded, _) = deserialize_entry(&data).unwrap();
        assert_eq!(decoded.ttl_ms, 5000);
        let ttl: Option<u64> = if decoded.ttl_ms == 0 { None } else { Some(decoded.ttl_ms) };
        assert_eq!(ttl, Some(5000), "ttl_ms=5000 must round-trip as Some(5000)");
    }

    /// Round-trip test: large TTL value stays exact through serialization.
    #[test]
    fn test_wal_ttl_large_value() {
        let entry = WalEntry {
            op: WalOp::Set,
            key: b"largettl".to_vec(),
            value: b"val".to_vec(),
            mem_type: MemoryType::Procedural,
            ttl_ms: u64::MAX / 2, // very large but not max
            timestamp_ms: 3000,
            embedding: None,
        };
        let data = serialize_entry(&entry, 3);
        let (_, decoded, _) = deserialize_entry(&data).unwrap();
        assert_eq!(decoded.ttl_ms, u64::MAX / 2);
        let ttl: Option<u64> = if decoded.ttl_ms == 0 { None } else { Some(decoded.ttl_ms) };
        assert_eq!(ttl, Some(u64::MAX / 2));
    }

    /// Test that record_set with None TTL stores ttl_ms=0 in WalEntry.
    #[test]
    fn test_record_set_none_ttl_stores_zero() {
        use crate::replication::ReplicationState;
        use crate::store::engine::KvEngine;
        use std::sync::Arc;
        let engine = Arc::new(KvEngine::new(4));
        let repl_state = ReplicationState::new_primary(engine, 100);
        repl_state.record_set(b"key1", b"val1", MemoryType::Semantic, None);
        let entries = repl_state.wal().entries_from(1);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].1.ttl_ms, 0,
            "record_set with None TTL must store ttl_ms=0"
        );
    }

    /// Test that record_set with Some(TTL) stores that TTL in WalEntry.
    #[test]
    fn test_record_set_some_ttl_stores_value() {
        use crate::replication::ReplicationState;
        use crate::store::engine::KvEngine;
        use std::sync::Arc;
        let engine = Arc::new(KvEngine::new(4));
        let repl_state = ReplicationState::new_primary(engine, 100);
        repl_state.record_set(b"key2", b"val2", MemoryType::Episodic, Some(10000));
        let entries = repl_state.wal().entries_from(1);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].1.ttl_ms, 10000,
            "record_set with Some(10000) must store ttl_ms=10000"
        );
    }
}
