# Enable I2S for the InnoMaker AMP Pro HAT
ENABLE_I2S = "1"

# Use real KMS instead of Fake KMS (required for Slint linuxkms backend)
VC4DTBO = "vc4-kms-v3d"

# Device tree overlays for display and amplifier
do_deploy:append() {
    # Merus Audio MA12070P amplifier (I2S + I2C)
    echo "dtoverlay=merus-amp" >> ${DEPLOYDIR}/${BOOTFILES_DIR_NAME}/config.txt

    # Waveshare 5" DSI LCD (B) — uses the same overlay as the official 7" DSI
    echo "dtoverlay=vc4-kms-dsi-7inch" >> ${DEPLOYDIR}/${BOOTFILES_DIR_NAME}/config.txt

    # Allocate GPU memory for DRM/KMS rendering
    echo "gpu_mem=64" >> ${DEPLOYDIR}/${BOOTFILES_DIR_NAME}/config.txt

    # Disable Bluetooth permanently (saves ~75mW)
    echo "dtoverlay=disable-bt" >> ${DEPLOYDIR}/${BOOTFILES_DIR_NAME}/config.txt

    # Disable HDMI permanently — DSI display only (saves ~65mW)
    echo "hdmi_blanking=2" >> ${DEPLOYDIR}/${BOOTFILES_DIR_NAME}/config.txt

    # Keep firmware splash (rainbow) as early visual feedback
    # It gets replaced by our splash as soon as /dev/fb0 is available
    echo "boot_delay=0" >> ${DEPLOYDIR}/${BOOTFILES_DIR_NAME}/config.txt
}
