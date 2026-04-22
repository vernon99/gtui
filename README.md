# GTUI

GTUI is a standalone local web dashboard for a Gas Town workspace. It serves a
static single-page UI from `index.html` plus `static/` assets, with a Python
standard-library backend from `server.py`.

## Features

- Task spine graph with dependency and commit-lineage edges.
- Agent activity grouped by hooked task, with detailed terminal/transcript
  rendering loaded on demand.
- Git memory viewer for recent commits, branches, worktrees, and diff-on-demand.
- Local activity feed from Gas Town.
- Light intervention hooks for retrying a running task, pausing agents, nudging,
  and sending instructions to active terminals.

## Screenshots

The screenshots below use representative sample data.

![GTUI task spine view](docs/assets/task-spine.png)

![GTUI mayor Claude transcript view](docs/assets/mayor-chat.png)

## Requirements

- Python 3.9 or newer.
- A local Gas Town workspace.
- No third-party Python packages are required.

## Run

```bash
git clone https://github.com/vernon99/gtui.git
cd gtui
python3 server.py
```

Then open [http://127.0.0.1:8420](http://127.0.0.1:8420).

By default, GTUI reads the workspace at `~/gt`. Override that with either
`GT_ROOT` or the `--gt-root` flag:

```bash
GT_ROOT=/path/to/gt python3 server.py
python3 server.py --gt-root /path/to/gt
```

Useful server flags:

```bash
python3 server.py --host 127.0.0.1 --port 8420 --interval 1
```

## Notes

- GTUI is designed for local use and binds to `127.0.0.1` by default.
- The backend adds `~/.local/bin` to `PATH` so local `gt` and related commands
  can be discovered.
- Runtime logs and PID files are local development artifacts and are not tracked.

## License

MIT. See [LICENSE](LICENSE).
