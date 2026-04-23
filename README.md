# GTUI

GTUI is a desktop dashboard for a [Gas Town](https://github.com/vernon99/gastown)
workspace. It visualises the task spine, agent activity, git memory, and live
intervention hooks for a local `gt` workspace.

GTUI is a [Tauri](https://tauri.app) application: a Rust backend in
`src-tauri/` hosts a static WebView frontend from `frontend/`. There is no
local HTTP server or REST API.

## Screenshots

The screenshots below are rendered from isolated representative fixtures, not
from a live app run.

![GTUI task spine view](docs/assets/task-spine.png)

![GTUI mayor Claude transcript view](docs/assets/mayor-chat.png)

## Prerequisites

- **Rust toolchain** — stable Rust 1.77 or newer (`rustup install stable`).
- **Tauri CLI** — `cargo install tauri-cli --version "^2"` installs
  `cargo tauri`.
- **Tauri platform dependencies** — see the
  [Tauri prerequisites guide](https://tauri.app/start/prerequisites/).
- A local Gas Town workspace. GTUI defaults to `~/gt`; override with `GT_ROOT`.

## Quick Start

```bash
cd src-tauri
cargo tauri dev
```

To build a runnable app bundle:

```bash
cd src-tauri
cargo tauri build
```

The macOS bundle is written to
`src-tauri/target/release/bundle/macos/GTUI.app`. Installer images are an
explicit packaging step; on macOS, run `cargo tauri build --bundles dmg` when
you need a `.dmg`.

To point GTUI at a non-default workspace:

```bash
GT_ROOT=/path/to/gt cargo tauri dev
```

## Layout

```text
gtui/
├── frontend/           # Static WebView frontend
│   ├── index.html
│   └── static/         # CSS, JS modules, transcript renderers
├── src-tauri/          # Rust backend and Tauri host process
│   ├── src/
│   │   ├── main.rs     # Entry point
│   │   ├── lib.rs      # Tauri builder + IPC registration
│   │   ├── config.rs   # GT_ROOT / workspace resolution
│   │   ├── command.rs  # Shell-out helpers for gt, git, tmux
│   │   ├── parse.rs    # Parsers for gt, git, session outputs
│   │   ├── sessions.rs # Claude / Codex session discovery
│   │   ├── models.rs   # Serde types exposed over IPC
│   │   ├── snapshot.rs # Periodic snapshot builder + cache
│   │   └── ipc.rs      # #[tauri::command] entry points
│   ├── capabilities/
│   ├── icons/
│   ├── tests/
│   ├── Cargo.toml
│   └── tauri.conf.json
├── docs/
├── scripts/
└── README.md
```

The backend polls Gas Town on a background Tokio task, builds immutable
snapshots, and exposes app actions through Tauri IPC. The frontend calls those
commands with `window.__TAURI__.core.invoke`.

For development details, see [docs/developing.md](docs/developing.md).

## Notes

- GTUI runs in-process through Tauri; it does not bind a local web server.
- The backend adds `~/.local/bin` to `PATH` so local `gt` and related commands
  are discoverable.
- Runtime logs, PID files, and Tauri build artifacts are git-ignored.

## License

MIT. See [LICENSE](LICENSE).
