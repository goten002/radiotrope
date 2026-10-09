# Radiotrope Yocto Build

Builds a minimal embedded Linux image for running Radiotrope as a kiosk appliance on Raspberry Pi 3B+.

## Target Hardware

- **Raspberry Pi 3B+** — quad-core Cortex-A53, 1GB RAM
- **Waveshare 5" DSI LCD (B)** — 800x480 capacitive touchscreen
- **InnoMaker HiFi AMP Pro** — MA12070P I2S amplifier HAT (2x40W RMS)

See [hardware.md](hardware.md) for detailed specs, wiring, and device tree configuration.

## Prerequisites

- [kas](https://kas.readthedocs.io/) build tool: `pip install kas`
- [podman](https://podman.io/) or Docker (for container builds on non-Debian hosts)
- ~50 GB free disk space (first build)

## Build

From the **project root** (not `yocto/`):

```bash
kas-container build kas-radiotrope-pi3.yml
```

This is the **release image**: no SSH unless you give a public key (below),
no root password, and the player runs as its own `radiotrope` user.

For development, the **dev image** adds passwordless root login over SSH.
Anyone on the same network can then log in as root, so never ship it:

```bash
kas-container build kas-radiotrope-pi3-dev.yml
```

### SSH with a key

Put your public key in `.config-seed/authorized_keys` (git ignores it) and
build the release image. The image then runs an SSH server where root logs
in with that key only, never with a password:

```bash
cp ~/.ssh/id_ed25519.pub .config-seed/authorized_keys
kas-container build kas-radiotrope-pi3.yml
ssh root@<pi address>
```

Without that file the release image has no SSH server.

Or use the convenience script:

```bash
./yocto/scripts/setup-build.sh
```

The first build takes several hours. Subsequent builds use sstate-cache and are much faster.

## Flash

```bash
sudo dd if=build/tmp/deploy/images/raspberrypi3-64/radiotrope-image-raspberrypi3-64.rootfs.rpi-sdimg \
    of=/dev/sdX bs=4M status=progress
sync
```

Replace `/dev/sdX` with your SD card device.

## What's in the Image

- Minimal Linux (Yocto Scarthgap / Poky)
- Systemd init, no desktop environment
- Radiotrope binary with `embedded` feature (Slint linuxkms backend)
- Auto-starts on boot via systemd service
- ALSA + MA12070P driver for I2S audio
- Goodix touch driver for DSI display; the touchscreen is the only input (no buttons or knobs)
- Dropbear SSH server only with a key (release) or passwordless root (dev image)

## Where the player keeps its files

The player runs as the `radiotrope` system user in a systemd sandbox
(`radiotrope.service`): the system is read-only to it, and a bug in the
player reaches only its own files. Everything it keeps is under
`/var/lib/radiotrope`:

- settings, favorites, paired phones: `/var/lib/radiotrope/.config/radiotrope/`
- recordings: `/var/lib/radiotrope/Music/Radiotrope/`

A recording folder on a USB stick must be under `/media`, `/run/media` or
`/mnt`; the sandbox lets the player write only there besides its own
folder. Wi-Fi works through iwd's D-Bus API, allowed for the `radiotrope`
user by `radiotrope-iwd.conf`.

## How It Works

The build uses [meta-rust-bin](https://github.com/rust-embedded/meta-rust-bin) for a modern Rust toolchain. Cargo downloads crate dependencies during compile (network access enabled per standard Yocto Rust pattern, as recommended by [meta-slint](https://github.com/slint-ui/meta-slint)). The `embedded` Cargo feature selects the linuxkms backend and software renderer.

## Layer Structure

```
meta-radiotrope/
├── conf/layer.conf                              # Layer config
├── recipes-radiotrope/radiotrope/
│   ├── radiotrope_0.1.0.bb                      # App recipe (cargo_bin)
│   └── files/
│       ├── radiotrope.service                   # Systemd unit (own user, sandbox)
│       ├── seatd.service                        # Seat daemon (screen and touch access)
│       ├── radiotrope-splash.service, .sh, splash.fb  # Boot splash until the player starts
│       └── radiotrope-iwd.conf                  # D-Bus: the player may use iwd
├── recipes-core/images/
│   └── radiotrope-image.bb                      # Image recipe
├── recipes-core/systemd-conf/
│   └── files/80-wireless.network                # DHCP on Wi-Fi (systemd-networkd)
├── recipes-connectivity/iwd/
│   └── files/main.conf                          # iwd: Wi-Fi only, auto-connect
├── recipes-bsp/rpi-config/
│   └── rpi-config_%.bbappend                    # config.txt: KMS, display, amp, memory
└── recipes-kernel/linux/
    ├── linux-raspberrypi_%.bbappend             # Kernel config fragments
    └── files/merus-amp.cfg, splash-boot.cfg     # MA12070P driver, deferred console
```

## First boot checklist

Things to look at the first time a new image runs on the Pi, over SSH
(dev image, or the release image with a key):

- Screen and touch: the player appears after the splash; a tap lands where
  the finger is (`libinput debug-events` shows touches if not).
- Sound: `aplay -l` lists only the amplifier (`merus-amp`), as card 0.
- Wi-Fi: Tools > Wi-Fi Settings finds networks and joins one; `ip addr
  show wlan0` has an address.
- Clock: the Pi has no clock of its own. `timedatectl` says "System clock
  synchronized: yes" within a minute of Wi-Fi coming up; the Scheduler,
  the Sleep Timer and HTTPS stations all depend on it.
- Memory: `free -m` shows most of the 1 GB free; `dmesg | grep -i cma`
  has no allocation failures (the image reserves 96 MB for the display).
- Modules: `lsmod` lists what is loaded; the image installs every kernel
  module (`kernel-modules`), which can be trimmed to that list later.

## Desktop vs Embedded Build

The same codebase supports both targets via Cargo features:

```bash
# Desktop (default)
cargo build

# Embedded (linuxkms + software renderer)
cargo build --no-default-features --features embedded
```
