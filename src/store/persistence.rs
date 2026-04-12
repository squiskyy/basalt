/// Persistence module for Basalt KV store.
///
/// Simple, fast binary snapshot format:
///   - Magic bytes: b"BASALT\x00" (8 bytes)
///   - Version: u8 (1 byte, currently 1)
///   - Entry count: u64 (8 bytes, little-endian)
///   - For each entry:
///     - key_len: u32 (4 bytes, LE)
///     - key: [u8; key_len]
///     - val_len: u32 (4 bytes, LE)
///     - value: [u8; val_len]
///     - memory_type: u8 (0=episodic, 1=semantic, 2=procedural)
///     - expires_at: u64 (8 bytes, LE, 0 = no expiry)
///
/// Snapshots are written atomically: write to a temp file, then rename.
/// On startup, load the latest snapshot from the db_path directory.

use crate::store::memory_type::MemoryType;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

/// Magic bytes for snapshot files.
const MAGIC: &[u8; 7] = b"BASALT\x00";

/// Current snapshot format version.
const VERSION: u8 = 1;

/// File name pattern for snapshots: `snapshot-<timestamp>.bin`
const SNAPSHOT_PREFIX: &str = "snapshot-";
const SNAPSHOT_EXT: &str = ".bin";

/// Convert MemoryType to u8 for serialization.
fn memory_type_to_u8(mt: MemoryType) -> u8 {
    match mt {
        MemoryType::Episodic => 0,
        MemoryType::Semantic => 1,
        MemoryType::Procedural => 2,
    }
}

/// Convert u8 to MemoryType for deserialization.
fn u8_to_memory_type(v: u8) -> Option<MemoryType> {
    match v {
        0 => Some(MemoryType::Episodic),
        1 => Some(MemoryType::Semantic),
        2 => Some(MemoryType::Procedural),
        _ => None,
    }
}

/// A single entry to be persisted, with its key.
#[derive(Debug)]
pub struct SnapshotEntry {
    pub key: String,
    pub value: Vec<u8>,
    pub memory_type: MemoryType,
    pub expires_at: Option<u64>,
}

/// Write a snapshot to disk atomically.
///
/// Writes to a temp file first, then renames to the final path.
/// Returns the path of the written snapshot file.
pub fn write_snapshot(
    db_path: &Path,
    entries: &[SnapshotEntry],
    timestamp_ms: u64,
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
        f.write_all(&(entry.value.len() as u32).to_le_bytes())
            .map_err(|e| format!("write val_len: {e}"))?;
        f.write_all(&entry.value)
            .map_err(|e| format!("write value: {e}"))?;
        f.write_all(&[memory_type_to_u8(entry.memory_type)])
            .map_err(|e| format!("write memory_type: {e}"))?;
        f.write_all(
            &entry
                .expires_at
                .unwrap_or(0)
                .to_le_bytes(),
        )
        .map_err(|e| format!("write expires_at: {e}"))?;
    }

    f.flush()
        .map_err(|e| format!("flush snapshot: {e}"))?;
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
    if version[0] != VERSION {
        return Err(format!(
            "unsupported snapshot version {} in {} (expected {})",
            version[0],
            path.display(),
            VERSION
        ));
    }

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

        // val_len
        f.read_exact(&mut len_bytes)
            .map_err(|e| format!("read val_len at entry {i}: {e}"))?;
        let val_len = u32::from_le_bytes(len_bytes) as usize;

        // value
        let mut value = vec![0u8; val_len];
        f.read_exact(&mut value)
            .map_err(|e| format!("read value at entry {i}: {e}"))?;

        // memory_type
        let mut mt_byte = [0u8; 1];
        f.read_exact(&mut mt_byte)
            .map_err(|e| format!("read memory_type at entry {i}: {e}"))?;
        let memory_type = u8_to_memory_type(mt_byte[0]).ok_or_else(|| {
            format!(
                "invalid memory_type {} at entry {i} in {}",
                mt_byte[0],
                path.display()
            )
        })?;

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

        entries.push(SnapshotEntry {
            key,
            value,
            memory_type,
            expires_at,
        });
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

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64;

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
            engine.set_force(&entry.key, entry.value.clone(), Some(ttl_ms), entry.memory_type);
        } else {
            engine.set_force(&entry.key, entry.value.clone(), None, entry.memory_type);
        }
        loaded += 1;
    }

    info!(
        "loaded {} entries from snapshot ({} expired skipped)",
        loaded, expired
    );

    Ok(loaded)
}

/// Collect all live entries from the KV engine for a snapshot.
pub fn collect_entries(engine: &crate::store::engine::KvEngine) -> Vec<SnapshotEntry> {
    // Scan all keys (empty prefix matches everything)
    let results = engine.scan_prefix("");
    results
        .into_iter()
        .map(|(key, value, meta)| SnapshotEntry {
            key,
            value,
            memory_type: meta.memory_type,
            expires_at: meta.ttl_remaining_ms.map(|ttl| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time went backwards")
                    .as_millis() as u64
                    + ttl
            }),
        })
        .collect()
}

/// Perform a snapshot: collect entries, write to disk, prune old snapshots.
pub fn snapshot(
    db_path: &Path,
    engine: &crate::store::engine::KvEngine,
    keep_snapshots: usize,
) -> Result<PathBuf, String> {
    let entries = collect_entries(engine);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64;

    let path = write_snapshot(db_path, &entries, now_ms)?;
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
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if interval_ms == 0 {
        info!("auto-snapshot disabled (interval = 0)");
        return;
    }

    info!(
        "auto-snapshot enabled: every {}ms, keeping {} snapshots",
        interval_ms, keep_snapshots
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)) => {
                match snapshot(&db_path, &engine, keep_snapshots) {
                    Ok(path) => info!("auto-snapshot saved: {} ({} entries)", path.display(), {
                        let entries = collect_entries(&engine);
                        entries.len()
                    }),
                    Err(e) => error!("auto-snapshot failed: {e}"),
                }
            }
            _ = shutdown.changed() => {
                info!("auto-snapshot shutting down, performing final snapshot...");
                match snapshot(&db_path, &engine, keep_snapshots) {
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
            },
            SnapshotEntry {
                key: "ns:mem2".to_string(),
                value: b"episodic memory".to_vec(),
                memory_type: MemoryType::Episodic,
                expires_at: Some(9999999999999),
            },
            SnapshotEntry {
                key: "ns:mem3".to_string(),
                value: vec![0, 1, 2, 3, 255],
                memory_type: MemoryType::Procedural,
                expires_at: Some(12345),
            },
        ];

        let path = write_snapshot(&dir, &entries, 1000).unwrap();
        assert!(path.exists());

        let loaded = read_snapshot(&path).unwrap();
        assert_eq!(loaded.len(), 3);

        assert_eq!(loaded[0].key, "ns:mem1");
        assert_eq!(loaded[0].value, b"hello world");
        assert_eq!(loaded[0].memory_type, MemoryType::Semantic);
        assert_eq!(loaded[0].expires_at, None);

        assert_eq!(loaded[1].key, "ns:mem2");
        assert_eq!(loaded[1].value, b"episodic memory");
        assert_eq!(loaded[1].memory_type, MemoryType::Episodic);
        assert_eq!(loaded[1].expires_at, Some(9999999999999));

        assert_eq!(loaded[2].key, "ns:mem3");
        assert_eq!(loaded[2].value, vec![0, 1, 2, 3, 255]);
        assert_eq!(loaded[2].memory_type, MemoryType::Procedural);
        assert_eq!(loaded[2].expires_at, Some(12345));

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
        let path = write_snapshot(&dir, &entries, 1000).unwrap();

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
        assert_eq!(u8_to_memory_type(memory_type_to_u8(MemoryType::Episodic)), Some(MemoryType::Episodic));
        assert_eq!(u8_to_memory_type(memory_type_to_u8(MemoryType::Semantic)), Some(MemoryType::Semantic));
        assert_eq!(u8_to_memory_type(memory_type_to_u8(MemoryType::Procedural)), Some(MemoryType::Procedural));
        assert_eq!(u8_to_memory_type(255), None);
    }
}
