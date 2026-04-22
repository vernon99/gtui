# webui (legacy)

This directory contains the original GTUI web UI — a static single-page app
(`index.html` + `static/`) served by a Python standard-library backend
(`server.py`). It is preserved here for comparison while GTUI migrates to a
Tauri application (Rust backend + WebView frontend).

## Run it

```bash
cd webui
python3 server.py
```

See `server.py --help` for host, port, gt-root, and interval flags.

## Layout

- `server.py` — HTTP server + data collection
- `index.html` — single-page app entrypoint
- `static/` — CSS, JS modules, renderers
- `scripts/` — auxiliary tooling (e.g. README screenshot renderer)
