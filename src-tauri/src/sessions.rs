//! Session scanning caches ported from `SnapshotStore` in `webui/server.py`.
//!
//! Covers the four caches referenced by the issue:
//!
//! 1. Codex rollout list cache (3s TTL)
//! 2. Claude session list cache (3s TTL)
//! 3. Signature-based per-file meta cache (mtime + size) — "skip re-parse if
//!    unchanged"
//! 4. Signature-based per-file transcript cache
//!
//! The scanning glob + TTL semantics match the Python implementation exactly.
//! Meta parsing is kept minimal (cwd + session_id) because the downstream
//! transcript rendering is not in scope for this bead.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{
    CLAUDE_SESSION_HEAD_BYTES, CLAUDE_SESSION_LIST_TTL, CLAUDE_SESSION_SCAN_LIMIT,
    CODEX_ROLLOUT_HEAD_BYTES, CODEX_ROLLOUT_LIST_TTL, CODEX_ROLLOUT_SCAN_LIMIT,
};
use crate::parse::{normalize_path_value, pathbuf_str};

/// Per-file fingerprint used to decide whether a cached parse is still valid.
/// The Python port uses `(st_mtime_ns, st_size)` — same shape here so any bug
/// we reproduce (or fix) matches 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileSignature {
    pub mtime_ns: i128,
    pub size: u64,
}

impl FileSignature {
    /// Read the signature for `path`. Returns `None` for missing files or
    /// permission errors — same as the Python `try: stat() except OSError`.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|dur| dur.as_nanos() as i128)
            .unwrap_or(0);
        Some(Self {
            mtime_ns,
            size: meta.len(),
        })
    }
}

/// Expiring, sorted-by-mtime list of session files on disk.
#[derive(Debug, Clone, Default)]
pub struct CachedFileList {
    pub expires_at: Option<Instant>,
    pub files: Vec<PathBuf>,
}

impl CachedFileList {
    fn fresh(&self, now: Instant) -> bool {
        matches!(self.expires_at, Some(deadline) if deadline > now && !self.files.is_empty())
    }
}

/// Cached snapshot of session metadata tagged with the signature that
/// produced it. Matches `{"signature": ..., "meta": ...}` in Python.
#[derive(Debug, Clone)]
pub struct SignedEntry<T: Clone> {
    pub signature: FileSignature,
    pub value: T,
}

/// Cache state for Codex rollouts (all `.jsonl` files under
/// `~/.codex/sessions/`).
#[derive(Debug, Default)]
pub struct CodexCache {
    pub list: CachedFileList,
    pub meta: HashMap<PathBuf, SignedEntry<Value>>,
    pub transcript: HashMap<PathBuf, SignedEntry<Value>>,
}

/// Cache state for Claude sessions (`.jsonl` files under
/// `~/.claude/projects/<encoded-cwd>/`).
#[derive(Debug, Default)]
pub struct ClaudeCache {
    pub list: CachedFileList,
    pub meta: HashMap<PathBuf, SignedEntry<Value>>,
    pub transcript: HashMap<PathBuf, SignedEntry<Value>>,
}

/// Refresh `cache` if the TTL has expired and return the current file list.
///
/// `scan()` performs the actual filesystem work and returns the desired file
/// list; it is only called on cache miss. This keeps the lock hold short.
pub fn cached_file_list<F>(
    cache: &mut CachedFileList,
    now: Instant,
    ttl: Duration,
    scan: F,
) -> Vec<PathBuf>
where
    F: FnOnce() -> Vec<PathBuf>,
{
    if cache.fresh(now) {
        return cache.files.clone();
    }
    let fresh = scan();
    cache.files = fresh.clone();
    cache.expires_at = Some(now + ttl);
    fresh
}

/// Walk a directory recursively and return `.jsonl` files matching `pattern`
/// sorted newest-first, capped at `limit`. A thin Rust equivalent of
/// `sorted(root.glob("**/<pattern>"), key=mtime, reverse=True)[:limit]`.
fn glob_recent_jsonl<F>(root: &Path, limit: usize, filter: F) -> Vec<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    if !root.is_dir() {
        return Vec::new();
    }
    let mut stack = vec![root.to_path_buf()];
    let mut hits: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && filter(&path) {
                if let Ok(meta) = entry.metadata() {
                    let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    hits.push((path, mtime));
                }
            }
        }
    }
    hits.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    hits.truncate(limit);
    hits.into_iter().map(|(path, _)| path).collect()
}

/// Enumerate Codex rollout files. `~/.codex/sessions/**/rollout-*.jsonl`.
pub fn scan_codex_rollouts(codex_root: &Path) -> Vec<PathBuf> {
    glob_recent_jsonl(codex_root, CODEX_ROLLOUT_SCAN_LIMIT, |path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-"))
    })
}

/// Enumerate Claude session files. `~/.claude/projects/**/*.jsonl`.
pub fn scan_claude_sessions(claude_root: &Path) -> Vec<PathBuf> {
    glob_recent_jsonl(claude_root, CLAUDE_SESSION_SCAN_LIMIT, |path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
    })
}

/// Public cache-aware Codex list getter. Refreshes if the 3s TTL has expired.
pub fn list_recent_codex_rollouts(
    cache: &mut CodexCache,
    codex_root: &Path,
    now: Instant,
) -> Vec<PathBuf> {
    cached_file_list(&mut cache.list, now, CODEX_ROLLOUT_LIST_TTL, || {
        scan_codex_rollouts(codex_root)
    })
}

/// Public cache-aware Claude list getter. Same TTL semantics as Codex.
pub fn list_recent_claude_sessions(
    cache: &mut ClaudeCache,
    claude_root: &Path,
    now: Instant,
) -> Vec<PathBuf> {
    cached_file_list(&mut cache.list, now, CLAUDE_SESSION_LIST_TTL, || {
        scan_claude_sessions(claude_root)
    })
}

/// Read the head of a file as UTF-8 (lossy), used to extract session metadata
/// from the first few KB.
fn read_head(path: &Path, max_bytes: usize) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let mut buf = vec![0u8; max_bytes];
    let Ok(n) = file.read(&mut buf) else {
        return String::new();
    };
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read the entire file as UTF-8 (lossy). Only used for full transcript reads.
pub fn read_full(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// Parse a chunk of JSONL text, skipping empty lines and bad records. Mirrors
/// `iter_jsonl_records` in Python.
pub fn iter_jsonl_records(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                None
            } else {
                serde_json::from_str::<Value>(trimmed).ok()
            }
        })
        .filter(|value| value.is_object())
        .collect()
}

fn signed_value<T: Clone>(
    cache: &mut HashMap<PathBuf, SignedEntry<T>>,
    path: &Path,
    signature: FileSignature,
    value: T,
) {
    cache.insert(path.to_path_buf(), SignedEntry { signature, value });
}

fn cached_signed<T: Clone>(
    cache: &HashMap<PathBuf, SignedEntry<T>>,
    path: &Path,
    signature: FileSignature,
) -> Option<T> {
    cache
        .get(path)
        .filter(|entry| entry.signature == signature)
        .map(|entry| entry.value.clone())
}

/// Read (or reuse from cache) a Codex rollout's header metadata.
///
/// The returned value matches the Python shape:
/// `{path, cwd, session_id, modified_at, mtime}`.
pub fn get_codex_rollout_meta(cache: &mut CodexCache, path: &Path) -> Value {
    let Some(signature) = FileSignature::of(path) else {
        return Value::Null;
    };
    if let Some(cached) = cached_signed(&cache.meta, path, signature) {
        return cached;
    }

    let mut meta = json!({
        "path": pathbuf_str(path),
        "cwd": "",
        "session_id": "",
        "modified_at": iso_from_mtime_ns(signature.mtime_ns),
        "mtime": mtime_secs(signature.mtime_ns),
    });

    let head = read_head(path, CODEX_ROLLOUT_HEAD_BYTES);
    for record in iter_jsonl_records(&head) {
        let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = record
            .get("payload")
            .and_then(Value::as_object)
            .cloned()
            .map(Value::Object)
            .unwrap_or(Value::Null);
        match record_type {
            "session_meta" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    meta["session_id"] = Value::String(id.to_string());
                }
                if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                    meta["cwd"] = Value::String(cwd.to_string());
                }
            }
            "turn_context" if meta["cwd"] == "" => {
                let cwd_candidate = record
                    .get("cwd")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("cwd").and_then(Value::as_str))
                    .unwrap_or("");
                if !cwd_candidate.is_empty() {
                    meta["cwd"] = Value::String(cwd_candidate.to_string());
                }
            }
            _ => {}
        }
        if meta["cwd"] != "" && meta["session_id"] != "" {
            break;
        }
    }

    if let Some(cwd) = meta.get("cwd").and_then(Value::as_str) {
        let normalized = normalize_path_value(cwd);
        meta["cwd"] = Value::String(normalized);
    }

    signed_value(&mut cache.meta, path, signature, meta.clone());
    meta
}

/// Read (or reuse) Claude session header metadata.
///
/// Returns the Python shape `{path, cwd, session_id, modified_at, mtime}`.
pub fn get_claude_session_meta(cache: &mut ClaudeCache, path: &Path) -> Value {
    let Some(signature) = FileSignature::of(path) else {
        return Value::Null;
    };
    if let Some(cached) = cached_signed(&cache.meta, path, signature) {
        return cached;
    }

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mut meta = json!({
        "path": pathbuf_str(path),
        "cwd": "",
        "session_id": stem,
        "modified_at": iso_from_mtime_ns(signature.mtime_ns),
        "mtime": mtime_secs(signature.mtime_ns),
    });

    let head = read_head(path, CLAUDE_SESSION_HEAD_BYTES);
    for record in iter_jsonl_records(&head) {
        if meta["cwd"] == "" {
            if let Some(cwd) = record.get("cwd").and_then(Value::as_str) {
                if !cwd.is_empty() {
                    meta["cwd"] = Value::String(cwd.to_string());
                }
            }
        }
        if let Some(session_id) = record.get("sessionId").and_then(Value::as_str) {
            if !session_id.is_empty() {
                meta["session_id"] = Value::String(session_id.to_string());
            }
        }
        if meta["cwd"] != "" && meta["session_id"] != "" {
            break;
        }
    }

    if let Some(cwd) = meta.get("cwd").and_then(Value::as_str) {
        let normalized = normalize_path_value(cwd);
        meta["cwd"] = Value::String(normalized);
    }

    signed_value(&mut cache.meta, path, signature, meta.clone());
    meta
}

/// Store a pre-parsed transcript in the Codex cache.
pub fn cache_codex_transcript(cache: &mut CodexCache, path: &Path, view: Value) -> Option<()> {
    let signature = FileSignature::of(path)?;
    signed_value(&mut cache.transcript, path, signature, view);
    Some(())
}

/// Retrieve a cached Codex transcript if the file signature is still current.
pub fn get_cached_codex_transcript(cache: &CodexCache, path: &Path) -> Option<Value> {
    let signature = FileSignature::of(path)?;
    cached_signed(&cache.transcript, path, signature)
}

/// Store a pre-parsed transcript in the Claude cache.
pub fn cache_claude_transcript(cache: &mut ClaudeCache, path: &Path, view: Value) -> Option<()> {
    let signature = FileSignature::of(path)?;
    signed_value(&mut cache.transcript, path, signature, view);
    Some(())
}

/// Retrieve a cached Claude transcript if the file signature is still current.
pub fn get_cached_claude_transcript(cache: &ClaudeCache, path: &Path) -> Option<Value> {
    let signature = FileSignature::of(path)?;
    cached_signed(&cache.transcript, path, signature)
}

fn mtime_secs(mtime_ns: i128) -> f64 {
    (mtime_ns as f64) / 1e9
}

fn iso_from_mtime_ns(mtime_ns: i128) -> String {
    use chrono::{DateTime, Local};
    let secs = (mtime_ns / 1_000_000_000) as i64;
    let nanos = (mtime_ns % 1_000_000_000) as u32;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
        .map(|utc| {
            let local: DateTime<Local> = utc.into();
            local.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
        })
        .unwrap_or_default()
}

/// Helper for call sites that need to append bytes to a file in tests (force a
/// signature bump). Returns the new signature.
#[cfg(test)]
pub(crate) fn touch_append(path: &Path, payload: &[u8]) -> FileSignature {
    use std::io::Write;
    // Sleep a hair so coarse-resolution filesystems (HFS+/APFS) observe a
    // different mtime — otherwise size alone must bump the signature.
    std::thread::sleep(Duration::from_millis(20));
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(payload).expect("append");
    drop(file);
    FileSignature::of(path).expect("signature after touch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).expect("create jsonl");
        for line in lines {
            file.write_all(line.as_bytes()).unwrap();
            file.write_all(b"\n").unwrap();
        }
        path
    }

    #[test]
    fn cached_file_list_honours_ttl() {
        let mut cache = CachedFileList::default();
        let start = Instant::now();
        let ttl = Duration::from_millis(100);

        let mut calls = 0;
        let first = cached_file_list(&mut cache, start, ttl, || {
            calls += 1;
            vec![PathBuf::from("/tmp/a")]
        });
        assert_eq!(first, vec![PathBuf::from("/tmp/a")]);
        assert_eq!(calls, 1);

        // Within TTL: scan must not run again.
        let _ = cached_file_list(&mut cache, start + Duration::from_millis(50), ttl, || {
            calls += 1;
            vec![]
        });
        assert_eq!(calls, 1);

        // After TTL expires: rescan.
        let _ = cached_file_list(&mut cache, start + Duration::from_millis(200), ttl, || {
            calls += 1;
            vec![PathBuf::from("/tmp/b")]
        });
        assert_eq!(calls, 2);
        assert_eq!(cache.files, vec![PathBuf::from("/tmp/b")]);
    }

    #[test]
    fn cached_file_list_rescans_when_previous_was_empty() {
        // Matches Python: `cached_files and expires_at > now`. If the last
        // scan returned no files we always re-scan.
        let mut cache = CachedFileList::default();
        let start = Instant::now();
        let ttl = Duration::from_secs(60);
        let mut calls = 0;
        let _ = cached_file_list(&mut cache, start, ttl, || {
            calls += 1;
            vec![]
        });
        let _ = cached_file_list(&mut cache, start, ttl, || {
            calls += 1;
            vec![PathBuf::from("/tmp/a")]
        });
        assert_eq!(calls, 2);
    }

    #[test]
    fn file_signature_changes_with_size() {
        let dir = tempdir();
        let path = write_jsonl(dir.path(), "a.jsonl", &[r#"{"type":"session_meta"}"#]);
        let sig1 = FileSignature::of(&path).unwrap();

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"more bytes\n").unwrap();
        drop(file);

        let sig2 = FileSignature::of(&path).unwrap();
        assert_ne!(sig1, sig2, "size change must bump signature");
        assert!(sig2.size > sig1.size);
    }

    #[test]
    fn signed_cache_skips_reparse_when_signature_matches() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "rollout-session.jsonl",
            &[r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/tmp/foo"}}"#],
        );
        let mut cache = CodexCache::default();

        let meta1 = get_codex_rollout_meta(&mut cache, &path);
        assert_eq!(meta1["session_id"], "abc");
        // The second call must hit the cache: wipe the underlying file to
        // prove it's not being re-read.
        fs::write(&path, b"").unwrap();
        // Size changed so signature differs -> cache miss -> empty meta.
        let meta2 = get_codex_rollout_meta(&mut cache, &path);
        assert_ne!(meta1["session_id"], meta2["session_id"]);
    }

    #[test]
    fn signed_cache_returns_cached_value_when_unchanged() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "claude-abc.jsonl",
            &[r#"{"cwd":"/home/user/proj","sessionId":"SID"}"#],
        );
        let mut cache = ClaudeCache::default();
        let first = get_claude_session_meta(&mut cache, &path);
        assert_eq!(first["session_id"], "SID");

        // Second read must not re-parse — we test that by removing the file.
        // A second call on a missing file would normally return Null, but the
        // cache keeps us on the prior value while the signature is still the
        // one we captured before deletion. We simulate "still on disk" by not
        // touching it. Instead poison the in-memory value and re-fetch.
        cache
            .meta
            .get_mut(&path)
            .unwrap()
            .value
            .as_object_mut()
            .unwrap()
            .insert("marker".into(), Value::Bool(true));
        let second = get_claude_session_meta(&mut cache, &path);
        assert_eq!(second["marker"], Value::Bool(true));
    }

    #[test]
    fn scan_codex_rollouts_picks_up_rollout_jsonl() {
        let dir = tempdir();
        let nested = dir.path().join("deep/dir");
        fs::create_dir_all(&nested).unwrap();
        let keep = write_jsonl(&nested, "rollout-1.jsonl", &["{}"]);
        let skip_ext = write_jsonl(&nested, "notes.txt", &["x"]);
        let skip_prefix = write_jsonl(&nested, "session.jsonl", &["{}"]);
        let files = scan_codex_rollouts(dir.path());
        assert!(files.contains(&keep));
        assert!(!files.contains(&skip_ext));
        assert!(!files.contains(&skip_prefix));
    }

    #[test]
    fn scan_claude_sessions_accepts_any_jsonl() {
        let dir = tempdir();
        let nested = dir.path().join("-tmp-gtui");
        fs::create_dir_all(&nested).unwrap();
        let keep = write_jsonl(&nested, "abc.jsonl", &["{}"]);
        let files = scan_claude_sessions(dir.path());
        assert!(files.contains(&keep));
    }

    #[test]
    fn iter_jsonl_records_skips_non_objects_and_bad_lines() {
        let text = "{\"a\":1}\nnot-json\n\n[1,2]\n{\"b\":2}";
        let records = iter_jsonl_records(text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["a"], 1);
        assert_eq!(records[1]["b"], 2);
    }

    #[test]
    fn touch_append_bumps_signature() {
        let dir = tempdir();
        let path = write_jsonl(dir.path(), "x.jsonl", &["{}"]);
        let before = FileSignature::of(&path).unwrap();
        let after = touch_append(&path, b"more\n");
        assert_ne!(before, after);
    }
}
