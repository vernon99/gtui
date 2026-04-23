# Developing GTUI

This guide covers the day-to-day tasks for the Tauri desktop app: adding
backend commands, modifying the frontend, running tests, and building bundles.

## Project Layout

```text
gtui/
├── frontend/    # Static SPA loaded by the WebView
├── src-tauri/   # Rust backend and Tauri host process
├── docs/
└── scripts/
```

See the [root README](../README.md) for a module-level map of `src-tauri/src/`.

## Running The App

```bash
cd src-tauri
cargo tauri dev
cargo tauri build
```

Both commands read `src-tauri/tauri.conf.json`. `frontendDist` points at
`../frontend`, so the frontend is served directly from disk and no bundler is
needed.

## Adding A Tauri Command

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

2. Prefer return types defined in
   [`models.rs`](../src-tauri/src/models.rs). Return `Result<T, String>` so the
   frontend receives a typed rejection it can surface.

3. Register the handler in [`lib.rs`](../src-tauri/src/lib.rs) under
   `tauri::generate_handler![...]`.

4. Call it from the frontend:

   ```js
   const thing = await window.__TAURI__.core.invoke("get_thing", { id });
   ```

   Argument keys are camelCase in JS; Tauri maps them to snake_case for Rust
   handler arguments.

5. If the command needs new permissions, update
   [`src-tauri/capabilities/`](../src-tauri/capabilities/).

## Modifying The Frontend

The frontend is a plain static SPA.

- [`frontend/index.html`](../frontend/index.html) — entry point.
- [`frontend/static/css/app.css`](../frontend/static/css/app.css) — styles.
- [`frontend/static/js/app.js`](../frontend/static/js/app.js) — top-level app
  module.
- [`frontend/static/js/renderers/`](../frontend/static/js/renderers/) —
  transcript and markdown renderers.

In `cargo tauri dev`, reload the WebView after editing frontend files. All data
access goes through `window.__TAURI__.core.invoke`; do not add local HTTP
fetches.

## Running Tests

```bash
cd src-tauri
cargo test
cargo test --test snapshot_flow
cargo clippy --all-targets -- -D warnings
```

Unit tests live alongside the code they cover. Integration tests live in
`src-tauri/tests/`, with shared fixtures under `src-tauri/tests/fixtures/` and
helpers under `src-tauri/tests/common/`.

## Smoke Testing

```bash
cd src-tauri
GT_ROOT=/path/to/gt cargo tauri dev
```

For a rebuild-and-launch loop on macOS:

```bash
scripts/rebuild_and_run.sh
```

## Inspection

Dump the live backend snapshot JSON:

```bash
scripts/dump_snapshot.sh --out logs/snapshot.json
```

Open the native Tauri WebView with Web Inspector in debug builds:

```bash
scripts/inspect_native.sh
```

This sets `GTUI_OPEN_DEVTOOLS=1` and launches the real app. On macOS, Tauri's
desktop WebDriver path is not available because WKWebView has no desktop
WebDriver driver; use the Web Inspector for native frontend inspection.

## README Screenshots

README screenshots are rendered from isolated frontend fixtures, not from a
running GTUI build:

```bash
scripts/render_readme_screenshots.sh
```

The fixture lives at
[`frontend/screenshots/readme.html`](../frontend/screenshots/readme.html) and
uses the production stylesheet from `frontend/static/css/app.css`. The renderer
uses macOS WebKit through a small Swift helper, captures the fixture elements,
and writes `docs/assets/task-spine.png` and `docs/assets/mayor-chat.png`.

No Node, Python, or browser automation package is required.

## Release Builds

```bash
cd src-tauri
cargo tauri build
```

Outputs:

- `src-tauri/target/release/gtui` — raw binary.
- `src-tauri/target/release/bundle/macos/GTUI.app` — macOS app bundle.

For a macOS disk image:

```bash
cargo tauri build --bundles dmg
```

The release profile is configured in `src-tauri/Cargo.toml`.
