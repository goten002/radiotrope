# Radiotrope

[![CI](https://github.com/goten002/radiotrope/actions/workflows/ci.yml/badge.svg)](https://github.com/goten002/radiotrope/actions/workflows/ci.yml)
[![License: GPL v3+](https://img.shields.io/badge/license-GPLv3%2B-blue.svg)](LICENSE)

An AI agent-enabled internet radio player built with Rust and [Slint](https://slint.dev). Control playback from your AI assistant via the [Model Context Protocol (MCP)](https://modelcontextprotocol.io), or use the desktop GUI and terminal interfaces directly.

![radiotrope](assets/radiotrope_app.png)

## Features

- **MCP server** - AI agents (Claude, etc.) can play stations, control volume, and query status through natural language
- **10-band equalizer** - 14 presets, per-band gain control, preamp
- **Recording** - record the playing station to MP3 (192 kbps), Opus (128 kbps) or WAV in `Music/Radiotrope` or a folder you choose, with or without the equalizer
- **Desktop GUI** - built with Slint, dark/light themes, user-selectable accent color, real-time spectrum visualization, stream statistics
- **Terminal player** - lightweight TUI with ratatui for headless/SSH use
- **Resilient streaming** - automatic reconnection with exponential backoff, stall detection, health monitoring
- **Wide format support** - MP3, AAC, HE-AAC, Vorbis, Opus, FLAC over ICY, HLS, and HTTP

## MCP Integration

Radiotrope exposes an MCP server that lets AI agents control the player. Add it to your MCP client configuration:

```json
{
  "mcpServers": {
    "radiotrope": {
      "command": "radiotrope",
      "args": ["--mcp"]
    }
  }
}
```

Every agent shares the one player running on your computer. `radiotrope --mcp` connects the agent to it, and starts the player (with its window) if it isn't running yet. Closing an agent leaves the music playing; closing the player's window ends every agent's session. Starting `radiotrope` a second time brings the running window forward instead of opening another. For a separate player of its own, give an agent `--mcp --standalone`.

**Tools > Agents (MCP)** has a ready line that adds Radiotrope to your agent, with a Copy button. Pick the agent at the top: Claude Code, Claude Desktop, Codex CLI, Gemini CLI, VS Code or Cursor (the last two and Claude Desktop get the JSON for their settings file). While agents use the player, a small robot chip at the right of the menu bar shows how many; hover it to see which (a network agent counts until it has been quiet for 5 minutes).

### Agents on other computers

Radiotrope can also take agents over the network (MCP Streamable HTTP). It is off until you turn it on in **Tools > Agents (MCP)**, where you also find:

- **Address**: `127.0.0.1:8765` (this computer only) by default. Enter this computer's network address, or `0.0.0.0` for all networks, to let other computers in.
- **Authentication**: **None** (the default) lets in anyone who can reach the address. **Token** makes every request carry the token as `Authorization: Bearer <token>`.
- **Token**: made the first time Token is picked with network agents on, kept in `mcp-token` in the config folder (readable by you only). **New token** replaces it; agents with the old one stop working.
- **A ready line for the picked agent** with a Copy button, for example for Claude Code:

  ```bash
  claude mcp add --transport http radiotrope http://192.168.1.20:8765/mcp --header "Authorization: Bearer <token>"
  ```

  With Authentication set to None, the line has no header. Codex CLI takes the token from the `RADIOTROPE_TOKEN` environment variable instead. Claude Desktop only takes agents on this computer.

Requests from web pages (with an `Origin` header) are refused. While the server listens on this computer only, other host names are refused too (DNS rebinding).

The server speaks plain http, meant for your own network. With Authentication set to None, anyone who can reach the address can use the player; the token keeps other people out, but it travels unencrypted. To reach the radio from anywhere without opening ports, run it over a private network such as [Tailscale](https://tailscale.com): put the computer's Tailscale address (100.x.y.z) in the address field. The traffic is then encrypted end to end. The claude.ai and Claude Desktop "custom connectors" connect from Anthropic's cloud, so they can't reach a radio on your home network; use Claude Code, or `radiotrope --mcp` on the same computer.

### Available Tools

| Tool | Description |
|------|-------------|
| `play_url` | Play a station by stream URL and wait until it plays or fails |
| `play_station` | Play a station from search results by its id |
| `play_favorite` | Play a favorite station by ID |
| `stop` | Stop playback |
| `set_volume` | Set volume 0-100 |
| `set_muted` | Mute or unmute |
| `get_status` | Playback state, station, song, volume, stream format, recording, the last error and which agent changed the player last |
| `search_stations` | Search radio-browser.info by name, genre, country, language, codec and minimum bitrate; with nothing given, the most popular stations |
| `list_categories` | List the directory's genres, countries or languages |
| `list_favorites` | List all saved favorite stations with IDs |
| `add_favorite` | Add a station to favorites (with optional logo URL and country) |
| `remove_favorite` | Remove a station from favorites by ID or URL |
| `start_recording` | Record the station playing, with the player's recording settings |
| `stop_recording` | Stop and save the recording |

The server speaks every MCP version from 2024-11-05 to 2026-07-28 (it is built on [rmcp](https://github.com/modelcontextprotocol/rust-sdk), the official Rust SDK). Tools that return data return structured JSON with an output schema, and every tool carries a title and behaviour hints (read-only, destructive, open-world) that clients use when asking for confirmation.

Once configured, you can ask your AI assistant things like *"play BBC Radio 1"*, *"search for jazz stations"*, *"set volume to 50"*, or *"what's currently playing?"*.

## Supported Formats

### Stream Protocols

| Protocol | Description |
|----------|-------------|
| ICY | Icecast/Shoutcast with metadata extraction |
| HLS | MPEG-TS segment demuxing |
| Direct HTTP | Plain HTTP/HTTPS audio streams |
| PLS / M3U | Playlist resolution with recursive following |

### Audio Codecs

| Codec | Provider |
|-------|----------|
| MP3 | symphonia |
| AAC-LC | fdk-aac |
| HE-AAC (SBR) | fdk-aac |
| OGG Vorbis | symphonia |
| FLAC | symphonia |
| WAV/PCM | symphonia |
| Opus | libopus (bundled) |

### Metadata

| Format | Status | Description |
|--------|--------|-------------|
| ICY (Icecast/Shoutcast) | Supported | Artist/title extraction from in-band ICY metadata blocks |
| ID3v1/ID3v2 | Planned | Embedded tags in MP3 streams (artist, title, album, genre, etc.) |

## Architecture

The project is a Cargo workspace with three crates:

| Crate | Type | Description |
|-------|------|-------------|
| `radiotrope` | Library | Streaming engine: stream resolution, audio decoding, buffering, health monitoring |
| `radiotrope-app` | Binary | Desktop GUI with MCP server, built with [Slint](https://slint.dev) |
| `radiotrope-cli` | Binary | Terminal player built with [ratatui](https://github.com/ratatui/ratatui) |

The engine is designed to be embedded in any Rust application. Both the GUI and CLI are consumers of the library API.

### Engine Highlights

| Feature | Description |
|---------|-------------|
| Stream resolution | Automatic protocol detection, playlist unwinding, format probing |
| Decoupled buffering | Producer-consumer architecture isolating network I/O from audio decoding |
| Health monitoring | Stall detection, no-audio timeout, stream failure reporting |
| Error recovery | Automatic reconnection with exponential backoff (ICY and HLS) |
| Spectrum analyzer | Real-time FFT-based frequency analysis |
| 10-band equalizer | Biquad IIR filters (LowShelf/PeakingEQ/HighShelf), 14 presets, live parameter updates |
| Event system | Channel-based events for playback state, metadata, and health changes |

## Installation

### From source

```bash
git clone https://github.com/goten002/radiotrope.git
cd radiotrope
cargo build --release
```

Binaries will be at:
- `target/release/radiotrope` - Desktop GUI + MCP server
- `target/release/radiotrope-cli` - Terminal player

### Dependencies (Linux)

Radiotrope uses rodio for audio output, which requires ALSA on Linux:

```bash
# Debian/Ubuntu
sudo apt install libasound2-dev

# Arch/Manjaro
sudo pacman -S alsa-lib

# Fedora
sudo dnf install alsa-lib-devel
```

### Desktop integration (Linux)

The window's app id (Wayland) and WM_CLASS (X11) are `radiotrope`, so the
desktop file must be installed as `radiotrope.desktop` for the dock/taskbar
to show the icon:

```bash
install -Dm644 packaging/linux/radiotrope.desktop /usr/share/applications/radiotrope.desktop
for s in 16 32 48 64 128 256; do
  install -Dm644 assets/icons/icon-$s.png /usr/share/icons/hicolor/${s}x${s}/apps/radiotrope.png
done
```

## Usage

### Desktop GUI

```bash
radiotrope
```

### MCP mode (for AI agents)

```bash
radiotrope --mcp               # connect to the running player, start it if needed
radiotrope --mcp --standalone  # a separate player for this agent alone
```

### Terminal player

```bash
radiotrope-cli <URL>
```

![radiotrope cli](assets/radiotrope_cli.png)

## Roadmap

- [x] Station search and browsing (Radio Browser API)
- [x] Favorites management
- [x] Equalizer and audio effects (DSP chain)
- [x] User-selectable accent color (9 palette presets + custom hex)
- [x] Persistent settings (window state, theme, EQ, preferences)
- [ ] Export/import favorites
- [ ] System tray integration
- [ ] ID3 tag metadata extraction (MP3 streams)
- [ ] Audio recording to file


## License

Copyright (C) 2026 George Alexiou

Radiotrope is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version. See [LICENSE](LICENSE) for the full text.

Radiotrope is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

**Additional permission under GNU GPL version 3 section 7:** if you modify
Radiotrope, or any covered work, by linking or combining it with the
Fraunhofer FDK AAC library (libfdk-aac), or a modified version of that library,
containing parts covered by the terms of the Fraunhofer FDK AAC Codec Library
license, the licensors of Radiotrope grant you additional permission to convey
the resulting work.

This project includes third-party dependencies with different licenses.
See [THIRD-PARTY-LICENSES](THIRD-PARTY-LICENSES) for details.
