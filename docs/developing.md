# Developing GTUI

This guide covers the day-to-day tasks of working on the Tauri port: adding
backend commands, modifying the frontend, running tests, and understanding the
module layout.

## Project layout

```
gtui/
├── src-tauri/   # Rust backend (Tauri host process)
├── src/         # Frontend (static SPA loaded by the WebView)
├── webui/       # Legacy Python implementation
└── docs/
```

See the [root README](../README.md) for a module-level map of `src-tauri/src/`.

## Running the app

```bash
cd src-tauri
cargo tauri dev     # development with hot reload
cargo tauri build   # release build + runnable app bundle
```

Both commands read `src-tauri/tauri.conf.json`. `frontendDist` points at `../src`,
so the frontend is served directly from disk — no bundler is needed.

## Adding a Tauri command

All IPC entry points live in [`src-tauri/src/ipc.rs`](../src-tauri/src/ipc.rs).
To expose new backend functionality to the frontend:

1. Write the handler in `ipc.rs`:

   ```rust
   #[tauri::command]
   pub async fn get_thing(
       store: tauri::State<'_, SnapshotStore>,
       id: String,
   ) -> Result<Thing, String> {
       store.get_thing(&id).await.map_err(|e| e.to_string())
   }
   ```

   - Return types must be `serde::Serialize`; arguments must be
     `serde::Deserialize`. Prefer types already defined in
     [`models.rs`](../src-tauri/src/models.rs).
   - Return `Result<T, String>` so the frontend receives a typed rejection it
     can surface to the user.
   - Long-running work should be `async` and delegate to `SnapshotStore` /
     `command.rs` helpers rather than blocking the IPC thread.

2. Register the handler in [`lib.rs`](../src-tauri/src/lib.rs) under
   `tauri::generate_handler![...]`.

3. Call it from the frontend via `window.__TAURI__.core.invoke`:

   ```js
   const thing = await window.__TAURI__.core.invoke('get_thing', { id });
   ```

   Argument keys are camelCase in JS; Tauri converts them to snake_case for the
   Rust handler.

4. If the command needs new permissions or a non-default capability, update the
   manifests under [`src-tauri/capabilities/`](../src-tauri/capabilities/).

## Modifying the frontend

The frontend is a plain static SPA — no build step, no bundler.

- [`src/index.html`](../src/index.html) — entry point.
- [`src/static/css/app.css`](../src/static/css/app.css) — styles.
- [`src/static/js/app.js`](../src/static/js/app.js) — top-level app module.
- [`src/static/js/renderers/`](../src/static/js/renderers/) — per-view
  renderers (Claude transcript, Codex rollout, Markdown, raw HTML).

In `cargo tauri dev`, edits to files under `src/` are picked up on WebView
refresh (⌘R / Ctrl+R). No build step runs between edit and reload.

All network access goes through `window.__TAURI__.core.invoke` — there is no
HTTP fetch to a local server. If you need to add a new data source, add a
Tauri command (see above) rather than a `fetch`.

## Running tests

### Rust

```bash
cd src-tauri
cargo test                         # full suite
cargo test --test snapshot_flow    # single integration test binary
cargo clippy --all-targets -- -D warnings
```

Unit tests live alongside the code they cover (`#[cfg(test)]` modules).
Integration tests live in `src-tauri/tests/`, with shared fixtures under
`src-tauri/tests/fixtures/` and helpers under `src-tauri/tests/common/`. The
fixtures simulate a Gas Town workspace on disk — snapshot tests assert against
serialised results built from those fixtures.

### End-to-end

For a quick smoke test of the full desktop app:

```bash
cd src-tauri
cargo tauri dev
```

Point at a real workspace with `GT_ROOT=/path/to/gt`.

## Release builds

```bash
cd src-tauri
cargo tauri build
```

Outputs:

- `src-tauri/target/release/gtui` — raw binary.
- `src-tauri/target/release/bundle/macos/GTUI.app` — macOS app bundle.

Installer artifacts are generated separately from the normal run path. On
macOS:

```bash
cargo tauri build --bundles dmg
```

That writes `src-tauri/target/release/bundle/dmg/GTUI_<version>_<arch>.dmg`.

The release profile is configured in `src-tauri/Cargo.toml` under
`[profile.release]` (LTO, single codegen unit, `strip = true`, `panic = abort`).
