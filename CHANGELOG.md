# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com),
and this project adheres to [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added
- Updates: Help > Check for Updates, and an optional daily check, tell you when a newer version is out (a dot on Help, a row in the menu and a line in About with a link to what's new; never a pop-up). The Windows install and the AppImage can download the update, check its signature and restart into it, each on your click. Packages from a package manager (AUR) and the Raspberry Pi image never check; `RADIOTROPE_UPDATES=off|notify` turns it down anywhere. A check sends nothing but the request itself.
- `make install` for Linux (with `PREFIX` and `DESTDIR`): the programs, the launcher, icons up to 512 px, an AppStream entry for software centres, man pages, and bash, zsh and fish completions.
- Windows downloads: a portable zip for x64 and for ARM64, and one installer for both (per-user by default, optional all-users, Start Menu entry, optional desktop icon, an uninstaller that offers to delete your data), listed in WinGet as `Radiotrope.Radiotrope`. Updates come through `winget upgrade` or the player's own update.
- Windows portable mode: an empty file named `portable` next to `radiotrope.exe` keeps all settings, favorites, cache and the log in a `data` folder beside it.
- `radiotrope --quit` closes the running player, including one in the tray, and waits for it to exit. The installer uses it before an update.
- Windows: `radiotrope-cli.exe` and `radiotrope-probe.exe` get the icon and version details too, and all three name "Radiotrope" as the publisher.
- Desktop keyboard shortcuts for playback, volume, favorites, recording, search, opening a stream, and closing the window. Help > Keyboard Shortcuts (F1) lists the bindings, and menus show hints next to the corresponding commands.
- The terminal player can search the station directory and add or remove favorites shared with the Radiotrope app.
- Scheduled playback in Tools > Scheduler: start a station at a set time with a fade-in, timed recordings, and stops, once on a chosen date or weekly on chosen days, with an end time or length and their own volume. If a station hasn't started 30 seconds after its start time, the Scheduler switches to alarm mode and plays a fallback sound until stopped (on by default, per entry). A Sleep Timer in Tools and the tray stops playback after 15 minutes to 6 hours or any length up to a day in 5 minute steps, opening on the length used last time, with a fade-out, and can be restarted or stopped while it runs. A grey Timer label with a moon under the station shows the time left; a grey Scheduler label counts down the last minute before an entry and says what started for 15 seconds, or until clicked. The player must be open; it does not wake the computer.
- `radiotrope-probe`, a station checker for people and schedulers: it listens to a station for a few seconds and reports whether it is on air, its codec and format, its sound level and what is playing, as text or JSON, with monitoring-style exit codes. It is a separate download from the releases page (Linux x86_64 and aarch64, Windows x64 and ARM64), not part of any package or installer.
- AI agents over MCP: one shared player for every local agent, 19 tools (play, search, favorites, volume, recording, timer, scheduler, status), an Agents dialog with setup lines for Claude Code, Claude Desktop, Cursor and Codex, and optional network access with a token.
- Recording of the playing station to MP3, Opus or WAV, with or without the equalizer.
- Song titles from ID3v1/ID3v2 tags in MP3, AAC and HLS streams when ICY gives none.
- Equalizer presets in groups, with an automatic preamp and new 40 Hz and 12 kHz shelves.
- "AAC+" label for HE-AAC stations.
- macOS: AAC+ (HE-AAC v1 and v2) stations play in full quality with the system's own decoder (AudioToolbox), with nothing to install. A stream it turns down goes to libfdk-aac if one is installed, then to the built-in AAC-LC decoder; `RADIOTROPE_AAC_DECODER=audiotoolbox|fdk|built-in` picks a decoder to compare them.
- Wave and Dot Matrix visualizer modes, visualizers coloured from the station logo, and a View menu switch to turn the visualizer off.
- System Theme in the View menu: the app follows the light or dark setting of Windows or the Linux desktop (GNOME, KDE and others, X11 or Wayland) and switches live when it changes. New installs start on it; a saved Dark or Light choice is kept.
- Station browser: logos in the list and clear errors with retry.
- A soft fog behind dark or light transparent logos that would otherwise vanish on the tile.
- Keyboard use and screen reader names for the controls, and touch support for the bubbles.
- Windows: an exe icon and version info, no console window in release, and a log file.
- A system tray icon on Windows and Linux: show or hide the window, Play/Stop, Mute, favorites, recording and Quit from its menu, middle click to play or stop. View has Show Tray Icon, Close to Tray, Minimize to Tray and Start Hidden in Tray; Quit (Ctrl+Q) is in the Open menu.
- `radiotrope-probe` names itself to stations as `RadiotropeProbe/<version>`, and a program using it as a library can set its own User-Agent with `set_user_agent`.

### Changed
- Version 0.1.0. `--version` names the program (`radiotrope 0.1.0`, not `radiotrope-app 0.1.0-dev`). Building needs Rust 1.95 or newer.
- The Linux app id is `app.radiotrope.Radiotrope` (window, launcher, icon and tray). An older hand-installed `radiotrope.desktop` should be removed. Settings and favorites stay where they were.
- AAC+ (HE-AAC) now uses the system's libfdk-aac, loaded at run time, instead of a copy built into the programs. Where the library is not installed (or is a build without HE-AAC), AAC+ stations play their AAC-LC core with the built-in decoder and show as "AAC"; the README says how to install the library. The programs no longer contain FDK AAC code, and a `system-fdk` cargo feature builds them without the loader.
- HTTPS uses rustls with the system's certificates instead of OpenSSL (Linux) and SChannel (Windows), so Linux builds no longer need or link OpenSSL.
- AAC surround streams (more than two channels) play mixed down to stereo instead of failing.
- Open > Network Stream asks for the stream URL first, and it is all a stream needs: the name, logo and country are optional. Without a name, the stream's own name is shown.
- Search Stations has Country and Genre filters under the search box, combinable with a name search, and Reset filters to clear them. Stations > By Country is gone: pick a country in the filters instead.
- Station search uses Radiotrope's own directory at api.radiotrope.app instead of radio-browser.info: typo-tolerant name search, countries, genres and languages, 256 px logos, and plays counted for its popular list. The Browse, phone Search and agent search all use it. Votes, trending and bitrate orders are gone (the phone's are treated as Most played); codec and bitrate filters for agents are checked on each page. Favorites saved from radio-browser keep playing as before.
- Licence: GPL-3.0-or-later (was MIT).
- Favorites, settings and the agent token are saved atomically with a backup, and one bad favorite no longer loses the whole file.
- Two players open at once see each other's favorite changes.
- The audio output closes while stopped, follows the default device, and reopens after a device is lost.
- Lower idle cost: fewer redraws and timer wake-ups while playing and stopped.
- Messages about the audio device and reconnects go through the standard `log` crate instead of straight to stderr. The player, the CLI and the probe print warnings and notices as before; set `RUST_LOG` (for example `RUST_LOG=radiotrope=debug`) to see more or fewer. The probe shows only warnings by default, and programs using the engine as a library choose for themselves.

### Removed
- Stations > Undo Favorites Import. The import dialog already shows what an import will do before it changes anything. Phones can still undo an import they sent.

### Fixed
- Favorite logos now update when a station changes its logo: a logo from a new address is downloaded at once, and every favorite's logo is downloaded again once a month. The old logo stays if the download fails.
- Many streaming fixes: HLS segment timeouts, plain M3U versus HLS, SHOUTcast redirects, stale HLS playlists, and reconnects after network drops.
- Logos landing on the wrong station, logo cache wipes after the id change, and caps on logo downloads.
- Recordings are finished properly at exit.
