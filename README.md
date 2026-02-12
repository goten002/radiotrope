# Radiotrope

[![CI](https://github.com/goten002/radiotrope/actions/workflows/ci.yml/badge.svg)](https://github.com/goten002/radiotrope/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Internet radio streaming engine and terminal player for Rust.

Playback is handled by [rodio](https://github.com/RustAudio/rodio). Format decoding is handled by [symphonia](https://github.com/pdeljanov/Symphonia), with additional codec support from [symphonia-adapter-fdk-aac](https://crates.io/crates/symphonia-adapter-fdk-aac) and [symphonia-adapter-libopus](https://crates.io/crates/symphonia-adapter-libopus).

![radiotrope cli](assets/radiotrope_cli.png)

## Supported Formats

### Stream Protocols

| Protocol | Status | Description |
|----------|--------|-------------|
| ICY | Supported | Icecast/Shoutcast with metadata extraction |
| HLS | Supported | MPEG-TS segment demuxing |
| Direct HTTP | Supported | Plain HTTP/HTTPS audio streams |
| PLS | Supported | Playlist resolution with recursive following |
| M3U | Supported | Playlist resolution with recursive following |

### Audio Codecs

| Codec | Status | Provider |
|-------|--------|----------|
| MP3 | Supported | symphonia |
| AAC-LC | Supported | symphonia / fdk-aac |
| OGG Vorbis | Supported | symphonia |
| FLAC | Supported | symphonia |
| WAV/PCM | Supported | symphonia |
| Opus | Supported | libopus (bundled) |
| HE-AAC (SBR) | Partial | fdk-aac decodes core AAC-LC, SBR data skipped |

## Architecture

The project is a Cargo workspace with two crates:

| Crate | Type | Description                                                                       |
|-------|------|-----------------------------------------------------------------------------------|
| `radiotrope` | Library | Streaming engine, stream resolution, audio decoding, buffering, health monitoring |
| `radiotrope-cli` | Binary | Terminal player built with [ratatui](https://github.com/ratatui/ratatui)          |

The engine is designed to be embedded in any Rust application. The CLI is one consumer of the library API.

### Engine Features

| Feature | Description |
|---------|-------------|
| Stream resolution | Automatic protocol detection, playlist unwinding, format probing |
| Decoupled buffering | Producer-consumer architecture isolating network I/O from audio decoding |
| Health monitoring | Stall detection, no-audio timeout, stream failure reporting |
| Error recovery | Automatic reconnection with exponential backoff (ICY and HLS) |
| Spectrum analyzer | Real-time FFT-based frequency analysis |
| Event system | Channel-based events for playback state, metadata, and health changes |

## Installation

### From source

```bash
git clone https://github.com/goten002/radiotrope.git
cd radiotrope
cargo build --release
```

The binary will be at `target/release/radiotrope-cli`.

### Dependencies (Linux only)

Radiotrope uses rodio for audio output, which requires ALSA on Linux:

```bash
# Debian/Ubuntu
sudo apt install libasound2-dev

# Arch/Manjaro
sudo pacman -S alsa-lib

# Fedora
sudo dnf install alsa-lib-devel
```

## Usage

```bash
radiotrope-cli <URL>
```

## Roadmap

- [ ] Desktop UI (Slint)
- [ ] Station search and browsing (Radio Browser API)
- [ ] Favorites management
- [ ] Audio recording to file
- [ ] Equalizer and audio effects (DSP chain)

## License

Radiotrope is provided under the MIT license. See [LICENSE](LICENSE) for details.

This project includes third-party dependencies with different licenses.
See [THIRD-PARTY-LICENSES](THIRD-PARTY-LICENSES) for details.
