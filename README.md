# Radiotrope

[![CI](https://github.com/goten002/radiotrope/actions/workflows/ci.yml/badge.svg)](https://github.com/goten002/radiotrope/actions/workflows/ci.yml)
[![License: GPL v3+](https://img.shields.io/badge/license-GPLv3%2B-blue.svg)](LICENSE)

An internet radio player for the desktop, written in Rust with [Slint](https://slint.dev).
Play it yourself, from your phone, or let your AI assistant drive it over the
[Model Context Protocol (MCP)](https://modelcontextprotocol.io).

![Radiotrope desktop app](assets/radiotrope_app.png)

- **Find stations**: typo-tolerant search by name, country, genre and language in the Radiotrope station directory, or play any stream address
- **Plays almost anything**: MP3, AAC, HE-AAC, Vorbis, Opus and FLAC over Icecast/SHOUTcast, HLS and plain HTTP, with song titles from ICY and ID3
- **Sounds right**: 10-band equalizer with 14 presets, volume leveler, hiss, hum and rumble filters
- **Records**: MP3, Opus or WAV, by hand or on a schedule
- **Wakes you, sends you to sleep**: Scheduler for plays, recordings and stops, plus a sleep timer
- **AI agents**: 19 MCP tools, shared by every agent on your computer or your network
- **Phone remote**: pair the Radiotrope Remote app with a QR code
- **Also**: a terminal player, a station checker for monitoring (a separate download), and an experimental Raspberry Pi kiosk image

## Contents

- [Install](#install)
- [Using the desktop app](#using-the-desktop-app)
- [AI agents (MCP)](#ai-agents-mcp)
- [Phone remote](#phone-remote)
- [Terminal player](#terminal-player)
- [Station checker (radiotrope-probe)](#station-checker-radiotrope-probe)
- [Raspberry Pi appliance (experimental)](#raspberry-pi-appliance-experimental)
- [Supported streams and formats](#supported-streams-and-formats)
- [Project layout](#project-layout)
- [Development](#development)
- [License](#license)

## Install

Linux and Windows builds are on the
[releases page](https://github.com/goten002/radiotrope/releases), with
packages for Debian, Ubuntu, Fedora and openSUSE, an AppImage, a Flatpak
bundle and the AUR on Linux (below), and WinGet (`Radiotrope.Radiotrope`) on
Windows. [macOS](#macos-experimental) is experimental. Or build from source with Rust 1.95 or newer
([rustup](https://rustup.rs)).

> [!NOTE]
> AAC and AAC+ stations play in every build. Full-quality AAC+ (HE-AAC)
> needs the FDK AAC library installed on the system: see
> [AAC+ stations](#aac-stations-he-aac).

### Linux packages

Every release has these files for `x86_64` and `aarch64` (64-bit ARM, such
as a Raspberry Pi 4 or 5 on a 64-bit system). Each one holds the desktop app
and the terminal player (see [the table below](#linux-build-from-source)); the
[station checker](#station-checker-radiotrope-probe) is a download of its
own. They are built on
Ubuntu 22.04, so they need glibc 2.35 or newer: Debian 12, Ubuntu 22.04,
Fedora 41, openSUSE Leap 15.6 / Tumbleweed or newer.

| Format | File | Install | Updates |
|--------|------|---------|---------|
| Debian, Ubuntu, Mint, Raspberry Pi OS | `radiotrope_<version>-1_amd64.deb` (`arm64`) | `sudo apt install ./radiotrope_*.deb` | download the new one |
| Fedora, openSUSE | `radiotrope-<version>-1.x86_64.rpm` (`aarch64`) | `sudo dnf install ./radiotrope-*.rpm` or `sudo zypper install --allow-unsigned-rpm ./radiotrope-*.rpm` | download the new one |
| Arch, Manjaro, CachyOS | [AUR](https://aur.archlinux.org/packages/radiotrope): `radiotrope` (builds from source) or `radiotrope-bin` | `yay -S radiotrope` (or `paru`, or `makepkg -si`) | with the system |
| Any distribution | `Radiotrope-<version>-x86_64.AppImage` (`aarch64`) | `chmod +x Radiotrope-*.AppImage` and run it | AppImageUpdate, Gear Lever or AppImageLauncher |
| Any distribution with Flatpak | `Radiotrope-<version>-x86_64.flatpak` (`aarch64`) | `flatpak install --user ./Radiotrope-*.flatpak` | download the new one |
| Any distribution, by hand | `radiotrope-<version>-linux-x86_64.tar.gz` (`aarch64`) | see below | download the new one |
| Any distribution with snapd (Snap) | not in the Snap Store yet; `radiotrope_<version>_amd64.snap` (`arm64`) from a [Package Snap](../../actions/workflows/package-snap.yml) run | `sudo snap install --dangerous ./radiotrope_*.snap` | through the store, once it is there |

Notes on each:

- **.deb and .rpm** pull in what the app needs (ALSA, fontconfig, the
  display libraries). The packages aren't signed and there is no apt or dnf
  repository yet, so updates are a new download.
- **AppImage**: one file, nothing installed. It runs the desktop app; the
  terminal player is inside it too: `./Radiotrope-*.AppImage radiotrope-cli`
  (or a link named `radiotrope-cli` pointing at the AppImage). For a menu
  entry, use an integration tool such as Gear Lever or AppImageLauncher. The
  system tray works as with any other install (GNOME needs the AppIndicator
  extension). Without FUSE, add `--appimage-extract-and-run`.
- **Flatpak bundle**: not on Flathub. It needs the freedesktop runtime from
  Flathub (`flatpak remote-add --if-not-exists --user flathub
  https://dl.flathub.org/repo/flathub.flatpakrepo`). Run the terminal player with
  `flatpak run --command=radiotrope-cli app.radiotrope.Radiotrope`. In the
  sandbox it keeps its own settings and favorites (under
  `~/.var/app/app.radiotrope.Radiotrope/`), records only into
  `~/Music/Radiotrope` unless you pick another folder, and plays AAC+ as plain
  AAC (the runtime has no FDK AAC). A second launch opens a second window, and
  agents can't use it over `--mcp` yet: use another format if you use agents.
- **Snap**: strictly confined, on the core24 base with the GNOME platform
  snap (installed with it). The terminal player is `radiotrope.cli`
  (`sudo snap alias radiotrope.cli radiotrope-cli` for the usual name). Settings, favorites and the cache are the snap's own, under
  `~/snap/radiotrope/`; recordings go to `~/Music/Radiotrope` as usual. All
  it needs connects by itself (network, the agents' and phone servers,
  sound through PulseAudio or PipeWire, your home folder, the desktop and
  the tray icon), except recording to a USB disk or anything under `/media`
  or `/mnt`: `sudo snap connect radiotrope:removable-media`. Agents use
  `/snap/bin/radiotrope --mcp` (the Agents dialog shows it). The snap never
  checks for updates itself: the store updates it. **AAC+ plays as plain
  AAC**: the snap can't use the system's FDK AAC library and doesn't contain
  one, so AAC+ (HE-AAC) stations play their AAC-LC core (half the
  bandwidth, mono for HE-AAC v2). Use another format for full-quality AAC+.
- **Tarball**: unpack it and copy `bin/` and `share/` into `~/.local` (or,
  with `sudo`, `/usr/local`):

  ```bash
  tar -xzf radiotrope-*-linux-x86_64.tar.gz
  cp -r radiotrope-*-linux-x86_64/{bin,share} ~/.local/
  ```

  It needs `libasound2` and `fontconfig`, which desktops have.

For full-quality AAC+ in any of them except the Flatpak and the Snap, also install the
system's FDK AAC library (`libfdk-aac`): see
[AAC+ stations](#aac-stations-he-aac). The .deb recommends `libfdk-aac2` and
the .rpm `fdk-aac` (RPM Fusion) or `libfdk-aac2` (Packman), so they come
along when those repositories are enabled; the AUR packages list it as
optional.

Check a download against the release's `SHA256SUMS` with
`sha256sum --check --ignore-missing SHA256SUMS`, and that GitHub built it
from this repository with
`gh attestation verify <file> --repo goten002/radiotrope`.

### Linux: build from source

Install the build dependencies (ALSA for audio, fontconfig for the GUI, CMake
and a C++ compiler for the bundled C libraries). OpenSSL is not needed: HTTPS
uses rustls with the system's certificates.

```bash
# Debian / Ubuntu
sudo apt install libasound2-dev libfontconfig1-dev cmake g++ pkg-config make
# Arch / Manjaro
sudo pacman -S alsa-lib fontconfig cmake gcc pkgconf make
# Fedora
sudo dnf install alsa-lib-devel fontconfig-devel cmake gcc-c++ pkgconf-pkg-config make
```

Then build and install:

```bash
git clone https://github.com/goten002/radiotrope.git
cd radiotrope
make                                  # the two programs, man pages and completions
sudo make install                     # into /usr/local
```

To install for your user only, with no root, use
`make install PREFIX="$HOME/.local"` instead (make sure `~/.local/bin` is on
your `PATH`). `make uninstall`, with the same `PREFIX`, removes it all again.

This installs two programs:

| Program | What it is |
|---------|------------|
| `radiotrope` | The desktop app, and the MCP server for agents |
| `radiotrope-cli` | The terminal player |

along with the app's launcher and icons (so the dock and taskbar show the
right icon), its AppStream entry for software centres, man pages, and bash,
zsh and fish completions. Packagers can stage the same files with
`make install DESTDIR=<dir> PREFIX=/usr`.

The station checker is not installed by `make install`: build it with
`make probe generated` and install it with `make install-probe` (same
`PREFIX`; `make uninstall-probe` removes it).

The terminal tools also install on their own with cargo:
`cargo install --locked --git https://github.com/goten002/radiotrope radiotrope-cli`
(or `radiotrope-probe`). For the desktop app use `make install`, which adds
the launcher and icons.

If you installed an earlier build by hand, remove its old launcher and icons,
which used the name `radiotrope`:
`/usr/share/applications/radiotrope.desktop` and
`/usr/share/icons/hicolor/*/apps/radiotrope.png`.

### AAC+ stations (HE-AAC)

Radiotrope has its own decoder for plain AAC (AAC-LC). For AAC+ (HE-AAC v1
and v2, which many low-bitrate stations use) it uses the Fraunhofer FDK AAC
library when the system has it: the library is not part of Radiotrope, which
looks for it when it first plays AAC. Without it an AAC+ station still plays,
but only its AAC-LC core: half the bandwidth, so it sounds dull, and mono for
HE-AAC v2. It then shows as "AAC" instead of "AAC+".

To get full AAC+, install the library:

| System | How |
|--------|-----|
| Arch / Manjaro | `sudo pacman -S libfdk-aac` |
| Debian | `sudo apt install libfdk-aac2`, with the `non-free` component enabled |
| Ubuntu up to 25.04 | `sudo apt install libfdk-aac2`, with `multiverse` enabled |
| Ubuntu 25.10 and newer | The packaged `libfdk-aac2` is `fdk-aac-free`, without HE-AAC. Build [fdk-aac](https://github.com/mstorsjo/fdk-aac) and install its `libfdk-aac.so.2` |
| Fedora | Fedora's own `fdk-aac-free` has no HE-AAC. Install `fdk-aac` from [RPM Fusion](https://rpmfusion.org) in its place |
| macOS | Not yet: AAC+ stations play their AAC-LC core. A decoder that uses macOS's own HE-AAC support is planned |
| Windows | Put a 64-bit `fdk-aac.dll` (an MSVC build, such as vcpkg's `fdk-aac`) or `libfdk-aac-2.dll` (MSYS2's `mingw-w64-x86_64-fdk-aac`, with the DLLs it needs) next to `radiotrope.exe` |

On Linux the library is found by the name `libfdk-aac.so.2`, wherever the
system looks for libraries. On Windows it is looked for next to the program
and in the system folder, not on the `PATH`. A build of the library without
HE-AAC is not used. To see which decoder is in use, start with
`RUST_LOG=radiotrope=info`: the log says `AAC decoder: FDK (system)` or
`AAC decoder: built-in AAC-LC` and why.

To build without any FDK code at all, turn the `system-fdk` feature off:
`cargo build --release --no-default-features --features desktop` for the app,
and `--no-default-features` for `radiotrope-cli` and `radiotrope-probe`.
Released binaries never contain or link FDK AAC either way.

### Windows

Windows 10 and 11, on x64 and on ARM64. Each release has two kinds of
download:

| Download | Best for |
|----------|----------|
| `radiotrope-<version>-setup.exe` | Most people. One installer for x64 and ARM64: Start Menu entry, optional desktop icon, uninstaller, updates through WinGet. Installs for your user only (no administrator prompt) unless you choose **Install for all users**. |
| `radiotrope-<version>-windows-x64.zip` / `-arm64.zip` | No install: unzip anywhere and run `radiotrope.exe`. No Start Menu entry and no updates. |

With WinGet, `winget install Radiotrope.Radiotrope` installs it, and
`winget upgrade Radiotrope.Radiotrope` (or `winget upgrade --all`) updates
it. **The app never checks for updates on its own.** Run `winget upgrade`,
or download the new installer from the releases page. The installer closes
a running player, including one in the tray, before it updates.

**Portable mode.** By default the zip uses the same folders as the installed
app, so you can switch between them and keep everything. To keep all of the
app's files in the unzipped folder instead (on a USB stick, say), create an
empty file named `portable` next to `radiotrope.exe`. A file that ends up as
`portable.txt` works too. Settings, favorites, cache and log then go to a
`data` folder next to it, and nothing is written to `%APPDATA%` or
`%LOCALAPPDATA%`. The folder must be one you can write to (not under
`Program Files`). A portable copy runs as a separate player from an
installed one.

**"Windows protected your PC".** The downloads are not code-signed yet, so
SmartScreen warns the first time. Check the download against the
`SHA256SUMS` file on the release page first. In PowerShell:

```powershell
Get-FileHash .\radiotrope-0.1.0-setup.exe    # compare with the line for this file in SHA256SUMS
```

If the hashes match, choose **More info > Run anyway**.

**Full AAC+.** No FDK AAC DLL is included. To use one, put it next to
`radiotrope.exe`, as described in [AAC+ stations](#aac-stations-he-aac).

**Uninstall** from **Settings > Apps > Installed apps**, or with
`winget uninstall Radiotrope.Radiotrope`. The uninstaller asks whether to
delete your settings, favorites and cache too (`%APPDATA%\radiotrope` and
`%LOCALAPPDATA%\radiotrope`). For the zip, delete the folder, plus those two
folders unless you used portable mode.

#### Build from source on Windows

1. Install Rust with [rustup](https://rustup.rs) (the default MSVC toolchain).
2. Install the Visual Studio 2022 Build Tools with the **Desktop development with C++** workload. On ARM64, also add the **C++ Clang Compiler for Windows** component.
3. Install [CMake](https://cmake.org/download/) and make sure it is on `PATH`.
4. Optional: install Git, so **Help > About** shows the commit the build came from.

```powershell
git clone https://github.com/goten002/radiotrope.git
cd radiotrope
cargo build --release
```

The programs are in `target\release\` with an `.exe` ending. The release app
opens without a console window. Started from a terminal it prints its log
there; otherwise the log goes to `%LOCALAPPDATA%\radiotrope\radiotrope.log`
(`data\local\radiotrope.log` in portable mode). See
[packaging/windows](packaging/windows/README.md) for how the downloads are
built.

### macOS (experimental)

macOS 12 Monterey and newer, on Apple Silicon and Intel: one universal
download. Radiotrope builds and runs on macOS, but the Mac version is still
being finished: the menu bar, the Dock, opening playlists from Finder and
full AAC+ are not done yet, and it has had little testing on real Macs.

| Download | What it is |
|----------|------------|
| `radiotrope-<version>-macos-universal.dmg` | Open it and drag **Radiotrope** onto **Applications** |
| `radiotrope-<version>-macos-universal.zip` | The same `Radiotrope.app`, zipped |

Until there are releases with it, the files come from the
[Package (macOS)](https://github.com/goten002/radiotrope/actions/workflows/package-macos.yml)
workflow: open a run and download `dist-macos` (you need to be signed in to
GitHub).

**"Radiotrope can't be opened" / "Apple could not verify…".** The app is not
signed with an Apple Developer ID or notarized yet, so macOS blocks it the
first time. Move it to Applications first, then:

- macOS 15 Sequoia and newer: open it once (macOS refuses), then go to
  **System Settings > Privacy & Security**, scroll down to the message about
  Radiotrope, choose **Open Anyway** and confirm with your password.
- macOS 12 to 14: Control-click (or right-click) **Radiotrope** in
  Applications, choose **Open**, then **Open** again in the dialog.
- Or, in Terminal, remove the "downloaded from the internet" mark:

  ```bash
  xattr -dr com.apple.quarantine /Applications/Radiotrope.app
  ```

Do this only with a file you got from this project, and check it against
the `SHA256SUMS` file of the release first (`shasum -a 256 <file>`). macOS
remembers the choice until the next version.

**Local network.** When the [phone remote](#phone-remote) is on, macOS asks
whether Radiotrope may find devices on your local network. Allow it, or the
phone can't find or control the player. To change it later: **System
Settings > Privacy & Security > Local Network**. With the macOS firewall on,
it also asks once whether to accept incoming connections.

**The terminal player** is inside the app. To run it from Terminal, link it
into your `PATH`:

```bash
sudo ln -s /Applications/Radiotrope.app/Contents/MacOS/radiotrope-cli /usr/local/bin/
```

**Updates.** Radiotrope tells you when a newer version is out (Help > Check
for Updates); download the new DMG and replace the app.

**Uninstall:** move `Radiotrope.app` to the Bin. Your settings and favorites
stay in `~/Library/Application Support/radiotrope/` and the cache in
`~/Library/Caches/radiotrope/`; delete those folders too to remove
everything. See [packaging/macos](packaging/macos/README.md) for how the
downloads are built.

#### Build from source on macOS

```bash
xcode-select --install     # Apple's compilers and tools
brew install cmake         # https://brew.sh
git clone https://github.com/goten002/radiotrope.git
cd radiotrope
cargo build --release
```

The programs are in `target/release/`. `packaging/macos/bundle.sh` makes the
`.app`, DMG and zip from them.

## Using the desktop app

Run `radiotrope`. A second start brings the open window forward instead of
opening another player.

| To... | Go to |
|-------|-------|
| Search the station directory | **Stations > Search Stations** (Ctrl+F), narrowed by country and genre |
| Play a stream by its address (stream, PLS, M3U or HLS) | **Open > Network Stream** |
| Save, reorder and edit favorites | the star on the playing station; drag rows by their logo; right-click a row |
| Move favorites between computers or phones | **Stations > Export / Import Favorites** (an import shows a preview first) |
| Shape the sound | **Tools > Equalizer** and **Tools > Filters** (volume leveler, hiss reducer, hum and rumble filters, smooth switching) |
| Record the station playing | **Tools > Start Recording**; **Tools > Recordings > Settings** for format, bitrate and folder |
| Start, record or stop at set times | **Tools > Scheduler** (once or weekly, with fade-in and an alarm fallback) |
| Stop after a while | **Tools > Sleep Timer** |
| See stream and decoder details | **Tools > Statistics**, or **View > Show Listening Stats** on the favorites |
| Change the look | **View**: Dark, Light or System theme, accent color, visualizer on or off; click the visualizer to change its style (wave, spectrum, mirror, dot matrix, VU, horizontal VU) |
| Keep it in the system tray | **View > System Tray** (show icon, close or minimize to tray, start hidden) |

Scheduled entries and the sleep timer only work while the app is running; they
do not wake the computer. Recordings go to `Music/Radiotrope` unless you pick
another folder.

On GNOME, the tray icon needs the AppIndicator extension.

### Keyboard shortcuts

These shortcuts work in the desktop window. Use **Help > Keyboard Shortcuts**
for the full list, including keys for focused controls.

| Key | Action |
|-----|--------|
| Space | Play or stop. A focused button or row keeps its own Space action. |
| M | Mute or unmute |
| `+` or `=` / `-` | Raise or lower the volume by five percentage points |
| N / P | Play the next or previous favorite, wrapping at the ends |
| ↑ / ↓ | Move the highlight through favorites |
| Enter | Play the highlighted favorite |
| Home / End | Highlight the first or last favorite |
| Page Up / Page Down | Move the favorites highlight by a visible page |
| Shift+F10 / Menu key | Open the highlighted or focused favorite's actions |
| R | Start or stop recording when available |
| F | Add or remove the loaded station as a favorite |
| Ctrl+F | Open Search Stations or focus its search field |
| Ctrl+O | Open Network Stream |
| Ctrl+W | Request to close the window, respecting Close to Tray |
| Ctrl+Q | Quit Radiotrope |
| F1 | Show Keyboard Shortcuts |
| Esc | Close the current dialog or popup, or clear the favorites highlight in the main window |

### Updates

Radiotrope can tell you when a newer version is out, and some downloads can
install it themselves. Nothing is installed without your click.

- **Help > Check for Updates** looks now and shows the answer in About.
- **Help > Check for Updates Automatically** looks once a day, a little after
  the player starts. A newer version shows as a dot on Help, a row in the
  Help menu and a line in About with a link to what's new: never a pop-up.
- A check is one HTTPS request to GitHub for the project's latest release.
  It sends nothing about you or your computer: only the request itself, with
  the `Radiotrope/<version>` User-Agent every request has. GitHub sees your
  IP address, as any website does.

What a newer version means depends on how you installed Radiotrope:

| Install | What the player does |
|---------|----------------------|
| Windows setup program (also through WinGet) | Checks daily. About offers **Download Update**, then **Restart to Update**, which runs the new setup program. `winget upgrade` works as well |
| AppImage | Checks daily. **Download Update**, then **Restart to Update**, which replaces the AppImage file in place. AppImageUpdate and Gear Lever work as well |
| `.deb`, `.rpm`, tarball, Windows portable zip, macOS app, built from source | Tells you when you ask, or daily if you turn that on; get the new file from the release page |
| Flatpak bundle | The same: it tells you, and you install the new bundle |
| AUR (`radiotrope`, `radiotrope-bin`) | Nothing: pacman brings the updates, and Help has no update rows |
| Raspberry Pi appliance | Nothing: a new image brings the updates |

A downloaded update is installed only if it carries the project's signature
([Minisign](https://jedisct1.github.io/minisign/), checked against a public
key inside the player). The first release may not be signed yet; the player
then only tells you about it.

To turn updates down or off:

- For yourself: untick **Check for Updates Automatically**.
- For a computer or a package: set `RADIOTROPE_UPDATES` to `off` (no checks,
  no update rows) or `notify` (never installs) in the player's environment,
  or write that word into a file named `update-mode`: in
  `/etc/radiotrope/` or `<prefix>/share/radiotrope/` on Linux, next to
  `radiotrope.exe` on Windows.
- When building: `RADIOTROPE_UPDATES=off|notify|self make` (or `cargo build`)
  sets the most the build will ever do. Nothing at run time can raise it.
  Distribution packages should build with `off`.

### Where things are saved

Favorites, listening stats, settings, schedules and pairings are JSON files in
the config folder: `~/.config/radiotrope/` on Linux,
`~/Library/Application Support/radiotrope/` on macOS and
`%APPDATA%\radiotrope\` on Windows (in [portable mode](#windows),
`data\config\` next to the program). They are written atomically with a backup,
and two open players see each other's favorite changes.

On macOS the cache (station logos, directory answers) is in
`~/Library/Caches/radiotrope/` and recordings go to `~/Music/Radiotrope/` by
default. Choosing Desktop, Documents or Downloads as the recordings folder
makes macOS ask once whether Radiotrope may use it.

Logging goes through the standard `log` crate: set `RUST_LOG`, for example
`RUST_LOG=radiotrope=debug`, to see more.

## AI agents (MCP)

Every agent talks to the one player running on your computer, so you hear the
same music whichever agent asks. Closing an agent leaves the music playing.

### Set it up

The quickest way is **Tools > Connections > Agents (MCP)**: pick your agent (Claude Code,
Claude Desktop, Codex CLI, Gemini CLI, VS Code or Cursor) and copy the line or
JSON it shows. By hand, the configuration is:

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

| Command | What it does |
|---------|--------------|
| `radiotrope --mcp` | Connects the agent to the running player, starting it first if needed |
| `radiotrope --mcp --standalone` | Gives the agent a separate player of its own |

Then ask things like *"play BBC Radio 1"*, *"find jazz stations from France"*,
*"record this for an hour"* or *"wake me at 7 with my first favorite"*. A robot
chip in the menu bar shows how many agents are using the player; hover it for
details.

### Tools

| Area | Tools |
|------|-------|
| Playback | `play_url`, `play_station`, `play_favorite`, `stop`, `set_volume`, `set_muted`, `get_status` |
| Stations | `search_stations`, `list_categories` |
| Favorites | `list_favorites`, `add_favorite`, `remove_favorite` |
| Recording | `start_recording`, `stop_recording` |
| Timer and Scheduler | `set_sleep_timer`, `list_schedule`, `add_schedule_entry`, `remove_schedule_entry`, `set_schedule_entry_enabled` |

The server is built on [rmcp](https://github.com/modelcontextprotocol/rust-sdk),
the official Rust SDK, and speaks every MCP version from 2024-11-05 to
2026-07-28. Data comes back as structured JSON with output schemas, and every
tool carries hints (read-only, destructive, open-world) that clients use when
asking for confirmation.

### Agents on other computers

Network access (MCP Streamable HTTP) is off until you turn it on in
**Tools > Connections > Agents (MCP)**. There you choose:

- **Listen on**: this computer only (`127.0.0.1`, the default), one network
  interface, or all networks. Port 8765 by default. A chosen interface keeps
  working when its address changes.
- **Authentication**: None, or a **Token** that every request must send as
  `Authorization: Bearer <token>`. The dialog shows a ready line for the agent
  you picked, for example:

  ```bash
  claude mcp add --transport http radiotrope http://192.168.1.20:8765/mcp \
    --header "Authorization: Bearer <token>"
  ```

> [!IMPORTANT]
> The server speaks plain HTTP, meant for your own network. With
> Authentication set to None, anyone who can reach the address controls the
> player, and even a token travels unencrypted. Requests from web pages (with an `Origin` header) are always refused.

The claude.ai and Claude Desktop "custom connectors" connect from Anthropic's
cloud, so they cannot reach a player on your home network. Use Claude Code, or
`radiotrope --mcp` on the same computer.

## Phone remote

Turn on **Tools > Connections > Remote Control**, press **Pair a Phone**, and
scan the QR code with the Radiotrope Remote app, or type the code it shows.
The player announces itself on the local network, so the phone finds it.

A paired phone can search and play stations, manage favorites, listen to the
station on the phone itself, change the equalizer, filters, sleep timer and
Scheduler, start recordings, and list, download or delete them.

## Terminal player

```bash
radiotrope-cli            # your favorites from the app
radiotrope-cli 3          # play favorite number 3
radiotrope-cli capital    # play the first favorite whose name contains "capital"
radiotrope-cli <URL>      # play a stream address
```

| Key | Action |
|-----|--------|
| `↑` `↓` (or `k` `j`), `PgUp` `PgDn`, `Home` `End` | Move through favorites or results |
| `Enter` | Play the highlighted station, submit a query, or retry a search error in the results |
| `Space` | Play or stop |
| `n` `p` | Next and previous favorite |
| `+` / `=` and `-` / `_` | Raise and lower volume |
| `m` | Mute |
| `r` | Start or stop recording (with the app's recording settings) |
| `Ctrl+F` or `/` | Open station search |
| `Tab` | Switch between the query field and results |
| `f` | Add the highlighted station to favorites or ask to remove it |
| `Enter` or `y` | Confirm favorite removal |
| `Esc` or `n` | Cancel favorite removal |
| `Esc` | Close the confirmation, leave search, or quit from favorites |
| `q`, `Ctrl+C`, `Ctrl+Q` | Quit |

Enter a name to search the station directory, or submit a blank query to see
the most-played stations. Press Down or PgDn at the end of the results to load
more. Stars mark favorites. Leaving search keeps playback and recording going.
Letter shortcuts also accept uppercase. In the query field, plain keys such as
`/` and `q` enter text.

The terminal player shares favorites and the directory cache with the
Radiotrope app. It reads the app's settings.

![Radiotrope terminal player](assets/radiotrope_cli.png)

## Station checker (radiotrope-probe)

`radiotrope-probe` connects to a station the way the player does, listens for
a few seconds (no sound card needed) and reports whether it is on air, its
format, its sound level and what is playing.

It is a separate download from the
[releases page](https://github.com/goten002/radiotrope/releases), not part of
any package or installer: `radiotrope-probe-<version>-linux-x86_64.tar.gz`
(`aarch64`) and `radiotrope-probe-<version>-windows-x64.zip` (`arm64`). Each
holds the program, a short README and the licences; the Linux ones also have
its man page and shell completions, under `bin/` and `share/` like the player's
tarball (copy them into `~/.local`). Check it against the release's
`SHA256SUMS` like any other download. To build it from source, see
[Linux: build from source](#linux-build-from-source).

```bash
radiotrope-probe http://example.com/radio.pls       # readable report
radiotrope-probe --json <URL>...                    # JSON for scripts
radiotrope-probe --ndjson --input stations.txt      # one JSON line per station
radiotrope-probe --quiet --listen 5 <URL>...        # one line per station
```

Run `radiotrope-probe --help` for the timeout, parallel jobs and silence level.

| Exit status | Meaning |
|-------------|---------|
| 0 | Up |
| 1 | Degraded: silent, slower than real time, stalling, many decode errors, or ended |
| 2 | Down, with an error code such as `dns`, `connect`, `http_status`, `web_page` or `timeout` |
| 3 | The check can't run |

With several stations the worst one counts, which suits cron, systemd timers,
Task Scheduler and Nagios-style monitors. On Linux it needs the ALSA runtime
library (`libasound2`) even though it plays nothing.

## Raspberry Pi appliance (experimental)

> [!NOTE]
> The Raspberry Pi image is experimental: it is built and tested on one
> hardware setup only, and things may change or break between versions.

The `yocto/` folder builds a minimal Linux image that runs Radiotrope as a
touch-screen kiosk on a Raspberry Pi 3B+ with a 5" DSI display and an I2S
amplifier, including WiFi settings and an on-screen keyboard. See
[yocto/README.md](yocto/README.md) for the hardware and build steps.

## Supported streams and formats

| | Supported |
|-|-----------|
| **Protocols** | Icecast/SHOUTcast (ICY), HLS (MPEG-TS, packed audio), plain HTTP/HTTPS, PLS and M3U playlists |
| **Codecs** | MP3, AAC-LC, HE-AAC (shown as AAC+), Vorbis, Opus, FLAC, WAV/PCM |
| **Song titles** | ICY metadata; ID3v1/ID3v2 tags in MP3, AAC and HLS streams when ICY gives none; legacy text encodings |

Streams reconnect with backoff after network drops, and a stalled or silent
stream is detected and reported.

## Project layout

A Cargo workspace; the main crates are:

| Crate | Kind | Description |
|-------|------|-------------|
| [`radiotrope`](crates/radiotrope) | Library | The streaming engine: resolving, decoding, buffering, DSP, recording, health monitoring. Usable in any Rust program |
| [`radiotrope-app`](crates/radiotrope-app) | Binary | The desktop app, MCP server and phone remote server |
| [`radiotrope-cli`](crates/radiotrope-cli) | Binary | The terminal player, built with [ratatui](https://github.com/ratatui/ratatui) |
| [`radiotrope-probe`](crates/radiotrope-probe) | Binary and library | The station checker |
| [`radiotrope-library`](crates/radiotrope-library) | Library | The station directory and favorites storage the programs share |

`crates/xtask` writes the man pages and shell completions
(`cargo xtask man-and-completions`), and `packaging/linux/` holds the desktop
file and AppStream metainfo that `make install` uses.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

CI runs these on Linux and Windows for every pull request to `main`, and also
checks the Raspberry Pi build
(`cargo check -p radiotrope-app --no-default-features --features embedded`).
Changes are listed in [CHANGELOG.md](CHANGELOG.md).

### UI snapshots

Render the desktop UI into PNGs and an `index.md` with a description of each
frame:

```bash
cargo run -p radiotrope-app --features ui-snapshots --bin radiotrope -- --ui-snapshots /tmp/radiotrope-snapshots
```

The command captures both themes at the default window size. Use
`--theme dark` or `--theme light` for a single theme and `--scale 2` for
double-resolution output. The hidden flag lets the capture reuse the compiled
Slint `App` and its bundled Inter font with a headless software renderer.
It uses fixture data and exits before starting player services or accessing
user settings and favorites.

WiFi Settings and the on-screen keyboard are embedded-only. Native file
pickers and native tray and window menus belong to the operating system.
The snapshots include Radiotrope's own window menu.

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

Radiotrope itself contains no FDK AAC code. It can load the library from your
system at run time (see [AAC+ stations](#aac-stations-he-aac)); the permission
above covers that combination, and building Radiotrope with the library.

This project includes third-party dependencies with different licenses.
See [THIRD-PARTY-LICENSES](THIRD-PARTY-LICENSES) for details.
