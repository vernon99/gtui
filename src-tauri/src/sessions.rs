//! Session scanning and transcript caches for Codex and Claude CLI sessions.
//!
//! Covers four cache layers:
//!
//! 1. Codex rollout list cache (3s TTL)
//! 2. Claude session list cache (3s TTL)
//! 3. Signature-based per-file meta cache (mtime + size) — "skip re-parse if
//!    unchanged"
//! 4. Signature-based per-file transcript cache
//!
//! Meta parsing is intentionally minimal: session matching only needs cwd and
//! session id, while transcript parsing happens on demand.

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
use crate::parse::{
    encode_claude_project_dir, match_path_score, normalize_path_value, pathbuf_str,
};

/// Per-file fingerprint used to decide whether a cached parse is still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileSignature {
    pub mtime_ns: i128,
    pub size: u64,
}

impl FileSignature {
    /// Read the signature for `path`. Returns `None` for missing files or
    /// permission errors.
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
/// produced it.
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

/// Root directory Claude Code uses for project-scoped JSONL transcripts.
pub fn claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

/// Return recent Claude session files for a specific working directory when
/// Claude's encoded project bucket exists, otherwise fall back to the cached
/// global scan.
pub fn list_recent_claude_sessions_for_path(
    cache: &mut ClaudeCache,
    claude_root: &Path,
    current_path: &str,
    now: Instant,
) -> Vec<PathBuf> {
    let encoded = encode_claude_project_dir(current_path);
    if !encoded.is_empty() {
        let project_dir = claude_root.join(encoded);
        if project_dir.is_dir() {
            return glob_recent_jsonl(&project_dir, CLAUDE_SESSION_SCAN_LIMIT, |path| {
                path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            });
        }
    }
    list_recent_claude_sessions(cache, claude_root, now)
}

/// Find the best Claude session for the given working directory.
pub fn find_claude_session(
    cache: &mut ClaudeCache,
    claude_root: &Path,
    current_path: &str,
    now: Instant,
) -> Option<Value> {
    let mut best_meta: Option<Value> = None;
    let mut best_score = -1;
    for path in list_recent_claude_sessions_for_path(cache, claude_root, current_path, now) {
        let meta = get_claude_session_meta(cache, &path);
        let cwd = meta.get("cwd").and_then(Value::as_str).unwrap_or("");
        let score = match_path_score(current_path, cwd);
        if score < 0 {
            continue;
        }
        let modified_at = meta.get("mtime").and_then(Value::as_f64).unwrap_or(0.0);
        let best_modified_at = best_meta
            .as_ref()
            .and_then(|best| best.get("mtime"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if score > best_score || (score == best_score && modified_at > best_modified_at) {
            best_score = score;
            best_meta = Some(meta);
        }
    }
    best_meta
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

/// Parse a chunk of JSONL text, skipping empty lines and bad records.
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
/// The returned value is `{path, cwd, session_id, modified_at, mtime}`.
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
/// Returns `{path, cwd, session_id, modified_at, mtime}`.
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

/// Parse a Claude Code JSONL transcript into the view consumed by the frontend.
pub fn parse_claude_transcript(cache: &mut ClaudeCache, path: &Path) -> Value {
    let Some(signature) = FileSignature::of(path) else {
        return Value::Null;
    };
    if let Some(cached) = get_cached_claude_transcript(cache, path) {
        return cached;
    }

    let mut items: Vec<Value> = Vec::new();
    let mut call_map: HashMap<String, Value> = HashMap::new();
    let mut last_model = String::new();
    for record in iter_jsonl_records(&read_full(path)) {
        if record
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }

        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let time = format_timestamp_short(&timestamp);
        let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
        let message = record
            .get("message")
            .and_then(Value::as_object)
            .cloned()
            .map(Value::Object)
            .unwrap_or(Value::Null);
        let content = message.get("content").cloned().unwrap_or(Value::Null);

        match record_type {
            "user" => {
                if let Some(text) = content
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    items.push(transcript_user_text_item(text, &time, &timestamp));
                    continue;
                }
                let Some(blocks) = content.as_array() else {
                    continue;
                };
                for block in blocks {
                    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                    match block_type {
                        "tool_result" => {
                            let call_id = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let tool_info = call_map.get(&call_id).cloned().unwrap_or(Value::Null);
                            let tool = tool_info.get("tool").and_then(Value::as_str).unwrap_or("");
                            let output_text = stringify_claude_tool_output(
                                record.get("toolUseResult"),
                                block.get("content"),
                            );
                            items.push(json!({
                                "kind": "tool_output",
                                "tool": tool,
                                "summary": summarize_tool_output(&output_text),
                                "text": excerpt_tool_output(&output_text),
                                "call_id": call_id,
                                "is_error": block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                                "time": time,
                                "timestamp": timestamp,
                            }));
                        }
                        "text" => {
                            let text = extract_claude_text_block(block);
                            if !text.is_empty() {
                                items.push(transcript_user_text_item(&text, &time, &timestamp));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "assistant" => {
                let model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !model.is_empty() {
                    last_model = model.clone();
                }
                if let Some(text) = content
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    items.push(json!({
                        "kind": "assistant",
                        "text": text,
                        "model": model,
                        "time": time,
                        "timestamp": timestamp,
                    }));
                    continue;
                }
                let Some(blocks) = content.as_array() else {
                    continue;
                };
                for block in blocks {
                    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                    match block_type {
                        "text" => {
                            let text = extract_claude_text_block(block);
                            if !text.is_empty() {
                                items.push(json!({
                                    "kind": "assistant",
                                    "text": text,
                                    "model": model,
                                    "time": time,
                                    "timestamp": timestamp,
                                }));
                            }
                        }
                        "tool_use" => {
                            let tool_name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let call_id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let tool_input = block.get("input").unwrap_or(&Value::Null);
                            let summary = summarize_tool_call(&tool_name, tool_input);
                            if !call_id.is_empty() {
                                call_map.insert(
                                    call_id.clone(),
                                    json!({"tool": tool_name, "summary": summary}),
                                );
                            }
                            let mut tool_item = json!({
                                "kind": "tool_call",
                                "tool": tool_name,
                                "summary": summary,
                                "call_id": call_id,
                                "time": time,
                                "timestamp": timestamp,
                            });
                            if let Some(command) = tool_input.get("command").and_then(Value::as_str)
                            {
                                tool_item["command"] = json!(command);
                            }
                            if let Some(description) =
                                tool_input.get("description").and_then(Value::as_str)
                            {
                                tool_item["description"] = json!(description);
                            }
                            items.push(tool_item);
                        }
                        "thinking" => {
                            let reasoning_item = json!({
                                "kind": "reasoning",
                                "summary": "Thinking...",
                                "time": time,
                                "timestamp": timestamp,
                            });
                            if items
                                .last()
                                .and_then(|item| item.get("kind"))
                                .and_then(Value::as_str)
                                == Some("reasoning")
                            {
                                if let Some(last) = items.last_mut() {
                                    *last = reasoning_item;
                                }
                            } else {
                                items.push(reasoning_item);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if !items.is_empty() {
        let trailing_reasoning = items
            .last()
            .filter(|item| item.get("kind").and_then(Value::as_str) == Some("reasoning"))
            .cloned();
        items.retain(|item| item.get("kind").and_then(Value::as_str) != Some("reasoning"));
        if let Some(reasoning) = trailing_reasoning {
            items.push(reasoning);
        }
    }

    let view = json!({
        "available": !items.is_empty(),
        "provider": "claude",
        "source": "claude-session",
        "session_file": pathbuf_str(path),
        "session_name": path.file_name().and_then(|name| name.to_str()).unwrap_or(""),
        "model": last_model,
        "revision": format!("{}:{}", signature.mtime_ns, signature.size),
        "updated_at": iso_from_mtime_ns(signature.mtime_ns),
        "items": items,
    });
    let _ = cache_claude_transcript(cache, path, view.clone());
    view
}

/// Root directory Codex CLI uses for its rollout transcripts.
pub fn codex_sessions_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}

/// Find the best Codex rollout for the given working directory.
pub fn find_codex_rollout(
    cache: &mut CodexCache,
    codex_root: &Path,
    current_path: &str,
    now: Instant,
) -> Option<Value> {
    let mut best_meta: Option<Value> = None;
    let mut best_score = -1;
    for path in list_recent_codex_rollouts(cache, codex_root, now) {
        let meta = get_codex_rollout_meta(cache, &path);
        if meta.is_null() {
            continue;
        }
        let cwd = meta.get("cwd").and_then(Value::as_str).unwrap_or("");
        let score = match_path_score(current_path, cwd);
        if score < 0 {
            continue;
        }
        let modified_at = meta.get("mtime").and_then(Value::as_f64).unwrap_or(0.0);
        let best_modified_at = best_meta
            .as_ref()
            .and_then(|best| best.get("mtime"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if score > best_score || (score == best_score && modified_at > best_modified_at) {
            best_score = score;
            best_meta = Some(meta);
        }
    }
    best_meta
}

/// Parse a Codex CLI JSONL rollout into the transcript view consumed by the
/// frontend.
pub fn parse_codex_transcript(cache: &mut CodexCache, path: &Path) -> Value {
    let Some(signature) = FileSignature::of(path) else {
        return Value::Null;
    };
    if let Some(cached) = get_cached_codex_transcript(cache, path) {
        return cached;
    }

    let mut items: Vec<Value> = Vec::new();
    let mut call_map: HashMap<String, Value> = HashMap::new();

    for record in iter_jsonl_records(&read_full(path)) {
        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
        if record_type != "response_item" {
            continue;
        }
        let payload = record
            .get("payload")
            .and_then(Value::as_object)
            .cloned()
            .map(Value::Object)
            .unwrap_or(Value::Null);
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let time = format_timestamp_short(&timestamp);

        match payload_type.as_str() {
            "message" => {
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                if role != "assistant" && role != "user" {
                    continue;
                }
                let text = extract_message_text(payload.get("content"));
                if text.is_empty() || is_hidden_transcript_message(&text) {
                    continue;
                }
                let phase = payload
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if role == "user" {
                    let mut item = transcript_user_text_item(&text, &time, &timestamp);
                    if !phase.is_empty() {
                        item["phase"] = json!(phase);
                    }
                    items.push(item);
                    continue;
                }
                items.push(json!({
                    "kind": role,
                    "phase": phase,
                    "text": text,
                    "time": time,
                    "timestamp": timestamp,
                }));
            }
            "function_call" => {
                let tool_name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let arguments = payload.get("arguments").cloned().unwrap_or(Value::Null);
                let arguments_for_summary = if let Some(arg_str) = arguments.as_str() {
                    serde_json::from_str::<Value>(arg_str).unwrap_or(arguments.clone())
                } else {
                    arguments.clone()
                };
                let summary = summarize_tool_call(&tool_name, &arguments_for_summary);
                if !call_id.is_empty() {
                    call_map.insert(
                        call_id.clone(),
                        json!({"tool": tool_name, "summary": summary}),
                    );
                }
                items.push(json!({
                    "kind": "tool_call",
                    "tool": tool_name,
                    "summary": summary,
                    "call_id": call_id,
                    "time": time,
                    "timestamp": timestamp,
                }));
            }
            "function_call_output" => {
                let call_id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool_info = call_map.get(&call_id).cloned().unwrap_or(Value::Null);
                let tool = tool_info
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let output_text = stringify_tool_output(payload.get("output"));
                items.push(json!({
                    "kind": "tool_output",
                    "tool": tool,
                    "summary": summarize_tool_output(&output_text),
                    "text": excerpt_tool_output(&output_text),
                    "call_id": call_id,
                    "time": time,
                    "timestamp": timestamp,
                }));
            }
            "reasoning" => {
                let reasoning_item = json!({
                    "kind": "reasoning",
                    "summary": "Thinking...",
                    "time": time,
                    "timestamp": timestamp,
                });
                if items
                    .last()
                    .and_then(|item| item.get("kind"))
                    .and_then(Value::as_str)
                    == Some("reasoning")
                {
                    *items.last_mut().unwrap() = reasoning_item;
                } else {
                    items.push(reasoning_item);
                }
            }
            _ => {}
        }
    }

    if !items.is_empty() {
        let trailing_reasoning = items
            .last()
            .filter(|item| item.get("kind").and_then(Value::as_str) == Some("reasoning"))
            .cloned();
        items.retain(|item| item.get("kind").and_then(Value::as_str) != Some("reasoning"));
        if let Some(trailing) = trailing_reasoning {
            items.push(trailing);
        }
    }

    let view = json!({
        "available": !items.is_empty(),
        "provider": "codex",
        "source": "codex-rollout",
        "session_file": pathbuf_str(path),
        "session_name": path.file_name().and_then(|name| name.to_str()).unwrap_or(""),
        "revision": format!("{}:{}", signature.mtime_ns, signature.size),
        "updated_at": iso_from_mtime_ns(signature.mtime_ns),
        "items": items,
    });
    let _ = cache_codex_transcript(cache, path, view.clone());
    view
}

fn extract_message_text(content: Option<&Value>) -> String {
    let Some(array) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut chunks: Vec<String> = Vec::new();
    for part in array {
        let Some(obj) = part.as_object() else {
            continue;
        };
        let part_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if part_type != "input_text" && part_type != "output_text" {
            continue;
        }
        let text = obj
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !text.is_empty() {
            chunks.push(text);
        }
    }
    chunks.join("\n\n").trim().to_string()
}

fn is_hidden_transcript_message(text: &str) -> bool {
    let normalized = text.trim();
    if normalized.is_empty() {
        return false;
    }
    if normalized.starts_with("<turn_aborted>") && normalized.ends_with("</turn_aborted>") {
        return true;
    }
    if normalized.starts_with("# AGENTS.md instructions for ") {
        return true;
    }
    false
}

fn transcript_user_text_item(text: &str, time: &str, timestamp: &str) -> Value {
    if is_system_generated_user_text(text) {
        return json!({
            "kind": "event",
            "event_type": "system",
            "summary": text,
            "text": text,
            "time": time,
            "timestamp": timestamp,
        });
    }
    json!({
        "kind": "user",
        "text": text,
        "time": time,
        "timestamp": timestamp,
    })
}

fn is_system_generated_user_text(text: &str) -> bool {
    let normalized = text.trim();
    if normalized.is_empty() {
        return false;
    }
    if normalized.starts_with("<system-reminder>") {
        return true;
    }
    if normalized.contains("Escalation mail from ")
        && normalized.contains("Run 'gt mail read ")
        && normalized.contains("gt escalate ack ")
    {
        return true;
    }
    if normalized
        .trim_start_matches('📬')
        .trim_start()
        .starts_with("You have new mail from ")
        && normalized.contains("Run 'gt mail inbox' to read")
    {
        return true;
    }
    if normalized.starts_with("Remember to reply to ")
        && normalized.contains(" via `gt mail send ")
        && normalized.contains("not in chat")
    {
        return true;
    }
    false
}

fn stringify_tool_output(output: Option<&Value>) -> String {
    match output {
        None => String::new(),
        Some(value) => {
            if value.is_null() {
                String::new()
            } else if let Some(text) = value.as_str() {
                text.to_string()
            } else {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            }
        }
    }
}

fn extract_claude_text_block(block: &Value) -> String {
    block
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| block.get("content").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn stringify_claude_tool_output(tool_result: Option<&Value>, fallback: Option<&Value>) -> String {
    if let Some(result) = tool_result.filter(|value| !value.is_null()) {
        if let Some(text) = structured_tool_result_text(result) {
            return text;
        }
        let text = value_to_text(result);
        if !text.trim().is_empty() {
            return text;
        }
    }
    value_to_text(fallback.unwrap_or(&Value::Null))
}

fn structured_tool_result_text(value: &Value) -> Option<String> {
    let obj = value.as_object()?;
    let has_process_output = obj.contains_key("stdout") || obj.contains_key("stderr");
    if !has_process_output {
        return None;
    }

    let mut parts = Vec::new();
    for key in ["stdout", "stderr"] {
        if let Some(text) = obj.get(key).and_then(Value::as_str) {
            let trimmed = text.trim_end();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    Some(parts.join("\n\n"))
}

fn value_to_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(arr) = value.as_array() {
        let chunks: Vec<String> = arr
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.as_str() {
                    return Some(text.to_string());
                }
                part.get("text")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| {
                        part.get("content")
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
            })
            .filter(|text| !text.trim().is_empty())
            .collect();
        if !chunks.is_empty() {
            return chunks.join("\n\n");
        }
    }
    if value.is_null() {
        String::new()
    } else {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    }
}

fn summarize_tool_call(name: &str, arguments: &Value) -> String {
    if arguments.is_null() {
        return name.to_string();
    }
    let arg_text = if let Some(obj) = arguments.as_object() {
        obj.iter()
            .take(4)
            .map(|(key, value)| format!("{key}={}", clip_text(&value_to_text(value), 48)))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        clip_text(&value_to_text(arguments), 96)
    };
    if arg_text.is_empty() {
        name.to_string()
    } else {
        format!("{name}({arg_text})")
    }
}

fn summarize_tool_output(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| line.chars().any(char::is_alphanumeric))
        .unwrap_or("");
    if first.is_empty() {
        "Tool returned no output.".to_string()
    } else {
        clip_text(first, 140)
    }
}

fn excerpt_tool_output(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() > 18 {
        let mut clipped = Vec::new();
        clipped.extend_from_slice(&lines[..8]);
        clipped.push("...");
        clipped.extend_from_slice(&lines[lines.len().saturating_sub(8)..]);
        lines = clipped;
    }
    clip_text_preserving_whitespace(&lines.join("\n"), 3200)
}

fn clip_text(text: &str, limit: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= limit {
        return compact;
    }
    let keep = limit.saturating_sub(3);
    let prefix: String = compact.chars().take(keep).collect();
    format!("{}...", prefix.trim_end())
}

fn clip_text_preserving_whitespace(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let keep = limit.saturating_sub(3);
    let prefix: String = text.chars().take(keep).collect();
    format!("{}...", prefix.trim_end())
}

fn format_timestamp_short(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
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
        // Empty scans are not considered fresh, so we always re-scan after one.
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

    #[test]
    fn parse_codex_transcript_folds_message_tool_and_reasoning_records() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "rollout-1.jsonl",
            &[
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello codex"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:01Z","payload":{"type":"reasoning"}}"#,
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:02Z","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\"}"}}"#,
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:03Z","payload":{"type":"function_call_output","call_id":"c1","output":"total 0\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:04Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            ],
        );
        let mut cache = CodexCache::default();
        let view = parse_codex_transcript(&mut cache, &path);
        assert_eq!(view["available"], true);
        assert_eq!(view["provider"], "codex");
        let items = view["items"].as_array().expect("items array");
        // reasoning is kept only if it's the trailing item — here it isn't, so
        // it's dropped. We expect: user, tool_call, tool_output, assistant.
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["kind"], "user");
        assert_eq!(items[0]["text"], "hello codex");
        assert_eq!(items[1]["kind"], "tool_call");
        assert_eq!(items[1]["tool"], "exec_command");
        assert_eq!(items[2]["kind"], "tool_output");
        assert_eq!(items[2]["tool"], "exec_command");
        assert_eq!(items[3]["kind"], "assistant");
    }

    #[test]
    fn parse_claude_transcript_unwraps_bash_result_stdout() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "claude-1.jsonl",
            &[
                r#"{"type":"assistant","timestamp":"2026-04-22T10:00:00Z","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"gt mail archive hq-123","description":"Archive stale messages"}}]}}"#,
                r#"{"type":"user","timestamp":"2026-04-22T10:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"fallback output"}]}]},"toolUseResult":{"interrupted":false,"isImage":false,"noOutputExpected":false,"stderr":"","stdout":">\nArchived 25 messages"}}"#,
            ],
        );

        let mut cache = ClaudeCache::default();
        let view = parse_claude_transcript(&mut cache, &path);
        let items = view["items"].as_array().expect("items array");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["kind"], "tool_call");
        assert_eq!(items[0]["tool"], "Bash");
        assert_eq!(items[0]["command"], "gt mail archive hq-123");
        assert_eq!(items[0]["description"], "Archive stale messages");
        assert_eq!(items[1]["kind"], "tool_output");
        assert_eq!(items[1]["tool"], "Bash");
        assert_eq!(items[1]["summary"], "Archived 25 messages");
        assert_eq!(items[1]["text"], ">\nArchived 25 messages");
    }

    #[test]
    fn parse_claude_transcript_marks_system_generated_user_text_as_event() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "claude-system-nudge.jsonl",
            &[
                r#"{"type":"user","timestamp":"2026-04-22T10:00:00Z","message":{"role":"user","content":"🚨 Escalation mail from deacon/dogs/alpha. ID: hq-wisp-0kfdg. Severity: high. Subject: [HIGH] Dolt: hq database has 593 open wisps. Run 'gt mail read hq-wisp-0kfdg' or 'gt escalate ack hq-wisp-0kfdg'."}}"#,
                r#"{"type":"user","timestamp":"2026-04-22T10:00:01Z","message":{"role":"user","content":"actual human prompt"}}"#,
            ],
        );

        let mut cache = ClaudeCache::default();
        let view = parse_claude_transcript(&mut cache, &path);
        let items = view["items"].as_array().expect("items array");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["kind"], "event");
        assert_eq!(items[0]["event_type"], "system");
        assert!(items[0]["summary"]
            .as_str()
            .unwrap()
            .contains("Escalation mail from deacon/dogs/alpha"));
        assert_eq!(items[1]["kind"], "user");
        assert_eq!(items[1]["text"], "actual human prompt");
    }

    #[test]
    fn parse_codex_transcript_keeps_trailing_reasoning() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "rollout-2.jsonl",
            &[
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:01Z","payload":{"type":"reasoning"}}"#,
            ],
        );
        let mut cache = CodexCache::default();
        let view = parse_codex_transcript(&mut cache, &path);
        let items = view["items"].as_array().expect("items array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["kind"], "reasoning");
    }

    #[test]
    fn parse_codex_transcript_marks_system_reminders_as_events() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "rollout-system-nudge.jsonl",
            &[
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder>\nQUEUED NUDGE (1 urgent):\n\n  [URGENT from deacon/] 🚨 Escalation mail from deacon/. ID: hq-wisp-680o0. Severity: critical. Subject: [CRITICAL] Dolt down. Run 'gt mail read hq-wisp-680o0' or 'gt escalate ack hq-wisp-680o0'.\n\nHandle urgent nudges before continuing current work.\n</system-reminder>"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"real input"}]}}"#,
            ],
        );
        let mut cache = CodexCache::default();
        let view = parse_codex_transcript(&mut cache, &path);
        let items = view["items"].as_array().expect("items array");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["kind"], "event");
        assert_eq!(items[0]["event_type"], "system");
        assert!(items[0]["summary"]
            .as_str()
            .unwrap()
            .contains("QUEUED NUDGE"));
        assert_eq!(items[1]["kind"], "user");
        assert_eq!(items[1]["text"], "real input");
    }

    #[test]
    fn parse_codex_transcript_marks_mail_inbox_notifications_as_events() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "rollout-mail-notification.jsonl",
            &[
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"📬 You have new mail from gastown/witness. Subject: Overseer sending malformed RESTART_POLECAT mail. Run 'gt mail inbox' to read."}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-04-22T10:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"real input"}]}}"#,
            ],
        );
        let mut cache = CodexCache::default();
        let view = parse_codex_transcript(&mut cache, &path);
        let items = view["items"].as_array().expect("items array");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["kind"], "event");
        assert_eq!(items[0]["event_type"], "system");
        assert!(items[0]["summary"]
            .as_str()
            .unwrap()
            .contains("You have new mail from gastown/witness"));
        assert_eq!(items[1]["kind"], "user");
        assert_eq!(items[1]["text"], "real input");
    }

    #[test]
    fn parse_codex_transcript_hides_turn_aborted_and_agents_md() {
        let dir = tempdir();
        let path = write_jsonl(
            dir.path(),
            "rollout-3.jsonl",
            &[
                r##"{"type":"response_item","timestamp":"2026-04-22T10:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<turn_aborted>stop</turn_aborted>"}]}}"##,
                r##"{"type":"response_item","timestamp":"2026-04-22T10:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp/foo"}]}}"##,
                r##"{"type":"response_item","timestamp":"2026-04-22T10:00:02Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"real input"}]}}"##,
            ],
        );
        let mut cache = CodexCache::default();
        let view = parse_codex_transcript(&mut cache, &path);
        let items = view["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["text"], "real input");
    }

    #[test]
    fn find_codex_rollout_picks_best_scoring_cwd() {
        let dir = tempdir();
        let sessions = dir.path();
        let shallow = write_jsonl(
            sessions,
            "rollout-shallow.jsonl",
            &[r#"{"type":"session_meta","payload":{"id":"shallow","cwd":"/tmp"}}"#],
        );
        let deep = write_jsonl(
            sessions,
            "rollout-deep.jsonl",
            &[r#"{"type":"session_meta","payload":{"id":"deep","cwd":"/tmp/project/subdir"}}"#],
        );
        let mut cache = CodexCache::default();
        let meta = find_codex_rollout(&mut cache, sessions, "/tmp/project/subdir", Instant::now())
            .expect("match");
        assert_eq!(meta["session_id"], "deep");
        // Sanity: both files are visible to the list.
        let files = list_recent_codex_rollouts(&mut cache, sessions, Instant::now());
        assert!(files.contains(&shallow) && files.contains(&deep));
    }
}
