# macOS packaging

What the macOS downloads are, how they are built, and the identifiers that
must never change. macOS is **experimental**: the downloads are not signed
with a Developer ID certificate or notarized yet, so Gatekeeper stops them
(main README, "macOS (experimental)").

| File | What it is | Made by |
|------|------------|---------|
| `radiotrope-<ver>-macos-universal.dmg` | Disk image with `Radiotrope.app` and a link to `/Applications`, to drag it onto | `bundle.sh` |
| `radiotrope-<ver>-macos-universal.zip` | `Radiotrope.app` alone (`ditto`), for the Homebrew cask and a later self-update | `bundle.sh` |

Both hold the same universal bundle (Apple Silicon and Intel), for macOS 12
and newer. `.github/workflows/package-macos.yml` builds them on a pull
request that changes this folder or by hand, and uploads them as the
artefact `dist-macos`. Nothing here publishes anything; `release.yml` doesn't
call it yet.

## The bundle

```
Radiotrope.app/Contents/
  Info.plist                 this folder's Info.plist, version filled in
  PkgInfo
  MacOS/radiotrope           the player (universal: lipo of arm64 + x86_64)
  MacOS/radiotrope-cli       the terminal player
  Resources/Radiotrope.icns  from assets/icons/macos (see "Icons")
  Resources/update-mode      "notify": About says when a newer version is out
  Resources/LICENSE, THIRD-PARTY-LICENSES
  Resources/man/man1/        man pages of the two programs
  Resources/completions/     bash, zsh, fish
```

The CLI is inside the bundle rather than in a separate download: one file
for everyone, and the Homebrew cask (phase 3) links it into the PATH with a
`binary` stanza, the man pages with `manpage`, and the
completions with `bash_completion`, `zsh_completion` and `fish_completion`.
Without Homebrew:

```bash
sudo ln -s /Applications/Radiotrope.app/Contents/MacOS/radiotrope-cli /usr/local/bin/
```

`Info.plist` declares:

- `LSMinimumSystemVersion` 12.0, the same as `MACOSX_DEPLOYMENT_TARGET` in
  `bundle.sh` (which checks each program's `minos`);
- the Local Network prompt text and the Bonjour service `_radiotrope._tcp`
  (the phone remote), and `NSAppSleepDisabled` (no App Nap, so scheduled
  recordings start on time);
- M3U, M3U8 and PLS playlists as `Viewer` with `LSHandlerRank`
  `Alternate`: Radiotrope is in Finder's Open With, never the default. An
  opened file arrives as an Apple event, not as an argument
  (`application:openURLs:`, `macos_app.rs`), and its first `http(s)`
  address plays (`open_file.rs`). XSPF is not declared: the player doesn't
  read it.

`entitlements.plist` is for the Developer ID signature (phase 3). The ad-hoc
signature made today doesn't use it.

## Building

On a Mac with Xcode's command line tools (`xcode-select --install`), CMake
(`brew install cmake`) and Rust:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
packaging/macos/bundle.sh                 # files in target/dist
ARCHS=aarch64 packaging/macos/bundle.sh   # one architecture, quicker
SKIP_BUILD=1 packaging/macos/bundle.sh    # reuse the release builds
```

`bundle.sh` builds the two programs for both targets (release profile,
`--locked`), joins each pair with `lipo`, and fails if a program is missing an
architecture, asks for another macOS than 12.0, or links a library outside
`/usr/lib` and `/System` (Homebrew, FDK AAC). It then writes the bundle, the
icon (`iconutil`), signs it ad hoc (`codesign --force --deep --sign -`; arm64
code doesn't run unsigned), and makes the DMG (`hdiutil`, UDZO) and the zip
(`ditto`).

## Icons

Both pictures are in `assets/icons/macos/` (its README says where they come
from):

- **The app icon** (Dock, Finder, the DMG): `radiotrope-macos-1024.png`, the
  logo on a white rounded tile, 824 px on a 1024 px canvas with the margin
  and shadow of Apple's layout. `bundle.sh` scales it with `sips -z` into
  the whole `.iconset` (16, 32, 128, 256 and 512 pt, each also @2x; 512@2x
  is the picture itself), prints the ten sizes and makes
  `Radiotrope.icns` with `iconutil`. To change the icon, replace the PNG
  (1024 x 1024, or the script stops) and build again. The Linux and Windows
  icons (`assets/icons/icon-*.png`, `radiotrope.ico`) are not used here.
- **The menu bar glyph**: `tray-template.png`, 64 x 64, black on
  transparent. It is compiled into the program (`tray.rs`) and shown as a
  template image: macOS uses only its shape (the alpha channel) and draws it
  black on a light menu bar, white on a dark one, and tinted where the bar
  is. **To swap it, copy another PNG over that file** and build again: any
  size works (it is scaled to the bar's 18 pt; 36 px or more stays sharp on
  Retina), black on a transparent background, no text. Colours in the
  file are ignored; how opaque each pixel is counts, so a glyph may have
  lighter parts (partial alpha).

## What CI checks

`package-macos.yml`:

1. **Build** on `macos-15` (Apple Silicon); the x86_64 half is
   cross-compiled. Uploads `dist-macos`; the run's summary lists the sizes.
2. **Smoke test** on `macos-15` (arm64) and `macos-15-intel` (x86_64): mounts
   the DMG, checks the Applications link, `codesign --verify --deep
   --strict` and that the signature is ad hoc, both architectures in each
   program, the bundle id, version and `update-mode`; runs the CLI with
   `--version`; copies the app to an Applications folder, starts
   it for 15 seconds (screenshot, log), then again through `open` (Launch
   Services, as Finder does). Then `scripts/macos-integration-check.sh`
   with the app from the DMG: a playlist opened with `open -a` starts the
   app and plays, a second `open -a` and a second run of the program start
   no second process and the running app is told to show its window, a
   `.pls` and a playlist with no station in it are answered while it runs,
   and `radiotrope --start-at-login on` writes the LaunchAgent file, which
   launchd accepts and starts the app from. Screenshots and logs are kept
   as `macos-package-smoke-<arch>` (`integration.txt` is that check's
   result).

`ci-macos.yml` runs the same script on every pull request, with a bundle
made around the debug build. The script also runs by hand on a Mac (its
header says what it changes in your profile).

A step "Sign and notarize (phase 3, not set up)" is in the build job and
never runs: it is where the Developer ID signature, `notarytool` and
`stapler` go once the Apple account and its secrets exist.

## Permanent identifiers

**Never change these after the first release.** macOS keys permissions
(Local Network, firewall, Login Items), preferences and file handlers on the
bundle id; the Homebrew cask and users' Dock entries on the bundle name.

| What | Value |
|------|-------|
| `CFBundleIdentifier` | `app.radiotrope.Radiotrope` |
| Bundle name | `Radiotrope.app` |
| Main program (`CFBundleExecutable`) | `radiotrope` |
| Bonjour service | `_radiotrope._tcp` |
| Settings folder | `~/Library/Application Support/radiotrope/` |
| Start at Login | `~/Library/LaunchAgents/app.radiotrope.Radiotrope.plist`, label `app.radiotrope.Radiotrope` |
