# rus-torrent

`rus-torrent` is a Rust BitTorrent client built on top of `librqbit`.

The repository currently contains two frontends that share the same torrent engine:

- `TUI`: a terminal interface built with `ratatui` and `crossterm`
- `Desktop`: a Tauri-based desktop app with a web UI

Both variants can queue torrents from:

- local `.torrent` files
- `http://` and `https://` URLs to `.torrent` files
- `magnet:` links

## Project Layout

- `src/`: shared torrent engine and terminal UI
- `src-tauri/`: desktop app wrapper and Tauri config
- `dist/`: built frontend assets for the desktop app
- `windows-build/`: prebuilt Windows desktop artifacts

## Features

- shared Rust torrent engine for both UIs
- queue torrents from files, URLs, and magnet links
- choose a per-download output directory
- live download list with progress, transfer speed, and peer counters
- TUI filtering, sorting, and compact/expanded download views
- local path autocompletion in the TUI

## Requirements

General:

- Rust toolchain
- Cargo

For the desktop build on Ubuntu/Debian-like systems:

```bash
sudo ./scripts/install-tauri-deps-ubuntu.sh
```

## Quick Start

### Run the TUI

From the repository root:

```bash
cargo run
```

The TUI stores its working data in:

```text
./rus-torrent-data/
```

That directory contains:

- `downloads/`
- `incoming-torrents/`

### Run the Desktop App

From the repository root:

```bash
./scripts/run-tauri.sh
```

Or directly:

```bash
cargo run --manifest-path src-tauri/Cargo.toml
```

The desktop app stores its data in the Tauri application data directory for the current OS/user profile.

## Build

### TUI

```bash
cargo check
cargo build
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo fmt
```

### Desktop

```bash
cargo check --manifest-path src-tauri/Cargo.toml
cargo run --manifest-path src-tauri/Cargo.toml
```

## TUI Controls

Main controls:

- `F1`: open help
- `F10` or `Ctrl+C`: force exit
- `Tab` / `Shift+Tab`: cycle local path completion
- `Enter`: queue torrent from the current source field
- `Up` / `Down`: switch active input field or move in lists

Downloads screen:

- `/`: open filter dialog
- `s`: open sort dialog
- `r`: reverse sort order
- `m`: toggle compact/expanded view
- `c`: clear filter with confirmation
- `x`: reset downloads view
- `q`: quit with confirmation

## Releases

The repository publishes two release tracks:

- `TUI`: Linux terminal build
- `Desktop`: Windows desktop build

See the GitHub Releases page for published artifacts.

## Notes

- The TUI and desktop app share the same core torrent engine.
- The `incoming-torrents` directory is reserved for future automation integrations.
