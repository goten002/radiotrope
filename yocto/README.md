# Radiotrope Yocto Build

Builds a minimal embedded Linux image for running Radiotrope as a kiosk appliance on Raspberry Pi 3B+.

> [!NOTE]
> This is a build recipe, not a download: you build the image yourself,
> which takes several hours the first time. It is experimental, built and
> tested on the one set of parts below only, and things may change or break
> between versions. Another Pi model, screen or amplifier needs changes to
> the layer before it works.

## What to buy

- Raspberry Pi 3 Model B+ (not a 3B, 4 or 5)
- Waveshare 5inch DSI LCD (B), the 800x480 capacitive touchscreen
- InnoMaker HiFi AMP Pro (MA12070P amplifier HAT)
- A 9–24 V DC power supply for the amplifier, which also powers the Pi
  through the GPIO header (see [hardware.md](hardware.md))
- Two passive speakers, 4–8 Ω
- A microSD card, 8 GB or larger, and a card reader for the computer that
  builds the image

## Target Hardware

- **Raspberry Pi 3B+** — quad-core Cortex-A53, 1GB RAM
- **Waveshare 5" DSI LCD (B)** — 800x480 capacitive touchscreen
- **InnoMaker HiFi AMP Pro** — MA12070P I2S amplifier HAT (2x40W RMS)

See [hardware.md](hardware.md) for detailed specs, wiring, and device tree configuration.

## Prerequisites

- [kas](https://kas.readthedocs.io/) build tool: `pipx install kas` (plain
  `pip install` is refused by most current distributions)
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

The kas 5 container runs Debian 13, which Scarthgap has not tested as a
build host, so every build prints a "host distribution not validated"
warning. It is harmless. To build on a tested host instead, pick the
Debian 12 variant of the same image:

```bash
KAS_CONTAINER_IMAGE_DISTRO=debian-bookworm kas-container build kas-radiotrope-pi3.yml
```

A rebuild after `git pull` picks up changes to the Rust and Slint sources,
the layer and the kas file on its own: bitbake checksums the source tree
the recipe copies in. The upstream layers are pinned to commits in the kas
file; move a pin on purpose and rebuild.

### SSH with a key

Put your public key in `.config-seed/authorized_keys` (git ignores it) and
build the release image. The image then runs an SSH server where root logs
in with that key only, never with a password:

```bash
cp ~/.ssh/id_ed25519.pub .config-seed/authorized_keys
kas-container build kas-radiotrope-pi3.yml
ssh root@<pi address>   # the Pi shows up on the router as radiotrope-pi
```

Without that file the release image has no SSH server.

### Time zone

The image has no time zone setting on screen. It uses the zone in the kas
file, which the Scheduler and the clock follow; the default is Athens.
Change it to yours before building, with a name from the tz database:

```yaml
    DEFAULT_TIMEZONE ?= "Europe/London"
```

On a running Pi with SSH, `timedatectl set-timezone Europe/London` and
then `systemctl restart radiotrope` change it until the next flash.

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

## After flashing

No keyboard or SSH is needed for this part:

1. Put the card in the Pi and power the amplifier. The screen shows
   "Booting...", then the player.
2. Open **Tools > Wi-Fi Settings**, pick your network and type its
   password on the screen keyboard. The Pi joins it again on every start.
3. Wait a minute for the clock to set itself over the network, then search
   for stations and add favorites.
4. To control the player from a phone, open **Tools > Connections >
   Remote Control**, turn on "Allow phones on this network", press **Pair a
   Phone** and scan the code with the Radiotrope Remote app.

## Updating

A new image replaces the whole SD card, including `/var/lib/radiotrope`
where the player keeps favorites, settings, schedules and paired phones.
Copy them off first, over SSH (dev image, or the release image with a
key):

```bash
ssh root@radiotrope-pi tar -C /var/lib/radiotrope -czf - .config > radiotrope-backup.tar.gz
```

Build and flash the new image, then put the files back and restart the
player:

```bash
ssh root@radiotrope-pi 'systemctl stop radiotrope &&
    tar -C /var/lib/radiotrope -xzf - &&
    chown -R radiotrope:radiotrope /var/lib/radiotrope/.config &&
    systemctl start radiotrope' < radiotrope-backup.tar.gz
```

Without SSH the new image starts with no favorites, and phones need to
pair again. Recordings in `/var/lib/radiotrope/Music` are lost too; copy
them off the same way, or record to a USB stick (see below).

## What's in the Image

- Minimal Linux (Yocto Scarthgap / Poky)
- Systemd init, no desktop environment
- Radiotrope binary with `embedded` feature (Slint linuxkms backend)
- Auto-starts on boot via systemd service
- ALSA + MA12070P driver for I2S audio
- Goodix touch driver for DSI display; the touchscreen is the only input (no buttons or knobs)
- Power Off and Restart in the player's Open menu (each asks twice); the sandboxed player creates a file in /run/radiotrope and a root path unit runs `systemctl poweroff` or `reboot`
- Splash screens: "Booting..." until the player starts, "Restarting..." or "Shutting down..." once it has stopped (the three PNGs and their framebuffer forms come from `yocto/scripts/make-splash.py`, which needs Pillow and numpy)
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
│       ├── radiotrope-splash.service, .sh         # Boot splash (loops until the player starts)
│       ├── radiotrope-shutdown-splash.service     # Restart / power-off splash (after the player stops)
│       ├── splash-{boot,reboot,poweroff}.png, -16/32.fb.gz  # The three screens (scripts/make-splash.py)
│       └── radiotrope-iwd.conf                  # D-Bus: the player may use iwd
├── recipes-core/images/
│   └── radiotrope-image.bb                      # Image recipe
├── recipes-core/systemd-conf/
│   └── files/80-wireless.network                # DHCP on Wi-Fi (systemd-networkd)
├── recipes-connectivity/iwd/
│   └── files/main.conf                          # iwd: Wi-Fi only, auto-connect
├── recipes-connectivity/wifi-powersave/
│   └── files/80-wifi-powersave.rules            # udev: Wi-Fi power save off (stable link)
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
  show wlan0` has an address, and `iw dev wlan0 get power_save` says
  "off" (the driver's default, on, makes the Pi 3 link drop and stall).
- Clock: the Pi has no clock of its own. `timedatectl` says "System clock
  synchronized: yes" within a minute of Wi-Fi coming up; the Scheduler,
  the Sleep Timer and HTTPS stations all depend on it.
- Time zone: `timedatectl` shows the zone set in the kas file, and the
  Scheduler dialog shows your local time.
- Memory: `free -m` shows most of the 1 GB free; `dmesg | grep -i cma`
  has no allocation failures (the image reserves 96 MB for the display).
- Power Off and Restart: Open > Restart (pressed twice) shows
  "Restarting..." and reboots within a few seconds, Open > Power Off shows
  "Shutting down..." and halts; `systemctl status radiotrope-poweroff.path
  radiotrope-reboot.path` shows both active.
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
