# Windows packaging

What the Windows downloads are, how they are built, and the identifiers that
must never change.

| File | What it is | Made by |
|------|------------|---------|
| `radiotrope-<ver>-windows-x64.zip` | Portable zip: the three programs, `README.txt`, `LICENSE.txt`, `THIRD-PARTY-LICENSES.txt` in a `Radiotrope\` folder | `stage.ps1` |
| `radiotrope-<ver>-windows-arm64.zip` | The same for Windows on ARM | `stage.ps1` |
| `radiotrope-<ver>-setup.exe` | One Inno Setup installer for x64 and ARM64 | `radiotrope.iss` |

All three are built by `.github/workflows/package-windows.yml`, which
`release.yml` calls with the release version. It uploads them as the
artefacts `dist-windows-x64`, `dist-windows-arm64` and
`dist-windows-installer`. Nothing here publishes anything.

Nothing is code-signed yet, so SmartScreen warns on the installer and on the
programs from the zip. The README tells users how to check the SHA-256
against the release's `SHA256SUMS`. Signing comes later. When it does, sign
the three exes before `stage.ps1` zips them, then sign the setup program.

No FDK AAC DLL is shipped. A user can put `fdk-aac.dll` or `libfdk-aac-2.dll`
next to `radiotrope.exe` for full HE-AAC (see the main README). `stage.ps1`
fails if an exe links FDK.

## Permanent identifiers

**Never change these after the first release.** Windows and WinGet use them
to find an earlier install for upgrades and uninstall.

| What | Value |
|------|-------|
| Inno Setup `AppId` | `{F8B5B3D8-4BAC-4AE4-9925-5AA527345B2E}` |
| Uninstall registry key (`ProductCode` in WinGet) | `{F8B5B3D8-4BAC-4AE4-9925-5AA527345B2E}_is1` |
| Name in Installed apps (`UninstallDisplayName`) | `Radiotrope` |
| Publisher | `Radiotrope` |
| WinGet package id | `Radiotrope.Radiotrope` |
| Per-user install folder | `%LOCALAPPDATA%\Programs\Radiotrope` |
| All-users install folder | `%ProgramFiles%\Radiotrope` |

## The build

- **x64** builds on `windows-latest`, **ARM64** builds natively on
  `windows-11-arm`, both with the `release` profile and `--locked`.
- **Static C runtime:** `RUSTFLAGS=-C target-feature=+crt-static`. The `cc`
  crate follows that flag, and `opusic-sys` passes it to the CMake build of
  libopus as `OPUS_STATIC_RUNTIME`. `stage.ps1` fails if any exe imports
  `vcruntime*`, `msvcp*`, `ucrtbase` or `api-ms-win-crt-*`, so the programs
  run without the Visual C++ Redistributable.
- **clang-cl on ARM64:** `aws-lc-sys` (rustls's crypto) can't build its ARM64
  code with MSVC's `cl.exe`. The workflow puts LLVM's `clang-cl` on `PATH`,
  but only while `aws-lc-sys` is in `Cargo.lock`.
- **The AAC+ tests are skipped on Windows ARM64.** The engine's AAC+ tests
  encode their samples with the `fdk-aac` crate, a dev-dependency. FDK's
  header doesn't recognise MSVC's ARM64 macro and stops the build, so that
  dev-dependency and the test module are left out on that target only. The
  programs themselves never contain FDK.
- `stage.ps1` also checks each exe's PE machine type against the architecture.

To build locally (in PowerShell, from the repository root, with Inno Setup 6.3
or newer):

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc -p radiotrope-app -p radiotrope-cli -p radiotrope-probe
packaging\windows\stage.ps1 -Target x86_64-pc-windows-msvc -Arch x64 -Version 0.1.0
# the same for aarch64-pc-windows-msvc / arm64, on an ARM64 PC or with the ARM64 tools
iscc /DAppVersion=0.1.0 packaging\windows\radiotrope.iss    # writes dist\radiotrope-0.1.0-setup.exe
```

The installer needs both staged folders. To test an x64-only installer, pass
`/DSourceArm64=` the x64 folder. Never release an installer made that way.

## Portable mode

The zip does not include the portable marker, so it runs in normal mode and
shares its data with an installed copy. Portable mode starts when a file
named `portable` or `portable.txt` sits next to the exe. All data then goes to
`data\config`, `data\cache` and `data\local` beside the exe, and nothing is
written to `%APPDATA%` or `%LOCALAPPDATA%`. The code is
`data_dirs()` in `crates/radiotrope-library/src/data/storage.rs`. Every place
that saves a file gets its folder from that function: settings, favorites,
stats, schedules, caches, the log, the instance lock, the agent token and
paired phones.

A portable copy uses its own lock and named pipe, so it runs as a separate
player from an installed copy. Recordings still go to the folder chosen in
the app (by default `Music\Radiotrope`).

## The installer

- **Install scope:** per-user by default, with no administrator prompt. The
  first page offers "Install for all users" (Program Files, one UAC prompt).
  On the command line, use `/CURRENTUSER` or `/ALLUSERS`.
- **Architecture:** the setup program is 32-bit x86, so it runs on both
  architectures. It installs the ARM64 programs on ARM64 Windows
  (`Check: IsArm64`) and the x64 programs everywhere else.
- **Upgrade:** before replacing any files, the installer runs
  `"{app}\radiotrope.exe" --quit`. This closes a running player, including one
  hidden in the tray, and waits up to 20 seconds for it to finish a
  recording and exit. The Restart Manager (`CloseApplications=yes`) is the
  fallback.
- **Shortcuts:** a Start Menu entry, plus a desktop icon if the user ticks
  the task for it (off by default).
- **Uninstall:** the player is closed with `--quit` first. After that the
  uninstaller asks whether to delete `%APPDATA%\radiotrope` and
  `%LOCALAPPDATA%\radiotrope`, and the answer defaults to No. A silent
  uninstall (WinGet's) keeps the data unless `/DELETEDATA` is passed.
- **Silent install:** `radiotrope-<ver>-setup.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART [/CURRENTUSER | /ALLUSERS]`.

The `smoke` job in `package-windows.yml` runs on x64 and on ARM64. It
installs silently and checks the exe architecture, `radiotrope-cli --version`,
the uninstall entry and the Start Menu entry. It also runs `--quit` with
nothing running. It then starts the player, upgrades over it, and checks that
the player exited. Last, it uninstalls silently and checks that nothing is
left.

## WinGet

The templates in `winget/` describe `Radiotrope.Radiotrope` with installer
type `inno`. One universal installer is listed four times: x64 and arm64,
each as `user` (`/CURRENTUSER`) and `machine` (`/ALLUSERS`). The download URL
is `https://github.com/goten002/radiotrope/releases/download/v<ver>/radiotrope-<ver>-setup.exe`.
WinGet only updates when the user runs `winget upgrade`, and the app never
checks for updates itself.

### First submission (by hand, once)

1. Publish the GitHub release. The installer URL must work.
2. On Windows, from the repository root:
   ```powershell
   packaging\windows\winget\render.ps1 -Version 0.1.0     # downloads the installer for its SHA-256
   winget validate --manifest dist\winget\manifests\r\Radiotrope\Radiotrope\0.1.0
   winget settings --enable LocalManifestFiles             # once, as administrator
   winget install --manifest dist\winget\manifests\r\Radiotrope\Radiotrope\0.1.0
   ```
3. Fork `microsoft/winget-pkgs`. Copy the folder `manifests/r/Radiotrope/Radiotrope/0.1.0/`
   into the fork and open a pull request titled
   `New package: Radiotrope.Radiotrope version 0.1.0`. Automated checks and
   a moderator review follow, which can take from hours to a few days.
   `wingetcreate new <installer URL>` is the interactive alternative. If you
   use it, give it the same id, publisher and ProductCode as above.

### Later versions (automatic, once enabled)

`.github/workflows/winget.yml` runs `wingetcreate update --submit` when a
release is published. It is off by default. To enable it after the first
manifest is merged:

1. Create a classic GitHub token with the `public_repo` scope and add it as
   the repository secret `WINGET_TOKEN`.
2. Set the repository variable `WINGET_ENABLED` to `true`.

It can also be run by hand from the Actions tab, for a given version.

## Not done yet: needs a real Windows or ARM64 device

- The GUI on Windows on ARM: the OpenGL renderer on Snapdragon drivers (it
  falls back to the software renderer), audio and the tray icon.
- Upgrading over a player that is playing or recording, and the tray icon
  going away. CI starts the player (software renderer, no sound device),
  then upgrades over it. The player exits on both x64 and ARM64.
- The interactive wizard: the scope page, the delete-my-data question and the
  desktop-icon task.
- The SmartScreen text when an unsigned installer is downloaded through Edge.
- Portable mode from a USB stick, watched with Process Monitor: nothing
  should be written under `%APPDATA%` or `%LOCALAPPDATA%\radiotrope`.
