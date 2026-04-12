/// Persistence module for Basalt KV store.
///
/// Binary snapshot format (version 3):
///   - Magic bytes: b"BASALT\x00" (8 bytes)
///   - Version: u8 (1 byte, currently 3)
///   - Entry count: u64 (8 bytes, little-endian)
///   - For each entry (version 3):
///     - key_len: u32 (4 bytes, LE)
///     - key: [u8; key_len]
///     - flags: u8 (1 byte, bit 0 = LZ4 compressed, bits 1-7 reserved)
///     - val_len: u32 (4 bytes, LE, length of stored value bytes)
///     - value: [u8; val_len] (compressed if flag bit 0 set)
///     - memory_type: u8 (0=episodic, 1=semantic, 2=procedural)
///     - embedding_flag: u8 (0 = no embedding, 1 = has embedding)
///     - expires_at: u64 (8 bytes, LE, 0 = no expiry)
///     - If embedding_flag == 1:
///       - dim: u32 (4 bytes, LE, number of f32 dimensions)
///       - embedding: [u8; dim * 4] (each f32 as 4 LE bytes)
///
/// Version 2 format (backward compatible on read):
///   Same as v3 but without the embedding_flag byte and embedding data.
///   After memory_type, expires_at follows immediately.
///
/// Version 1 format (backward compatible on read):
///   Same as v2 but without the flags byte per entry.
///   When reading version 1, entries are read without flags (no compression).
///
/// Values larger than snapshot_compression_threshold are LZ4-compressed
/// before writing and decompressed on read. Threshold default: 1024 bytes.
/// Setting threshold to 0 disables compression entirely.
///
/// Snapshots are written atomically: write to a temp file, then rename.
/// On startup, load the latest snapshot from the db_path directory.

use crate::store::memory_type::MemoryType;
use crate::time::now_ms;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

/// Magic bytes for snapshot files.
const MAGIC: &[u8; 7] = b"BASALT\x00";

/// Current snapshot format version.
const VERSION: u8 = 3;

/// Version 3 (current: supports embedding vectors).
const VERSION_3: u8 = 3;

/// Version 2 (no embedding vectors, but has flags byte).
const VERSION_2: u8 = 2;

/// Version 1 (legacy, no compression/flags byte).
const VERSION_1: u8 = 1;

/// Flag bit: entry value is LZ4-compressed.
const FLAG_COMPRESSED: u8 = 0b0000_0001;

/// Default compression threshold in bytes.
const DEFAULT_COMPRESSION_THRESHOLD: usize = 1024;

/// File name pattern for snapshots: `snapshot-<timestamp>.bin`
const SNAPSHOT_PREFIX: &str = "snapshot-";
const SNAPSHOT_EXT: &str = ".bin";

/// Convert MemoryType to u8 for serialization.

/// A single entry to be persisted, with its key.
#[derive(Debug)]
pub struct SnapshotEntry {
    pub key: String,
    pub value: Vec<u8>,
    pub memory_type: MemoryType,
    pub expires_at: Option<u64>,
    /// Optional embedding vector for semantic similarity search.
    pub embedding: Option<Vec<f32>>,
}

/// Write a snapshot to disk atomically.
///
/// Writes to a temp file first, then renames to the final path.
/// Returns the path of the written snapshot file.
///
/// Values larger than `compression_threshold` bytes are LZ4-compressed
/// before writing. Set threshold to 0 to disable compression.
pub fn write_snapshot(
    db_path: &Path,
    entries: &[SnapshotEntry],
    timestamp_ms: u64,
    compression_threshold: usize,
) -> Result<PathBuf, String> {
    // Ensure db_path directory exists
    fs::create_dir_all(db_path)
        .map_err(|e| format!("failed to create db_path {}: {e}", db_path.display()))?;

    let filename = format!("{SNAPSHOT_PREFIX}{timestamp_ms}{SNAPSHOT_EXT}");
    let final_path = db_path.join(&filename);
    let tmp_path = db_path.join(format!("{filename}.tmp"));

    // Write to temp file
    let mut f = fs::File::create(&tmp_path)
        .map_err(|e| format!("failed to create temp snapshot file {}: {e}", tmp_path.display()))?;

    // Header
    f.write_all(MAGIC)
        .map_err(|e| format!("write magic: {e}"))?;
    f.write_all(&[VERSION])
        .map_err(|e| format!("write version: {e}"))?;
    f.write_all(&(entries.len() as u64).to_le_bytes())
        .map_err(|e| format!("write entry count: {e}"))?;

    // Entries
    for entry in entries {
        let key_bytes = entry.key.as_bytes();
        f.write_all(&(key_bytes.len() as u32).to_le_bytes())
            .map_err(|e| format!("write key_len: {e}"))?;
        f.write_all(key_bytes)
            .map_err(|e| format!("write key: {e}"))?;

        // Compress if threshold > 0 and value is large enough
        let (flags, stored_value): (u8, Vec<u8>) = if compression_threshold > 0
            && entry.value.len() > compression_threshold
        {
            let compressed = lz4_flex::compress_prepend_size(&entry.value);
            // Only use compression if it actually reduces size
            if compressed.len() < entry.value.len() {
                (FLAG_COMPRESSED, compressed)
            } else {
                (0u8, entry.value.clone())
            }
        } else {
            (0u8, entry.value.clone())
        };

        f.write_all(&[flags])
            .map_err(|e| format!("write flags: {e}"))?;
        f.write_all(&(stored_value.len() as u32).to_le_bytes())
            .map_err(|e| format!("write val_len: {e}"))?;
        f.write_all(&stored_value)
            .map_err(|e| format!("write value: {e}"))?;
        f.write_all(&[entry.memory_type.to_u8()])
            .map_err(|e| format!("write memory_type: {e}"))?;

        // Embedding flag: 0 = no embedding, 1 = has embedding
        let embedding_flag: u8 = if entry.embedding.is_some() { 1 } else { 0 };
        f.write_all(&[embedding_flag])
            .map_err(|e| format!("write embedding_flag: {e}"))?;

        f.write_all(
            &entry
                .expires_at
                .unwrap_or(0)
                .to_le_bytes(),
        )
        .map_err(|e| format!("write expires_at: {e}"))?;

        // If embedding is present, write dim (u32 LE) then dim*f32 bytes
        if let Some(ref emb) = entry.embedding {
            let dim = emb.len() as u32;
            f.write_all(&dim.to_le_bytes())
                .map_err(|e| format!("write embedding dim: {e}"))?;
            for &val in emb {
                f.write_all(&val.to_le_bytes())
                    .map_err(|e| format!("write embedding value: {e}"))?;
            }
        }
    }

    f.flush()
        .map_err(|e| format!("flush snapshot: {e}"))?;
    f.sync_all()
        .map_err(|e| format!("fsync snapshot: {e}"))?;
    drop(f);

    // Atomic rename
    fs::rename(&tmp_path, &final_path)
        .map_err(|e| format!("rename snapshot: {e}"))?;

    debug!(
        "snapshot written: {} ({} entries)",
        final_path.display(),
        entries.len()
    );

    Ok(final_path)
}

/// Read a snapshot from disk.
///
/// Supports version 3 (embedding vectors), version 2 (flags byte, no embedding),
/// and version 1 (no flags byte, no compression, no embedding).
///
/// Returns the list of entries stored in the snapshot.
pub fn read_snapshot(path: &Path) -> Result<Vec<SnapshotEntry>, String> {
    let mut f = fs::File::open(path)
        .map_err(|e| format!("failed to open snapshot {}: {e}", path.display()))?;

    // Read and validate header
    let mut magic = [0u8; 7];
    f.read_exact(&mut magic)
        .map_err(|e| format!("read magic: {e}"))?;
    if &magic != MAGIC {
        return Err(format!(
            "invalid snapshot file {}: bad magic bytes",
            path.display()
        ));
    }

    let mut version = [0u8; 1];
    f.read_exact(&mut version)
        .map_err(|e| format!("read version: {e}"))?;
    if version[0] != VERSION_3 && version[0] != VERSION_2 && version[0] != VERSION_1 {
        return Err(format!(
            "unsupported snapshot version {} in {} (expected {}, {}, or {})",
            version[0],
            path.display(),
            VERSION_1,
            VERSION_2,
            VERSION_3
        ));
    }
    let ver = version[0];
    let is_v1 = ver == VERSION_1;
    let is_v2 = ver == VERSION_2;
    let is_v3 = ver == VERSION_3;

    let mut count_bytes = [0u8; 8];
    f.read_exact(&mut count_bytes)
        .map_err(|e| format!("read entry count: {e}"))?;
    let entry_count = u64::from_le_bytes(count_bytes) as usize;

    let mut entries = Vec::with_capacity(entry_count.min(1_000_000));

    for i in 0..entry_count {
        // key_len
        let mut len_bytes = [0u8; 4];
        f.read_exact(&mut len_bytes)
            .map_err(|e| format!("read key_len at entry {i}: {e}"))?;
        let key_len = u32::from_le_bytes(len_bytes) as usize;

        // key
        let mut key_bytes = vec![0u8; key_len];
        f.read_exact(&mut key_bytes)
            .map_err(|e| format!("read key at entry {i}: {e}"))?;
        let key = String::from_utf8(key_bytes)
            .map_err(|e| format!("invalid key UTF-8 at entry {i}: {e}"))?;

        // flags (only in version 2+)
        let flags = if is_v1 {
            0u8
        } else {
            let mut flags_byte = [0u8; 1];
            f.read_exact(&mut flags_byte)
                .map_err(|e| format!("read flags at entry {i}: {e}"))?;
            flags_byte[0]
        };

        // val_len
        f.read_exact(&mut len_bytes)
            .map_err(|e| format!("read val_len at entry {i}: {e}"))?;
        let val_len = u32::from_le_bytes(len_bytes) as usize;

        // value (compressed if flag set)
        let mut stored_value = vec![0u8; val_len];
        f.read_exact(&mut stored_value)
            .map_err(|e| format!("read value at entry {i}: {e}"))?;

        let value = if (flags & FLAG_COMPRESSED) != 0 {
            lz4_flex::decompress_size_prepended(&stored_value)
                .map_err(|e| format!("LZ4 decompress failed at entry {i} key '{}': {e}", key))?
        } else {
            stored_value
        };

        // memory_type
        let mut mt_byte = [0u8; 1];
        f.read_exact(&mut mt_byte)
            .map_err(|e| format!("read memory_type at entry {i}: {e}"))?;
        let memory_type = MemoryType::from_u8(mt_byte[0]).ok_or_else(|| {
            format!(
                "invalid memory_type {} at entry {i} in {}",
                mt_byte[0],
                path.display()
            )
        })?;

        // In v3, read embedding_flag before expires_at
        // In v1/v2, no embedding data
        let embedding = if is_v3 {
            // embedding_flag: 0 = no embedding, 1 = has embedding
            let mut emb_flag_byte = [0u8; 1];
            f.read_exact(&mut emb_flag_byte)
                .map_err(|e| format!("read embedding_flag at entry {i}: {e}"))?;
            let emb_flag = emb_flag_byte[0];

            // expires_at
            let mut exp_bytes = [0u8; 8];
            f.read_exact(&mut exp_bytes)
                .map_err(|e| format!("read expires_at at entry {i}: {e}"))?;
            let expires_at_raw = u64::from_le_bytes(exp_bytes);
            let expires_at = if expires_at_raw == 0 {
                None
            } else {
                Some(expires_at_raw)
            };

            // Read embedding if flag is set
            let emb = if emb_flag == 1 {
                let mut dim_bytes = [0u8; 4];
                f.read_exact(&mut dim_bytes)
                    .map_err(|e| format!("read embedding dim at entry {i}: {e}"))?;
                let dim = u32::from_le_bytes(dim_bytes) as usize;
                let mut emb_data = vec![0u8; dim * 4];
                f.read_exact(&mut emb_data)
                    .map_err(|e| format!("read embedding data at entry {i}: {e}"))?;
                let mut embedding = Vec::with_capacity(dim);
                for j in 0..dim {
                    let offset = j * 4;
                    let val = f32::from_le_bytes([
                        emb_data[offset],
                        emb_data[offset + 1],
                        emb_data[offset + 2],
                        emb_data[offset + 3],
                    ]);
                    embedding.push(val);
                }
                Some(embedding)
            } else if emb_flag == 0 {
                None
            } else {
                return Err(format!(
                    "invalid embedding flag {} at entry {i} in {}",
                    emb_flag,
                    path.display()
                ));
            };

            // We need to return both expires_at and embedding, but we read
            // expires_at here. Use a small struct-like approach via early push.
            entries.push(SnapshotEntry {
                key,
                value,
                memory_type,
                expires_at,
                embedding: emb,
            });
            continue;
        } else {
            // v1/v2: no embedding flag, read expires_at directly
            let mut exp_bytes = [0u8; 8];
            f.read_exact(&mut exp_bytes)
                .map_err(|e| format!("read expires_at at entry {i}: {e}"))?;
            let expires_at_raw = u64::from_le_bytes(exp_bytes);
            let expires_at = if expires_at_raw == 0 {
                None
            } else {
                Some(expires_at_raw)
            };

            entries.push(SnapshotEntry {
                key,
                value,
                memory_type,
                expires_at,
                embedding: None,
            });
            continue;
        };
    }

    Ok(entries)
}

/// Find the latest snapshot file in the db_path directory.
///
/// Looks for files matching `snapshot-<timestamp>.bin` and returns
/// the one with the highest timestamp.
pub fn find_latest_snapshot(db_path: &Path) -> Option<PathBuf> {
    let dir = fs::read_dir(db_path).ok()?;
    let mut latest: Option<(u64, PathBuf)> = None;

    for entry in dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if let Some(rest) = name_str.strip_prefix(SNAPSHOT_PREFIX) {
            if let Some(ts_str) = rest.strip_suffix(SNAPSHOT_EXT) {
                if let Ok(ts) = ts_str.parse::<u64>() {
                    match &latest {
                        Some((current_ts, _)) if ts <= *current_ts => {}
                        _ => latest = Some((ts, entry.path())),
                    }
                }
            }
        }
    }

    latest.map(|(_, path)| path)
}

/// Remove old snapshot files, keeping only the N most recent.
///
/// Returns the number of files removed.
pub fn prune_snapshots(db_path: &Path, keep: usize) -> usize {
    let dir = match fs::read_dir(db_path) {
        Ok(d) => d,
        Err(_) => return 0,
    };

    // Collect all snapshot files with their timestamps
    let mut snapshots: Vec<(u64, PathBuf)> = dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let rest = name_str.strip_prefix(SNAPSHOT_PREFIX)?;
            let ts_str = rest.strip_suffix(SNAPSHOT_EXT)?;
            let ts = ts_str.parse::<u64>().ok()?;
            Some((ts, entry.path()))
        })
        .collect();

    // Sort by timestamp descending (newest first)
    snapshots.sort_by(|a, b| b.0.cmp(&a.0));

    // Remove everything beyond the keep count
    let mut removed = 0;
    for (_, path) in snapshots.into_iter().skip(keep) {
        if let Err(e) = fs::remove_file(&path) {
            warn!("failed to remove old snapshot {}: {e}", path.display());
        } else {
            removed += 1;
        }
    }

    removed
}

/// Load the latest snapshot from db_path into the KV engine.
/// Returns the number of entries loaded, or 0 if no snapshot exists.
pub fn load_latest_snapshot(
    db_path: &Path,
    engine: &crate::store::engine::KvEngine,
) -> Result<usize, String> {
    let snapshot_path = match find_latest_snapshot(db_path) {
        Some(p) => p,
        None => {
            info!("no snapshot found in {}, starting fresh", db_path.display());
            return Ok(0);
        }
    };

    info!("loading snapshot from {}", snapshot_path.display());
    let entries = read_snapshot(&snapshot_path)?;

    let now = now_ms();

    let mut loaded = 0;
    let mut expired = 0;

    for entry in &entries {
        // Skip expired entries
        if let Some(exp) = entry.expires_at {
            if exp <= now {
                expired += 1;
                continue;
            }
            let ttl_ms = exp - now;
            if entry.embedding.is_some() {
                engine.set_force_with_embedding(&entry.key, entry.value.clone(), Some(ttl_ms), entry.memory_type, entry.embedding.clone());
            } else {
                engine.set_force(&entry.key, entry.value.clone(), Some(ttl_ms), entry.memory_type);
            }
        } else {
            if entry.embedding.is_some() {
                engine.set_force_with_embedding(&entry.key, entry.value.clone(), None, entry.memory_type, entry.embedding.clone());
            } else {
                engine.set_force(&entry.key, entry.value.clone(), None, entry.memory_type);
            }
        }
        loaded += 1;
    }

    info!(
        "loaded {} entries from snapshot ({} expired skipped)",
        loaded, expired
    );

    Ok(loaded)
}

/// Collect all live entries from the KV engine for a snapshot,
/// including their embedding vectors.
pub fn collect_entries(engine: &crate::store::engine::KvEngine) -> Vec<SnapshotEntry> {
    // Scan all keys (empty prefix matches everything), including embeddings
    let results = engine.scan_prefix_with_embeddings("");
    results
        .into_iter()
        .map(|(key, value, meta)| SnapshotEntry {
            key,
            value,
            memory_type: meta.memory_type,
            expires_at: meta.ttl_remaining_ms.map(|ttl| now_ms() + ttl),
            embedding: meta.embedding,
        })
        .collect()
}

/// Perform a snapshot: collect entries, write to disk, prune old snapshots.
pub fn snapshot(
    db_path: &Path,
    engine: &crate::store::engine::KvEngine,
    keep_snapshots: usize,
) -> Result<PathBuf, String> {
    snapshot_with_threshold(db_path, engine, keep_snapshots, DEFAULT_COMPRESSION_THRESHOLD)
}

/// Perform a snapshot with a custom compression threshold.
pub fn snapshot_with_threshold(
    db_path: &Path,
    engine: &crate::store::engine::KvEngine,
    keep_snapshots: usize,
    compression_threshold: usize,
) -> Result<PathBuf, String> {
    let entries = collect_entries(engine);
    let now_ms_val = now_ms();

    let path = write_snapshot(db_path, &entries, now_ms_val, compression_threshold)?;
    let pruned = prune_snapshots(db_path, keep_snapshots);
    if pruned > 0 {
        debug!("pruned {pruned} old snapshot(s)");
    }
    Ok(path)
}

/// Start the auto-snapshot background task.
/// Returns a JoinHandle that will complete when the shutdown signal is received.
pub async fn start_snapshot_loop(
    db_path: PathBuf,
    engine: std::sync::Arc<crate::store::engine::KvEngine>,
    interval_ms: u64,
    keep_snapshots: usize,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    start_snapshot_loop_with_threshold(
        db_path,
        engine,
        interval_ms,
        keep_snapshots,
        DEFAULT_COMPRESSION_THRESHOLD,
        shutdown,
    )
    .await
}

/// Start the auto-snapshot background task with a custom compression threshold.
pub async fn start_snapshot_loop_with_threshold(
    db_path: PathBuf,
    engine: std::sync::Arc<crate::store::engine::KvEngine>,
    interval_ms: u64,
    keep_snapshots: usize,
    compression_threshold: usize,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if interval_ms == 0 {
        info!("auto-snapshot disabled (interval = 0)");
        return;
    }

    info!(
        "auto-snapshot enabled: every {}ms, keeping {} snapshots, compression threshold {} bytes",
        interval_ms, keep_snapshots, compression_threshold
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)) => {
                match snapshot_with_threshold(&db_path, &engine, keep_snapshots, compression_threshold) {
                    Ok(path) => info!("auto-snapshot saved: {} ({} entries)", path.display(), {
                        let entries = collect_entries(&engine);
                        entries.len()
                    }),
                    Err(e) => error!("auto-snapshot failed: {e}"),
                }
            }
            _ = shutdown.changed() => {
                info!("auto-snapshot shutting down, performing final snapshot...");
                match snapshot_with_threshold(&db_path, &engine, keep_snapshots, compression_threshold) {
                    Ok(path) => info!("final snapshot saved: {}", path.display()),
                    Err(e) => error!("final snapshot failed: {e}"),
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory_type::MemoryType;
    use std::fs;

    #[test]
    fn test_write_and_read_snapshot() {
        let dir = std::env::temp_dir().join("basalt_test_snapshot");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let entries = vec![
            SnapshotEntry {
                key: "ns:mem1".to_string(),
                value: b"hello world".to_vec(),
                memory_type: MemoryType::Semantic,
                expires_at: None,
                embedding: None,
            },
            SnapshotEntry {
                key: "ns:mem2".to_string(),
                value: b"episodic memory".to_vec(),
                memory_type: MemoryType::Episodic,
                expires_at: Some(9999999999999),
                embedding: None,
            },
            SnapshotEntry {
                key: "ns:mem3".to_string(),
                value: vec![0, 1, 2, 3, 255],
                memory_type: MemoryType::Procedural,
                expires_at: Some(12345),
                embedding: None,
            },
        ];

        let path = write_snapshot(&dir, &entries, 1000, 1024).unwrap();
        assert!(path.exists());

        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 3);

        assert_eq!(loaded[0].key, "ns:mem1");
        assert_eq!(loaded[0].value, b"hello world");
        assert_eq!(loaded[0].memory_type, MemoryType::Semantic);
        assert_eq!(loaded[0].expires_at, None);
        assert_eq!(loaded[0].embedding, None);

        assert_eq!(loaded[1].key, "ns:mem2");
        assert_eq!(loaded[1].value, b"episodic memory");
        assert_eq!(loaded[1].memory_type, MemoryType::Episodic);
        assert_eq!(loaded[1].expires_at, Some(9999999999999));
        assert_eq!(loaded[1].embedding, None);

        assert_eq!(loaded[2].key, "ns:mem3");
        assert_eq!(loaded[2].value, vec![0, 1, 2, 3, 255]);
        assert_eq!(loaded[2].memory_type, MemoryType::Procedural);
        assert_eq!(loaded[2].expires_at, Some(12345));
        assert_eq!(loaded[2].embedding, None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_find_latest_snapshot() {
        let dir = std::env::temp_dir().join("basalt_test_find_latest");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // No snapshots yet
        assert!(find_latest_snapshot(&dir).is_none());

        // Create some snapshot files
        fs::write(dir.join("snapshot-1000.bin"), b"").unwrap();
        fs::write(dir.join("snapshot-2000.bin"), b"").unwrap();
        fs::write(dir.join("snapshot-500.bin"), b"").unwrap();
        // Non-snapshot file should be ignored
        fs::write(dir.join("other-file.txt"), b"").unwrap();

        let latest = find_latest_snapshot(&dir).unwrap();
        assert_eq!(latest.file_name().unwrap(), "snapshot-2000.bin");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_prune_snapshots() {
        let dir = std::env::temp_dir().join("basalt_test_prune");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Create 5 snapshots
        for ts in [100, 200, 300, 400, 500] {
            fs::write(dir.join(format!("snapshot-{ts}.bin")), b"").unwrap();
        }

        // Keep only 2
        let removed = prune_snapshots(&dir, 2);
        assert_eq!(removed, 3);

        // The 2 newest should remain
        let remaining: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("snapshot-"))
            .collect();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&"snapshot-500.bin".to_string()));
        assert!(remaining.contains(&"snapshot-400.bin".to_string()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_empty_snapshot() {
        let dir = std::env::temp_dir().join("basalt_test_empty_snap");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let entries: Vec<SnapshotEntry> = vec![];
        let path = write_snapshot(&dir, &entries, 1000, 1024).unwrap();

        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 0);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_invalid_magic() {
        let dir = std::env::temp_dir().join("basalt_test_bad_magic");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("snapshot-bad.bin");
        fs::write(&path, b"INVALID\x00\x01\x00\x00\x00\x00\x00\x00\x00").unwrap();

        let result = read_snapshot(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("bad magic"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_memory_type_roundtrip() {
        assert_eq!(MemoryType::from_u8(MemoryType::Episodic.to_u8()), Some(MemoryType::Episodic));
        assert_eq!(MemoryType::from_u8(MemoryType::Semantic.to_u8()), Some(MemoryType::Semantic));
        assert_eq!(MemoryType::from_u8(MemoryType::Procedural.to_u8()), Some(MemoryType::Procedural));
        assert_eq!(MemoryType::from_u8(255), None);
    }

    #[test]
    fn test_compression_large_values() {
        let dir = std::env::temp_dir().join("basalt_test_compression_large");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Create a value that is larger than the threshold (1024 bytes)
        // Use a compressible pattern so LZ4 actually reduces size
        let large_value: Vec<u8> = "ABCD".repeat(512).into_bytes(); // 2048 bytes of repeating data
        assert!(large_value.len() > 1024);

        let entries = vec![
            SnapshotEntry {
                key: "large:data".to_string(),
                value: large_value.clone(),
                memory_type: MemoryType::Semantic,
                expires_at: None,
                embedding: None,
            },
        ];

        // Write with threshold of 1024
        let path = write_snapshot(&dir, &entries, 1000, 1024).unwrap();

        // Read back and verify data integrity
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key, "large:data");
        assert_eq!(loaded[0].value, large_value);
        assert_eq!(loaded[0].memory_type, MemoryType::Semantic);
        assert_eq!(loaded[0].expires_at, None);
        assert_eq!(loaded[0].embedding, None);

        // Verify the snapshot file is smaller than the uncompressed data
        let file_size = fs::metadata(&path).unwrap().len() as usize;
        let uncompressed_data_size = large_value.len();
        // File should be smaller than if the value were stored uncompressed
        // (header is ~17 bytes + key overhead, so file should be well under uncompressed size + header)
        assert!(
            file_size < uncompressed_data_size + 100,
            "file size {} should be less than uncompressed data size {} + overhead",
            file_size,
            uncompressed_data_size
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_no_compression_small_values() {
        let dir = std::env::temp_dir().join("basalt_test_no_compress_small");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Small value well below the threshold
        let small_value = b"tiny data".to_vec();
        assert!(small_value.len() < 1024);

        let entries = vec![
            SnapshotEntry {
                key: "small:data".to_string(),
                value: small_value.clone(),
                memory_type: MemoryType::Semantic,
                expires_at: None,
                embedding: None,
            },
        ];

        // Write with threshold of 1024
        let path = write_snapshot(&dir, &entries, 1000, 1024).unwrap();

        // Read back
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key, "small:data");
        assert_eq!(loaded[0].value, small_value);

        // Verify the snapshot file does NOT have the compressed flag set
        // by reading the raw bytes and checking the flags byte
        let raw = fs::read(&path).unwrap();
        // Header: 7 bytes magic + 1 byte version + 8 bytes count = 16
        // Then: 4 bytes key_len + key bytes + 1 byte flags
        let key_bytes = b"small:data";
        let key_len_offset = 16; // after header
        let flags_offset = key_len_offset + 4 + key_bytes.len();
        let flags = raw[flags_offset];
        assert_eq!(flags & FLAG_COMPRESSED, 0, "small value should NOT be compressed");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_backward_compatibility_v1() {
        let dir = std::env::temp_dir().join("basalt_test_v1_compat");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Manually create a version 1 snapshot (no flags byte)
        let path = dir.join("snapshot-1000.bin");
        let mut data = Vec::new();

        // Magic
        data.extend_from_slice(b"BASALT\x00");
        // Version 1
        data.push(1u8);
        // Count: 2 entries
        data.extend_from_slice(&2u64.to_le_bytes());

        // Entry 1: key="hello", value="world", memory_type=Semantic, expires_at=0
        let key1 = b"hello";
        data.extend_from_slice(&(key1.len() as u32).to_le_bytes());
        data.extend_from_slice(key1);
        let val1 = b"world";
        data.extend_from_slice(&(val1.len() as u32).to_le_bytes());
        data.extend_from_slice(val1);
        data.push(1u8); // Semantic
        data.extend_from_slice(&0u64.to_le_bytes()); // no expiry

        // Entry 2: key="test", value=[0,1,2,3], memory_type=Procedural, expires_at=99999
        let key2 = b"test";
        data.extend_from_slice(&(key2.len() as u32).to_le_bytes());
        data.extend_from_slice(key2);
        let val2: Vec<u8> = vec![0, 1, 2, 3];
        data.extend_from_slice(&(val2.len() as u32).to_le_bytes());
        data.extend_from_slice(&val2);
        data.push(2u8); // Procedural
        data.extend_from_slice(&99999u64.to_le_bytes());

        fs::write(&path, &data).unwrap();

        // Read back using current reader — should handle v1 format
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 2);

        assert_eq!(loaded[0].key, "hello");
        assert_eq!(loaded[0].value, b"world".to_vec());
        assert_eq!(loaded[0].memory_type, MemoryType::Semantic);
        assert_eq!(loaded[0].expires_at, None);
        assert_eq!(loaded[0].embedding, None);

        assert_eq!(loaded[1].key, "test");
        assert_eq!(loaded[1].value, vec![0, 1, 2, 3]);
        assert_eq!(loaded[1].memory_type, MemoryType::Procedural);
        assert_eq!(loaded[1].expires_at, Some(99999));
        assert_eq!(loaded[1].embedding, None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_backward_compatibility_v2() {
        let dir = std::env::temp_dir().join("basalt_test_v2_compat");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Manually create a version 2 snapshot (flags byte, but no embedding_flag/embedding data)
        // v2 format per entry: key_len(u32) key bytes flags(u8) val_len(u32) value bytes memory_type(u8) expires_at(u64)
        let path = dir.join("snapshot-1000.bin");
        let mut data = Vec::new();

        // Magic
        data.extend_from_slice(b"BASALT\x00");
        // Version 2
        data.push(2u8);
        // Count: 2 entries
        data.extend_from_slice(&2u64.to_le_bytes());

        // Entry 1: key="v2key1", value="v2val1", flags=0, memory_type=Semantic, expires_at=0
        let key1 = b"v2key1";
        data.extend_from_slice(&(key1.len() as u32).to_le_bytes());
        data.extend_from_slice(key1);
        data.push(0u8); // flags: no compression
        let val1 = b"v2val1";
        data.extend_from_slice(&(val1.len() as u32).to_le_bytes());
        data.extend_from_slice(val1);
        data.push(1u8); // Semantic
        data.extend_from_slice(&0u64.to_le_bytes()); // no expiry

        // Entry 2: key="v2key2", value=[10,20,30], flags=0, memory_type=Episodic, expires_at=55555
        let key2 = b"v2key2";
        data.extend_from_slice(&(key2.len() as u32).to_le_bytes());
        data.extend_from_slice(key2);
        data.push(0u8); // flags: no compression
        let val2: Vec<u8> = vec![10, 20, 30];
        data.extend_from_slice(&(val2.len() as u32).to_le_bytes());
        data.extend_from_slice(&val2);
        data.push(0u8); // Episodic
        data.extend_from_slice(&55555u64.to_le_bytes());

        fs::write(&path, &data).unwrap();

        // Read back using current reader (v3) — should handle v2 format
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 2);

        assert_eq!(loaded[0].key, "v2key1");
        assert_eq!(loaded[0].value, b"v2val1".to_vec());
        assert_eq!(loaded[0].memory_type, MemoryType::Semantic);
        assert_eq!(loaded[0].expires_at, None);
        assert_eq!(loaded[0].embedding, None);

        assert_eq!(loaded[1].key, "v2key2");
        assert_eq!(loaded[1].value, vec![10, 20, 30]);
        assert_eq!(loaded[1].memory_type, MemoryType::Episodic);
        assert_eq!(loaded[1].expires_at, Some(55555));
        assert_eq!(loaded[1].embedding, None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_embedding_roundtrip() {
        let dir = std::env::temp_dir().join("basalt_test_embedding");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let embedding1 = vec![0.1, 0.2, 0.3, 0.4, 0.5f32];
        let embedding2 = vec![-1.0, 2.5, -0.001, 100.0f32];

        let entries = vec![
            SnapshotEntry {
                key: "emb:1".to_string(),
                value: b"data1".to_vec(),
                memory_type: MemoryType::Semantic,
                expires_at: None,
                embedding: Some(embedding1.clone()),
            },
            SnapshotEntry {
                key: "emb:2".to_string(),
                value: b"data2".to_vec(),
                memory_type: MemoryType::Episodic,
                expires_at: Some(88888),
                embedding: Some(embedding2.clone()),
            },
            SnapshotEntry {
                key: "noemb:3".to_string(),
                value: b"data3".to_vec(),
                memory_type: MemoryType::Procedural,
                expires_at: Some(99999),
                embedding: None,
            },
        ];

        let path = write_snapshot(&dir, &entries, 1000, 1024).unwrap();
        assert!(path.exists());

        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 3);

        // Entry with embedding, no expiry
        assert_eq!(loaded[0].key, "emb:1");
        assert_eq!(loaded[0].value, b"data1");
        assert_eq!(loaded[0].memory_type, MemoryType::Semantic);
        assert_eq!(loaded[0].expires_at, None);
        assert!(loaded[0].embedding.is_some());
        let emb0 = loaded[0].embedding.as_ref().unwrap();
        assert_eq!(emb0.len(), 5);
        for (i, &v) in embedding1.iter().enumerate() {
            assert!((emb0[i] - v).abs() < 1e-6, "embedding1[{}] = {} vs {}", i, emb0[i], v);
        }

        // Entry with embedding and expiry
        assert_eq!(loaded[1].key, "emb:2");
        assert_eq!(loaded[1].value, b"data2");
        assert_eq!(loaded[1].memory_type, MemoryType::Episodic);
        assert_eq!(loaded[1].expires_at, Some(88888));
        assert!(loaded[1].embedding.is_some());
        let emb1 = loaded[1].embedding.as_ref().unwrap();
        assert_eq!(emb1.len(), 4);
        for (i, &v) in embedding2.iter().enumerate() {
            assert!((emb1[i] - v).abs() < 1e-6, "embedding2[{}] = {} vs {}", i, emb1[i], v);
        }

        // Entry without embedding
        assert_eq!(loaded[2].key, "noemb:3");
        assert_eq!(loaded[2].value, b"data3");
        assert_eq!(loaded[2].memory_type, MemoryType::Procedural);
        assert_eq!(loaded[2].expires_at, Some(99999));
        assert_eq!(loaded[2].embedding, None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_embedding_with_compression() {
        let dir = std::env::temp_dir().join("basalt_test_emb_compress");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Large value to trigger compression, with embedding
        let large_value: Vec<u8> = "ABCDEFGH".repeat(256).into_bytes();
        let embedding = vec![1.0f32, -1.0, 0.5, 0.25, 0.125];

        let entries = vec![
            SnapshotEntry {
                key: "big:emb".to_string(),
                value: large_value.clone(),
                memory_type: MemoryType::Semantic,
                expires_at: Some(12345678),
                embedding: Some(embedding.clone()),
            },
        ];

        let path = write_snapshot(&dir, &entries, 1000, 1024).unwrap();
        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key, "big:emb");
        assert_eq!(loaded[0].value, large_value);
        assert_eq!(loaded[0].memory_type, MemoryType::Semantic);
        assert_eq!(loaded[0].expires_at, Some(12345678));
        assert!(loaded[0].embedding.is_some());
        let emb = loaded[0].embedding.as_ref().unwrap();
        assert_eq!(emb.len(), 5);
        for (i, &v) in embedding.iter().enumerate() {
            assert!((emb[i] - v).abs() < 1e-6);
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_compression_disabled_with_zero_threshold() {
        let dir = std::env::temp_dir().join("basalt_test_no_compress_zero");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Large value but compression disabled
        let large_value: Vec<u8> = "XYZ".repeat(1024).into_bytes(); // 3072 bytes
        let entries = vec![
            SnapshotEntry {
                key: "big:data".to_string(),
                value: large_value.clone(),
                memory_type: MemoryType::Semantic,
                expires_at: None,
                embedding: None,
            },
        ];

        // threshold=0 disables compression
        let path = write_snapshot(&dir, &entries, 1000, 0).unwrap();

        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].value, large_value);

        // Verify no compression flag in file
        let raw = fs::read(&path).unwrap();
        let key_bytes = b"big:data";
        let key_len_offset = 16; // after header
        let flags_offset = key_len_offset + 4 + key_bytes.len();
        let flags = raw[flags_offset];
        assert_eq!(flags & FLAG_COMPRESSED, 0, "compression should be disabled with threshold=0");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_unsupported_version() {
        let dir = std::env::temp_dir().join("basalt_test_bad_version");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("snapshot-badver.bin");
        let mut data = Vec::new();
        data.extend_from_slice(b"BASALT\x00");
        data.push(99u8); // unsupported version
        data.extend_from_slice(&0u64.to_le_bytes());

        fs::write(&path, &data).unwrap();

        let result = read_snapshot(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("unsupported snapshot version"), "error was: {err}");
        assert!(err.contains("99"));

        fs::remove_dir_all(&dir).ok();
    }
}
