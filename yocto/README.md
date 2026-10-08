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
- Goodix touch driver for DSI display
- Dropbear SSH server for remote access

## How It Works

The build uses [meta-rust-bin](https://github.com/rust-embedded/meta-rust-bin) for a modern Rust toolchain. Cargo downloads crate dependencies during compile (network access enabled per standard Yocto Rust pattern, as recommended by [meta-slint](https://github.com/slint-ui/meta-slint)). The `embedded` Cargo feature selects the linuxkms backend and software renderer.

## Layer Structure

```
meta-radiotrope/
├── conf/layer.conf                              # Layer config
├── recipes-radiotrope/radiotrope/
│   ├── radiotrope_0.1.0.bb                      # App recipe (cargo_bin)
│   └── files/radiotrope.service                 # Systemd unit
├── recipes-core/images/
│   └── radiotrope-image.bb                      # Image recipe
├── recipes-bsp/rpi-config/
│   └── rpi-config_%.bbappend                    # DT overlays (display + amp)
└── recipes-kernel/linux/
    ├── linux-raspberrypi_%.bbappend             # Kernel config fragment
    └── files/merus-amp.cfg                      # Enable MA12070P driver
```

## Desktop vs Embedded Build

The same codebase supports both targets via Cargo features:

```bash
# Desktop (default)
cargo build

# Embedded (linuxkms + software renderer)
cargo build --no-default-features --features embedded
```
