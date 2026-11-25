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
  Resources/Radiotrope.icns  from assets/icons
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
- M3U, M3U8, PLS and XSPF playlists as `Viewer` with `LSHandlerRank`
  `Alternate`: Radiotrope is in Finder's Open With, never the default. The
  app doesn't handle an opened file yet (phase 2: it arrives as an Apple
  Event, not as an argument).

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

The icon's 1024 px picture (512@2x) is the 512 px one scaled up: a
macOS-shaped icon (a rounded square on a 1024 canvas) is still to be drawn.

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
   Services, as Finder does). Screenshots and logs are kept as
   `macos-package-smoke-<arch>`.

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
