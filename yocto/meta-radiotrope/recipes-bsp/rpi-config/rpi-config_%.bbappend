# config.txt for the Pi 3B+ with the Waveshare 5" DSI screen and the
# InnoMaker AMP Pro HAT. meta-raspberrypi's own variables where it has
# them, RPI_EXTRA_CONFIG for the rest.

# I2S for the amplifier
ENABLE_I2S = "1"

# Real KMS (the Slint linuxkms backend needs it), with HDMI and its audio
# off: the DSI screen is the only display, and HDMI audio would otherwise
# be one more ALSA card next to the amplifier. 96 MB of CMA is plenty for
# the software renderer at 800x480 (the default reserves 256 MB)
VC4DTBO = "vc4-kms-v3d,nohdmi,noaudio,cma-96"

# The firmware's own memory: KMS doesn't use it, so the minimum
GPU_MEM = "16"

# No wait before the kernel; the rainbow screen goes as soon as possible
BOOT_DELAY = "0"

# The rest of config.txt: the Merus Audio MA12070P amplifier (I2S + I2C),
# the Waveshare 5 inch DSI LCD (B), which uses the official 7 inch panel's
# driver, and Bluetooth off (unused, saves ~75 mW)
RPI_EXTRA_CONFIG = "\n\
dtoverlay=merus-amp\n\
dtoverlay=vc4-kms-dsi-7inch\n\
dtoverlay=disable-bt\n\
"

# The machine config turns the headphone jack's sound card on; off, so the
# amplifier is ALSA card 0 and the player's default device
do_deploy:append() {
    sed -i 's/^dtparam=audio=on$/dtparam=audio=off/' ${DEPLOYDIR}/${BOOTFILES_DIR_NAME}/config.txt
}
