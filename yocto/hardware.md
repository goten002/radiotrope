# Radiotrope Hardware Reference

## Target Platform

Raspberry Pi 3 Model B+ with a DSI touchscreen and I2S amplifier HAT, running a minimal Yocto Linux image in kiosk mode.

---

## 1. Raspberry Pi 3 Model B+

- **SoC:** Broadcom BCM2837B0, quad-core Cortex-A53 @ 1.4 GHz (64-bit)
- **RAM:** 1 GB LPDDR2
- **GPU:** VideoCore IV (supports DRM/KMS via `vc4`)
- **Connectivity:** Gigabit Ethernet (over USB 2.0, ~300 Mbps), 802.11ac Wi-Fi, Bluetooth 4.2
- **Interfaces:** 40-pin GPIO, DSI display, CSI camera, 4× USB 2.0, HDMI, 3.5mm audio jack
- **Power:** 5V/2.5A via micro-USB

### Kernel / Device Tree

- Machine: `raspberrypi3-64` (Yocto meta-raspberrypi)
- Base DT: `bcm2710-rpi-3-b-plus.dtb`
- GPU driver: `vc4-kms-v3d` (loaded by default in meta-raspberrypi)

---

## 2. Waveshare 5" DSI LCD (B)

- **Model:** Waveshare 5inch DSI LCD (B)
- **Resolution:** 800 × 480 (IPS)
- **Interface:** DSI (MIPI, 2-lane), directly connected to Pi DSI ribbon connector
- **Touch:** Capacitive, Goodix GT911 controller over I2C
- **Backlight:** Controlled via DSI commands (automatic)
- **Power:** Powered from DSI connector (no separate supply needed)
- **Dimensions:** 120.7 × 75.8 mm active area

### Device Tree Overlay

```
dtoverlay=vc4-kms-dsi-7inch
```

> The `vc4-kms-dsi-7inch` overlay works for both the official 7" and Waveshare 5" DSI displays.
> The Goodix touch controller is auto-detected on the I2C bus.

### Kernel Requirements

- `CONFIG_DRM_VC4=m` (default in meta-raspberrypi)
- `CONFIG_TOUCHSCREEN_GOODIX=m` (default in meta-raspberrypi)
- `CONFIG_INPUT_EVDEV=y`

### Slint Configuration

- Backend: `linuxkms` (renders directly to DRM/KMS framebuffer)
- Renderer: `renderer-software` (no GPU acceleration needed for this resolution)
- Touch input via `libinput` → evdev

---

## 3. InnoMaker HiFi AMP Pro (MA12070P)

- **Model:** InnoMaker Raspberry Pi HiFi AMP HAT Pro
- **Amplifier IC:** Infineon (Merus Audio) MA12070P
- **Output:** 2 × 40W RMS (4Ω), 2 × 80W peak
- **Interface:** I2S (BCM GPIO 18/19/20/21) + I2C control
- **Power Input:** 9–24V DC barrel jack (separate from Pi power)
- **ALSA Card:** `merus-amp` (card name used by the kernel driver)

### Device Tree Overlay

```
dtoverlay=merus-amp
```

### Kernel Requirements

- `CONFIG_SND_SOC_MA120X0P=m` — Merus Audio MA120x0P codec driver
- `CONFIG_SND_BCM2835_SOC_I2S=m` — BCM2835 I2S (default in meta-raspberrypi)
- `CONFIG_I2C_BCM2835=m` — BCM2835 I2C (default)

The `merus-amp` overlay configures GPIO 18/19/20/21 for I2S and registers the MA12070P codec on the I2C bus.

### Power Wiring

```
DC barrel jack (9-24V) ──► AMP Pro board
                           ├── Amplifier (MA12070P)
                           └── 5V buck regulator ──► Pi 5V GPIO pins (pin 2,4)
```

> The AMP Pro can back-power the Pi through the GPIO header. No separate micro-USB power needed
> when using ≥12V input to the AMP Pro barrel jack.

### ALSA Quick Test

```bash
# List cards
aplay -l
# Should show: card N: merusamp [merus-amp], device 0: ...

# Play test tone
speaker-test -D plughw:merusamp -c 2 -t sine -f 440
```

---

## Wiring Summary

```
┌─────────────────────┐
│   Raspberry Pi 3B+  │
│                     │
│  DSI ◄──ribbon──►  Waveshare 5" LCD    (display + touch)
│                     │
│  GPIO 40-pin ◄──►  AMP Pro HAT         (I2S audio + I2C control)
│                     │
│  Ethernet/Wi-Fi     │                   (internet radio streams)
└─────────────────────┘
         ▲
         │ 5V from AMP Pro buck regulator
         │
    AMP Pro DC input (12-24V)
         │
    ┌────┴────┐
    │ Speaker │ × 2  (4-8Ω, up to 40W each)
    └─────────┘
```

---

## References

- [Raspberry Pi 3B+ datasheet](https://datasheets.raspberrypi.com/rpi3/raspberry-pi-3-b-plus-product-brief.pdf)
- [Waveshare 5inch DSI LCD (B) wiki](https://www.waveshare.com/wiki/5inch_DSI_LCD_(B))
- [InnoMaker HiFi AMP Pro](http://www.intecs.com.hk/product/raspberry-pi-hifi-amp-hat-pro/)
- [MA12070P datasheet](https://www.infineon.com/dgdl/Infineon-MA12070P-DataSheet-v01_00-EN.pdf)
- [meta-raspberrypi layer](https://git.yoctoproject.org/meta-raspberrypi/)
