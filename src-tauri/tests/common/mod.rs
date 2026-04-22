//! Shared helpers for integration tests.
//!
//! Loads fixture files from `tests/fixtures/` by name. Integration tests live
//! in separate crates in Cargo, so this module is pulled in via
//! `mod common;` from each `tests/*.rs` entry point. That also means each
//! consumer crate compiles this file independently; helpers it doesn't call
//! show up as `dead_code` there, hence `#[allow(dead_code)]`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

pub fn load_fixture(name: &str) -> String {
    let path = fixtures_dir().join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to load fixture {name}: {err} (at {path:?})"))
}

pub fn load_fixture_json(name: &str) -> serde_json::Value {
    let text = load_fixture(name);
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("fixture {name} is not valid JSON: {err}"))
}
