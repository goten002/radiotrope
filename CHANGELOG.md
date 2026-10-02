# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com),
and this project adheres to [Semantic Versioning](https://semver.org).

## [Unreleased]

Everything below is on the `test/all-upgrades` branch and not yet on `main`.

### Added
- `radiotrope-probe`, a station checker for people and schedulers: it listens to a station for a few seconds and reports whether it is on air, its codec and format, its sound level and what is playing, as text or JSON, with monitoring-style exit codes.
- AI agents over MCP: one shared player for every local agent, 14 tools (play, search, favorites, volume, recording, status), an Agents dialog with setup lines for Claude Code, Claude Desktop, Cursor and Codex, and optional network access with a token.
- Recording of the playing station to MP3, Opus or WAV, with or without the equalizer.
- Song titles from ID3v1/ID3v2 tags in MP3, AAC and HLS streams when ICY gives none.
- Equalizer presets in groups, with an automatic preamp and new 40 Hz and 12 kHz shelves.
- "AAC+" label for HE-AAC stations.
- Wave and Dot Matrix visualizer modes, visualizers coloured from the station logo, and a View menu switch to turn the visualizer off.
- Station browser: logos in the list, clear errors with retry, and switching to another radio-browser server when one fails.
- A soft fog behind dark or light transparent logos that would otherwise vanish on the tile.
- Keyboard use and screen reader names for the controls, and touch support for the bubbles.
- Windows: an exe icon and version info, no console window in release, and a log file.
- A system tray icon on Windows and Linux: show or hide the window, Play/Stop, Mute, favorites, recording and Quit from its menu, middle click to play or stop. View has Show Tray Icon, Close to Tray and Start Hidden in Tray; Quit (Ctrl+Q) is in the Open menu.

### Changed
- Licence: GPL-3.0-or-later (was MIT).
- Favorites, settings and the agent token are saved atomically with a backup, and one bad favorite no longer loses the whole file.
- Two players open at once see each other's favorite changes.
- The audio output closes while stopped, follows the default device, and reopens after a device is lost.
- Lower idle cost: fewer redraws and timer wake-ups while playing and stopped.

### Fixed
- Many streaming fixes: HLS segment timeouts, plain M3U versus HLS, SHOUTcast redirects, stale HLS playlists, and reconnects after network drops.
- Logos landing on the wrong station, logo cache wipes after the id change, and caps on logo downloads.
- Recordings are finished properly at exit.
