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

Nothing is GPG-signed yet; that comes with an apt/dnf repository.

## Secrets

| Secret | For | Notes |
|--------|-----|-------|
| `AUR_SSH_KEY` | the `aur` task | Private half of an SSH key added to the AUR account (My Account > SSH Public Key). Use a key made for this only: `ssh-keygen -t ed25519 -f aur -C radiotrope-ci`. Settings > Secrets and variables > Actions > New repository secret |
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

## Testing by hand

On GNOME and KDE (Wayland), and one X11 session, with each format you
changed: sound; tray icon and its menu; a second launch raises the window;
record to the default folder; play an AAC+ station with and without
libfdk-aac installed (`RUST_LOG=radiotrope=info` logs the decoder); an agent over `--mcp` with the player running and not running.
AppImage: update from the previous release with AppImageUpdate or Gear Lever
once this one is published.
