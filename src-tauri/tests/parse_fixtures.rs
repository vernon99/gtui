//! Integration tests that drive the parsers against recorded `gt` CLI output.
//!
//! The fixtures under `tests/fixtures/` are hand-curated samples matching the
//! shape of real `gt status --fast`, `gt feed --plain`, and `gt crew list
//! --all --json` responses. If the parsers ever drift from local HTTP server
//! these tests are the first line of defence.

mod common;

use gtui_lib::parse::{parse_feed, parse_services, parse_status_summary};

#[test]
fn parse_status_fixture_extracts_every_field() {
    let text = common::load_fixture("gt_status_fast.txt");
    let summary = parse_status_summary(&text);
    assert_eq!(summary.town, "gastown");
    assert_eq!(summary.root_path, "/home/user/gt");
    assert_eq!(summary.overseer, "mayor");
    assert_eq!(summary.tmux_socket, "gastown");
    assert_eq!(
        summary.services,
        vec![
            "daemon (running)".to_string(),
            "witness (running)".to_string(),
            "dolt (running)".to_string(),
        ]
    );
    assert!(summary.raw.contains("Town: gastown"));
}

#[test]
fn parse_status_fixture_flags_stopped_daemon() {
    let text = common::load_fixture("gt_status_daemon_stopped.txt");
    let services = parse_services(&text);
    assert!(services.iter().any(|s| s == "daemon (stopped)"));
}

#[test]
fn parse_feed_fixture_separates_structured_from_raw() {
    let text = common::load_fixture("gt_feed.txt");
    let events = parse_feed(&text);
    // 5 structured lines + 1 raw line = 6 events.
    assert_eq!(events.len(), 6);
    assert_eq!(events[0]["actor"], "gtui/polecats/furiosa");
    assert_eq!(events[0]["symbol"], "◐");
    assert_eq!(events[0]["message"], "started gui-bn8.4");
    // The unstructured line is preserved verbatim with empty actor/symbol.
    assert!(events
        .iter()
        .any(|e| e["actor"] == "" && e["message"] == "some unstructured line from the feed"));
}

#[test]
fn crew_list_fixture_parses_as_array() {
    let value = common::load_fixture_json("gt_crew_list.json");
    let crews = value.as_array().expect("array");
    assert_eq!(crews.len(), 3);
    let risky = crews
        .iter()
        .filter(|c| {
            c.get("git_has_risky_changes")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .count();
    assert_eq!(risky, 2);
}
