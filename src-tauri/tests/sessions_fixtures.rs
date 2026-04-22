//! Integration tests for session scanning and metadata extraction.
//!
//! Builds a realistic on-disk session tree from fixture JSONL files and
//! exercises the same cache + parse helpers the snapshot store relies on.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gtui_lib::sessions::{
    cache_claude_transcript, cache_codex_transcript, cached_file_list,
    get_cached_claude_transcript, get_cached_codex_transcript, get_claude_session_meta,
    get_codex_rollout_meta, iter_jsonl_records, list_recent_claude_sessions,
    list_recent_codex_rollouts, scan_claude_sessions, scan_codex_rollouts, CachedFileList,
    ClaudeCache, CodexCache, FileSignature,
};
use serde_json::json;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn write_fixture_to(dir: &std::path::Path, fixture: &str, dest_name: &str) -> PathBuf {
    let body = common::load_fixture(fixture);
    let dest = dir.join(dest_name);
    fs::write(&dest, body).expect("write fixture");
    dest
}

#[test]
fn codex_rollout_meta_extracts_cwd_and_session_id() {
    let dir = tempdir();
    let nested = dir.path().join("2026/04/21");
    fs::create_dir_all(&nested).unwrap();
    let rollout = write_fixture_to(&nested, "codex_rollout.jsonl", "rollout-abc.jsonl");

    let mut cache = CodexCache::default();
    let meta = get_codex_rollout_meta(&mut cache, &rollout);
    assert_eq!(meta["session_id"], "01HZSESSION0000000000000001");
    // cwd goes through normalize_path_value — for a non-existent path we get
    // the POSIX normpath back, which is the same string here.
    assert!(
        meta["cwd"]
            .as_str()
            .unwrap_or("")
            .ends_with("polecats/rictus"),
        "unexpected cwd: {:?}",
        meta["cwd"]
    );
    assert!(meta["modified_at"].as_str().unwrap_or("").contains('T'));
}

#[test]
fn codex_rollout_meta_falls_back_to_turn_context_when_no_session_meta() {
    let dir = tempdir();
    let rollout = write_fixture_to(
        dir.path(),
        "codex_rollout_turn_only.jsonl",
        "rollout-nosm.jsonl",
    );
    let mut cache = CodexCache::default();
    let meta = get_codex_rollout_meta(&mut cache, &rollout);
    // session_id is empty because no session_meta record was present.
    assert_eq!(meta["session_id"], "");
    // But cwd must be populated from the turn_context fallback.
    assert!(
        meta["cwd"].as_str().unwrap_or("").ends_with("gastown"),
        "unexpected cwd: {:?}",
        meta["cwd"]
    );
}

#[test]
fn claude_session_meta_picks_up_cwd_and_session_id() {
    let dir = tempdir();
    let nested = dir.path().join("-home-user-gt-gtui-polecats-rictus");
    fs::create_dir_all(&nested).unwrap();
    let session = write_fixture_to(
        &nested,
        "claude_session.jsonl",
        "55505d27-1dce-4761-b085-dd1cb3dced97.jsonl",
    );

    let mut cache = ClaudeCache::default();
    let meta = get_claude_session_meta(&mut cache, &session);
    assert_eq!(meta["session_id"], "55505d27-1dce-4761-b085-dd1cb3dced97");
    assert!(
        meta["cwd"]
            .as_str()
            .unwrap_or("")
            .contains("polecats/rictus"),
        "unexpected cwd: {:?}",
        meta["cwd"]
    );
}

#[test]
fn claude_session_meta_uses_filename_stem_when_record_lacks_session_id() {
    let dir = tempdir();
    let session = write_fixture_to(
        dir.path(),
        "claude_session_no_cwd.jsonl",
        "fallback-stem.jsonl",
    );
    let mut cache = ClaudeCache::default();
    let meta = get_claude_session_meta(&mut cache, &session);
    // First record in the fixture has no sessionId; eventually one with
    // "abc-123" appears, so the record wins over the stem.
    assert_eq!(meta["session_id"], "abc-123");
}

#[test]
fn scan_codex_rollouts_sorts_newest_first() {
    let dir = tempdir();
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    let older = write_fixture_to(
        &dir.path().join("a"),
        "codex_rollout.jsonl",
        "rollout-old.jsonl",
    );
    // Give older an older mtime, then write the new one.
    std::thread::sleep(Duration::from_millis(25));
    let newer = write_fixture_to(
        &dir.path().join("b"),
        "codex_rollout.jsonl",
        "rollout-new.jsonl",
    );

    let files = scan_codex_rollouts(dir.path());
    assert!(files.contains(&newer) && files.contains(&older));
    let idx_new = files.iter().position(|p| p == &newer).unwrap();
    let idx_old = files.iter().position(|p| p == &older).unwrap();
    assert!(idx_new < idx_old, "expected newest first, got {files:?}");
}

#[test]
fn list_recent_codex_rollouts_uses_cache_then_rescans_after_ttl() {
    let dir = tempdir();
    let mut cache = CodexCache::default();

    let initial_files = list_recent_codex_rollouts(&mut cache, dir.path(), Instant::now());
    assert!(initial_files.is_empty());

    // Add a rollout and call again within TTL. Because the previous result
    // was empty, the cache rule says "rescan on next call".
    let created = write_fixture_to(dir.path(), "codex_rollout.jsonl", "rollout-t.jsonl");
    let next = list_recent_codex_rollouts(&mut cache, dir.path(), Instant::now());
    assert!(next.contains(&created));
}

#[test]
fn list_recent_claude_sessions_reuses_cache_within_ttl() {
    let dir = tempdir();
    let mut cache = ClaudeCache::default();
    // Prime with one session.
    let original = write_fixture_to(dir.path(), "claude_session.jsonl", "session-a.jsonl");
    let first = list_recent_claude_sessions(&mut cache, dir.path(), Instant::now());
    assert!(first.contains(&original));

    // Add another session but query immediately — within the 3s TTL we must
    // get the cached (stale) list back, without the new file.
    let _late = write_fixture_to(dir.path(), "claude_session.jsonl", "session-b.jsonl");
    let second = list_recent_claude_sessions(&mut cache, dir.path(), Instant::now());
    assert_eq!(second, first, "expected cached list inside TTL");
}

#[test]
fn scan_claude_sessions_ignores_non_jsonl_files() {
    let dir = tempdir();
    fs::write(dir.path().join("notes.md"), "not a session").unwrap();
    fs::write(dir.path().join("bogus.txt"), "nope").unwrap();
    let kept = write_fixture_to(dir.path(), "claude_session.jsonl", "keep.jsonl");
    let files = scan_claude_sessions(dir.path());
    assert_eq!(files, vec![kept]);
}

#[test]
fn iter_jsonl_records_handles_fixture_with_bad_lines() {
    let text = common::load_fixture("claude_session_no_cwd.jsonl");
    let records = iter_jsonl_records(&text);
    // Fixture has 3 lines: 1 object, 1 non-json, 1 object. Expect 2 records.
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["type"], "assistant");
    assert_eq!(records[1]["sessionId"], "abc-123");
}

#[test]
fn transcript_cache_round_trips_for_both_agents() {
    let dir = tempdir();
    let codex_file = write_fixture_to(dir.path(), "codex_rollout.jsonl", "rollout-c.jsonl");
    let claude_file = write_fixture_to(dir.path(), "claude_session.jsonl", "c.jsonl");

    let mut codex_cache = CodexCache::default();
    let mut claude_cache = ClaudeCache::default();
    assert!(cache_codex_transcript(&mut codex_cache, &codex_file, json!({"records": 4})).is_some());
    assert!(
        cache_claude_transcript(&mut claude_cache, &claude_file, json!({"records": 2})).is_some()
    );

    let codex_hit = get_cached_codex_transcript(&codex_cache, &codex_file).expect("codex hit");
    let claude_hit = get_cached_claude_transcript(&claude_cache, &claude_file).expect("claude hit");
    assert_eq!(codex_hit["records"], 4);
    assert_eq!(claude_hit["records"], 2);

    // Mutating the file must invalidate the transcript cache.
    fs::write(&codex_file, b"{}\n").unwrap();
    assert!(get_cached_codex_transcript(&codex_cache, &codex_file).is_none());
}

#[test]
fn file_signature_missing_returns_none() {
    let missing = PathBuf::from("/tmp/definitely-not-here/gtui-test-404.jsonl");
    assert!(FileSignature::of(&missing).is_none());
}

#[test]
fn cached_file_list_scans_only_once_within_ttl() {
    let mut cache = CachedFileList::default();
    let ttl = Duration::from_secs(30);
    let now = Instant::now();
    let mut invocations = 0usize;
    let scan = |i: &mut usize| {
        *i += 1;
        vec![PathBuf::from("/tmp/only-once")]
    };
    let first = cached_file_list(&mut cache, now, ttl, || scan(&mut invocations));
    assert_eq!(first.len(), 1);
    assert_eq!(invocations, 1);
    // Second call inside TTL: the closure MUST NOT run.
    let second = cached_file_list(&mut cache, now + Duration::from_millis(5), ttl, || {
        panic!("closure should not have been invoked within TTL");
    });
    assert_eq!(second.len(), 1);
}
