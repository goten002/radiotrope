# Releasing Radiotrope

A release is a tag `v<version>` and a GitHub release with the files for
every platform. The workflow only ever makes a **draft**: you look at it,
test it, and publish it yourself.

## Cutting a release

1. **Version.** Set `version` in `[workspace.package]` in `Cargo.toml` (every
   crate inherits it). Pre-releases are `0.2.0-rc.1`; they become GitHub
   pre-releases and are not pushed to the AUR.
2. **Changelog.** Move the `[Unreleased]` items in `CHANGELOG.md` under
   `## [<version>] - <date>`. That section becomes the release notes.
3. **Metainfo.** In `packaging/linux/app.radiotrope.Radiotrope.metainfo.xml`
   add a `<release version="<version>" date="<date>" type="stable">` with a
   short description (pre-releases: `type="development"`).
4. **Yocto recipe.** Rename `yocto/meta-radiotrope/recipes-*/radiotrope/radiotrope_<old>.bb`
   to the new version if the Pi image should carry it.
5. Merge all of that to `main` and wait for CI.
6. **Try it first** (optional): Actions > Release > Run workflow on `main`
   with task `dry-run`. It builds and checks everything and keeps the files
   as the workflow artefact `release-files`; no release, no tag.
7. **Tag:**

   ```bash
   git tag -a v0.1.0 -m "Radiotrope 0.1.0"
   git push origin v0.1.0
   ```

   The Release workflow starts. It stops at once if the tag doesn't match
   `Cargo.toml` or the version ends in `-dev`.
8. **Review the draft** (Releases page, marked Draft): the file list below,
   the notes, `SHA256SUMS`. Download and try the files on real desktops
   (sound, tray, second launch, recording, agents over `--mcp`; see
   "Testing by hand").
9. **Publish** the draft (Edit > Publish release). Publishing is what makes
   the files public and what AppImage updaters see as `latest`.
10. **AUR** (stable releases): Actions > Release > Run workflow, **use workflow
    from: the tag `v<version>`**, task `aur`. See "AUR" below.
11. **WinGet**: `winget.yml` opens the winget-pkgs PR when a release is
    published, once it is turned on (`WINGET_TOKEN` secret and the
    `WINGET_ENABLED` variable; the first version goes in by hand). See
    `packaging/windows/README.md`.

To redo a draft, delete it (or leave it: running the workflow again for the
same version updates the draft's files and notes), fix, move the tag
(`git tag -f`, `git push -f origin v<version>`) and push again. A published
release is never changed by the workflow: it fails instead.

## What the workflows do

| Workflow | When | Does |
|----------|------|------|
| `release.yml` | tag `v*`; by hand (`dry-run`, `draft`, `aur`) | runs the two below, writes `SHA256SUMS`, attests every file, makes or updates the draft |
| `package-linux.yml` | called by `release.yml`; by hand on any branch | builds the Linux files (artefacts `dist-linux-<arch>`, `dist-linux-flatpak-<arch>`) and the smoke tests |
| `package-windows.yml` | called by `release.yml`; by hand | the Windows files (artefacts `dist-windows-x64`, `dist-windows-arm64`, `dist-windows-installer`); see `packaging/windows/README.md` |
| `package-macos.yml` | by hand; PRs that change `packaging/macos/` | the macOS files (artefact `dist-macos`), unsigned; **not called by `release.yml` yet** (see "macOS" below) |
| `winget.yml` | a release is published (when enabled) | the WinGet submission |
| `ci.yml` | pushes and PRs to `main` | build, tests, lint; the Linux install and packaging-script checks |

The Linux builds run natively on `ubuntu-24.04` (x86_64) and
`ubuntu-24.04-arm` (aarch64), inside an `ubuntu:22.04` container, so the
programs need glibc 2.35 or newer. `packaging/linux/dist.sh` does the work
and runs the same way on your machine in that container:

```bash
docker run --rm -it -v "$PWD:/src" -w /src ubuntu:22.04 bash
packaging/linux/build-deps.sh
curl -sSf https://sh.rustup.rs | sh -s -- -y && . ~/.cargo/env
cargo install --locked cargo-deb@3.8.0 cargo-generate-rpm@0.21.1
packaging/linux/dist.sh            # files in target/dist
```

(It writes `target/` as root; use a copy of the checkout.)

The smoke jobs install each file in clean containers and check that
`radiotrope`, `radiotrope-cli` and `radiotrope-probe` print their version:

| File | Tried on |
|------|----------|
| .deb, tarball | Debian 12, 13, Ubuntu 22.04, 24.04 (x86_64); Debian 12, Ubuntu 24.04 (aarch64) |
| .rpm | Fedora (both), openSUSE Tumbleweed (x86_64) |
| AppImage | Ubuntu 22.04 (both), Fedora, Arch, openSUSE Tumbleweed (x86_64), unpacked (`--appimage-extract-and-run`: no FUSE in containers) |
| AUR | `PKGBUILD` built from the commit with makepkg, `PKGBUILD-bin` from the tarball, both through `namcap`, installed (Arch, x86_64) |
| Flatpak | installed and run in the build container |

None of them opens a window or plays sound: that is for "Testing by hand".

## The files

| File | What it is |
|------|------------|
| `radiotrope-<ver>-linux-<arch>.tar.gz` | `bin/` (three programs), `share/` (launcher, icons, metainfo, man pages, bash/zsh/fish completions), README, licences. Copy into `~/.local` or `/usr/local`. Also the source of `radiotrope-bin` |
| `radiotrope_<ver>-1_<amd64\|arm64>.deb` | Debian/Ubuntu package `radiotrope` (cargo-deb). Depends found by `dpkg-shlibdeps` plus `libxkbcommon0`, `ca-certificates`; Recommends the display libraries and `libfdk-aac2` |
| `radiotrope-<ver>-1.<x86_64\|aarch64>.rpm` | Fedora/openSUSE package (cargo-generate-rpm). Requires by soname; Recommends `fdk-aac` or `libfdk-aac2` |
| `Radiotrope-<ver>-<arch>.AppImage` | linuxdeploy + appimagetool, static runtime (no libfuse2). Carries libxkbcommon; ALSA, fontconfig, GL and the display libraries come from the system |
| `Radiotrope-<ver>-<arch>.AppImage.zsync` | Delta updates. The AppImage's update information is `gh-releases-zsync\|goten002\|radiotrope\|latest\|Radiotrope-*-<arch>.AppImage.zsync`: updaters look at the latest **published** release |
| `Radiotrope-<ver>-<arch>.flatpak` | Single-file Flatpak bundle (freedesktop 26.08), manifest in `packaging/flatpak/`. Not on Flathub |
| Windows files | portable zips and the installer: see `packaging/windows/README.md` |
| macOS files | not in releases yet: see "macOS" below and `packaging/macos/README.md` |
| `SHA256SUMS` | SHA-256 of every file above |

A pre-release (`0.2.0-rc.1`) has not been through the workflow yet: check
how cargo-deb and cargo-generate-rpm write its version (it must sort before
`0.2.0`) the first time.

## Checking a download

Checksums:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

Provenance: every file has a GitHub build attestation (Sigstore, no keys to
manage), proving it was built by this repository's Release workflow from a
given commit:

```bash
gh attestation verify Radiotrope-0.1.0-x86_64.AppImage --repo goten002/radiotrope
# or, for a file attached to a published release
gh release verify-asset v0.1.0 Radiotrope-0.1.0-x86_64.AppImage --repo goten002/radiotrope
```

The files the player installs by itself (the Windows setup program and the
AppImages) and `SHA256SUMS` also have a Minisign signature beside them, once
the signing key is set up (next section):

```bash
minisign -Vm Radiotrope-0.1.0-x86_64.AppImage -P <the public key>
```

Nothing is GPG-signed yet; that comes with an apt/dnf repository.

## Signing the updates

The player's own update (Help > Check for Updates, then Download Update in
About) installs a download only when it is signed with the project's
Minisign key. Until the steps below are done, every build only *tells* about
a newer version: the public key in the source is empty, and the workflow's
signing step is skipped.

One time, on your own computer (minisign 0.11 or newer):

```bash
# 1. Make the key pair. -W: no password, so the workflow can use it.
minisign -G -W -p radiotrope-minisign.pub -s radiotrope-minisign.key

# 2. The secret key goes into the repository's secrets, and nowhere else
gh secret set UPDATE_SIGNING_KEY --repo goten002/radiotrope < radiotrope-minisign.key

# 3. Keep a copy of radiotrope-minisign.key offline (a password manager, a
#    USB stick in a drawer), then delete it from the disk. Whoever has it
#    can sign updates that every installed player accepts.

# 4. The public key is the second line of the .pub file (56 characters,
#    starting with RW)
tail -n1 radiotrope-minisign.pub
```

Then paste that line into `PUBLIC_KEY` in
`crates/radiotrope-app/src/update/verify.rs`, commit it, and release. From
that release on:

- the workflow's "Sign the update files" step writes
  `radiotrope-<ver>-setup.exe.minisig`, `Radiotrope-<ver>-<arch>.AppImage.minisig`
  and `SHA256SUMS.minisig`, each with the trusted comment `file:<name>`;
- players built with the key offer **Download Update** for a newer release,
  and install it only if its `.minisig` matches. A release without
  signatures (the secret was missing) is only told about.

The key can't be changed quietly: players in the field trust the key they
were built with. To replace a lost or leaked key, put the new public key in
a release signed with the old one (so players update to it), then switch the
secret. If the old key is gone, users have to install that release by hand.

A dry run of the Release workflow signs too when the secret is there, so the
step can be tried before a tag exists. To try the update itself, see
"Testing by hand".

## Secrets

| Secret | For | Notes |
|--------|-----|-------|
| `AUR_SSH_KEY` | the `aur` task | Private half of an SSH key added to the AUR account (My Account > SSH Public Key). Use a key made for this only: `ssh-keygen -t ed25519 -f aur -C radiotrope-ci`. Settings > Secrets and variables > Actions > New repository secret |
| `SNAPCRAFT_STORE_CREDENTIALS` | uploading the snap | `snapcraft export-login` output (see "Snap"). Without it the upload is skipped |
| `UPDATE_SIGNING_KEY` | signing the update files | The whole Minisign secret key file, made without a password (see "Signing the updates"). Without it the release is unsigned and players only tell about it |
| `WINGET_TOKEN` | `winget.yml` | A classic token with `public_repo`, from the account whose winget-pkgs fork the PR comes from. Also set the variable `WINGET_ENABLED=true` |

`GITHUB_TOKEN` covers the release itself and the attestations; nothing else
is needed for Linux.

## AUR

Two packages, both maintained from `packaging/arch/`:

- `radiotrope` (`PKGBUILD`): builds the tag's source archive with the
  system's Rust.
- `radiotrope-bin` (`PKGBUILD-bin`): installs the release tarball.

The `aur` task (`packaging/arch/aur-publish.sh`) runs in an Arch container:
sets `pkgver`, fills in the real checksums with `updpkgsums` (from the tag's
archive and the published tarballs, checked against `SHA256SUMS`), writes
`.SRCINFO` and pushes both to `ssh://aur@aur.archlinux.org/<name>.git`. Run
it only after the release is **published** (the tarball URLs must work), and
from the tag. It refuses pre-releases.

First time: make the AUR account, add the key's public half to it, store the
private half as `AUR_SSH_KEY`. The first push creates the two packages. If
you'd rather do the first push by hand:

```bash
cd packaging/arch
sed -i 's/^pkgver=.*/pkgver=0.1.0/' PKGBUILD && updpkgsums && makepkg --printsrcinfo > .SRCINFO
git clone ssh://aur@aur.archlinux.org/radiotrope.git aur-radiotrope
cp PKGBUILD .SRCINFO aur-radiotrope/ && cd aur-radiotrope && git add . && git commit -m "Initial upload" && git push
```

`DRY_RUN=1 packaging/arch/aur-publish.sh` does everything except the push.

## Flatpak

The bundle is built from `packaging/flatpak/app.radiotrope.Radiotrope.yml`
with the crates from `cargo-sources.json` (made at build time by
`packaging/flatpak/cargo-sources.sh`, so no network during the build). The
app is not submitted to Flathub.

Sandbox permissions: Wayland with X11 fallback, GPU, network (streams, the
agents and phone servers, mDNS), PulseAudio (ALSA goes through it), the
StatusNotifierWatcher D-Bus name (tray), and `~/Music/Radiotrope`. No
notification permission: the app sends none.

What the app still needs to work fully in the sandbox (code changes, not done
yet):

1. **Single instance**: the lock and socket live in `$XDG_RUNTIME_DIR/radiotrope/`
   (`instance.rs`), which is private to each `flatpak run`. Use
   `$XDG_RUNTIME_DIR/app/$FLATPAK_ID/` when `FLATPAK_ID` is set: shared by all
   instances, writable without extra permissions. Fixes a second launch
   starting a second player.
2. **The agents' command** (`mcp/setup.rs`): show `flatpak run
   app.radiotrope.Radiotrope --mcp` instead of `/app/bin/radiotrope --mcp`.
3. **The `--mcp` relay starting the player** (`instance.rs`): a player spawned
   by the relay lives and dies with the agent's sandbox instance. In Flatpak,
   don't spawn: answer "Start Radiotrope first". Later: D-Bus activation.
4. **Second launch raising the window on Wayland** (review P-07), after 1.
5. **Autostart** through the Background portal, when "Start at Login" exists.
6. **AAC+**: the runtime has no libfdk-aac, so AAC+ plays as AAC-LC.

## macOS

macOS is experimental: `package-macos.yml` builds the files, but
`release.yml` doesn't call it, so a release has no macOS download yet. The
files are unsigned (ad hoc only) until the Apple Developer account exists
(macOS review, phase 3).

To build them for a version:

1. Actions > **Package (macOS)** > Run workflow, on `main` or the tag, with
   the version (or empty for the workspace version; a version that doesn't
   match `Cargo.toml` stops the run).
2. The **Build (universal)** job runs `packaging/macos/bundle.sh dist` on
   `macos-15`: release builds of the player and the CLI for `aarch64-apple-darwin`
   and `x86_64-apple-darwin` (`MACOSX_DEPLOYMENT_TARGET=12.0`), `lipo` into
   universal programs, the `Radiotrope.app` bundle (Info.plist from
   `packaging/macos/`, icon, `update-mode` = `notify`, licences, man pages,
   completions), an ad-hoc signature, then the DMG and the zip. It fails if a
   program lacks an architecture, asks for another macOS, or links anything
   outside the system (FDK, Homebrew). The run's summary has the sizes.
3. The **Smoke test** jobs mount the DMG on Apple Silicon and on Intel,
   verify the signature and the architectures, run the CLI,
   and start the app (screenshots in `macos-package-smoke-<arch>`).
4. Download the artefact `dist-macos`:
   `radiotrope-<ver>-macos-universal.dmg` and `.zip`. To attach them to a
   draft by hand, add them to the draft's files and their lines to
   `SHA256SUMS` (`shasum -a 256 *.dmg *.zip`).

The same on a Mac: `rustup target add aarch64-apple-darwin
x86_64-apple-darwin`, `brew install cmake`, then
`packaging/macos/bundle.sh` (files in `target/dist`).

Still to do before macOS is a release platform (phase 3): Developer ID
signing with `packaging/macos/entitlements.plist` and the hardened runtime,
notarization (`notarytool`) and stapling in the placeholder step of
`package-macos.yml`, the Apple secrets in "Secrets", `release.yml` calling
the workflow, and the Homebrew tap's cask.

Testing by hand on a Mac: open the DMG, drag the app to Applications, open
it past Gatekeeper (README, "macOS (experimental)"); sound; the menu bar
icon; Local Network prompt with the phone remote on; record; `open -a
Radiotrope` a second time brings the window forward; the CLI from
`Contents/MacOS` in Terminal; Help > About shows the version and that
updates are notify-only.

## Snap

Built from `packaging/snap/snapcraft.yaml` by `package-snap.yml` (amd64 and
arm64, with `snapcore/action-build`), which also installs it with snapd on
the runner, runs the three programs confined and checks two public streams
with the probe. To build one locally without LXD or Multipass:
`packaging/snap/build-docker.sh` (Docker; the .snap lands in `target/dist`,
and the volume `radiotrope-snap-cache` keeps rustup and the crates; remove it
with `docker volume rm radiotrope-snap-cache`).

The snap is strictly confined, built without `system-fdk` (AAC+ plays as
AAC-LC; nothing of FDK is in it) and with `RADIOTROPE_UPDATES=off`.
Interfaces: `network`, `network-bind`, `audio-playback`, `home`,
`removable-media` (connected by hand), `unity7` (tray), and the gnome
extension's `desktop`, `desktop-legacy`, `wayland`, `x11`, `opengl`,
`gsettings` and `gpu-2404`. None of them needs the store's manual review.

Nothing publishes it yet. To put it in the Snap Store (once):

1. Make a Snap Store account at <https://snapcraft.io/account> (an Ubuntu
   One login), and set the publisher name to "Radiotrope" there.
2. Install snapcraft (`sudo snap install snapcraft --classic`), then
   `snapcraft login` and `snapcraft register radiotrope`. A name that is
   taken or a trademark goes to a review on the forum.
3. Make a login for CI, limited to this snap and the early channels, and
   store it as the repository secret `SNAPCRAFT_STORE_CREDENTIALS`:

   ```bash
   snapcraft export-login --snaps=radiotrope \
       --acls package_access,package_push,package_update,package_release \
       --channels edge,beta exported.txt
   gh secret set SNAPCRAFT_STORE_CREDENTIALS < exported.txt && rm exported.txt
   ```

4. In `release.yml`, call the workflow with a channel (it already has the
   upload job; without the secret it skips it):

   ```yaml
   snap:
     uses: ./.github/workflows/package-snap.yml
     with:
       store-channel: edge
     secrets: inherit
   ```

5. Channels: each release goes to **edge**; after a check on a desktop
   (`sudo snap install radiotrope --edge`), promote it to **beta** on
   snapcraft.io (Releases) or with `snapcraft release radiotrope <revision>
   beta`, then to **stable**. Stable needs `grade: stable` in
   `snapcraft.yaml`: change it for the first stable release.
6. Optional: ask on <https://forum.snapcraft.io> (category store-requests)
   for the auto-aliases `radiotrope-cli` and `radiotrope-probe`, so the
   tools keep their usual names.

## Testing by hand

On GNOME and KDE (Wayland), and one X11 session, with each format you
changed: sound; tray icon and its menu; a second launch raises the window;
record to the default folder; play an AAC+ station with and without
libfdk-aac installed (`RUST_LOG=radiotrope=info` logs the decoder); an agent over `--mcp` with the player running and not running.
Snap: sound, the tray icon, Start at Login (log out and in), the recordings
folder and Open Folder, a file dialog (Import), an agent with
`/snap/bin/radiotrope --mcp`, and `snap connections radiotrope`.
AppImage: update from the previous release with AppImageUpdate or Gear Lever
once this one is published.

The player's own update (from the second signed release on): with the
previous release installed, Help > Check for Updates, Download Update, then
Restart to Update.
- Windows, installed for the current user: the setup program's progress
  window shows, the player closes and comes back as the new version. Repeat
  with an install for all users (it asks for administrator rights, and the
  player must come back *not* elevated) and through WinGet (`winget list
  Radiotrope` shows the new version afterwards).
- AppImage, x86_64 and aarch64: the file keeps its name and place, and the
  player comes back as the new version. Try it once from a folder you can't
  write to: the player must only tell about the update.
- A `.deb` or tarball install, and the portable zip: About shows the newer
  version and What's New, with no button.
- `RADIOTROPE_UPDATES=off radiotrope`: Help has no update rows.
- Start Restart to Update while recording: the player asks to stop the
  recording first.
