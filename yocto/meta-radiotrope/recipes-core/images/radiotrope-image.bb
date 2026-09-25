SUMMARY = "Radiotrope embedded internet radio image"
DESCRIPTION = "Minimal Linux image for Raspberry Pi 3B+ running Radiotrope in kiosk mode"

inherit core-image

IMAGE_FEATURES += " \
    ssh-server-dropbear \
    allow-empty-password \
    allow-root-login \
    debug-tweaks \
"

IMAGE_INSTALL += " \
    radiotrope \
    alsa-utils \
    ca-certificates \
    kernel-modules \
    libinput \
    seatd \
    fontconfig \
    liberation-fonts \
    libgpiod \
    libgpiod-tools \
    iwd \
    linux-firmware-bcm43430 \
    nano \
"

# Image size — keep minimal
IMAGE_ROOTFS_EXTRA_SPACE = "131072"

# Kiosk mode setup
setup_kiosk() {
    # Mask getty on tty1 — Radiotrope owns the display
    ln -sf /dev/null ${IMAGE_ROOTFS}${sysconfdir}/systemd/system/getty@tty1.service
    # Mask serial getty
    ln -sf /dev/null ${IMAGE_ROOTFS}${sysconfdir}/systemd/system/serial-getty@ttyS0.service
    ln -sf /dev/null ${IMAGE_ROOTFS}${sysconfdir}/systemd/system/serial-getty@ttyAMA0.service
    # Disable USB/Ethernet at boot (saves ~300mW, not needed — WiFi only)
    install -d ${IMAGE_ROOTFS}${systemd_system_unitdir}
    cat > ${IMAGE_ROOTFS}${systemd_system_unitdir}/disable-usb-eth.service << 'SVCEOF'
[Unit]
Description=Disable USB/Ethernet controller
After=multi-user.target

[Service]
Type=oneshot
ExecStart=/bin/sh -c "echo '1-1' > /sys/bus/usb/drivers/usb/unbind"
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
SVCEOF
    install -d ${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants
    ln -sf ${systemd_system_unitdir}/disable-usb-eth.service ${IMAGE_ROOTFS}${sysconfdir}/systemd/system/multi-user.target.wants/disable-usb-eth.service

    # Pre-load favorites from desktop config
    mkdir -p ${IMAGE_ROOTFS}/root/.config/radiotrope
    if [ -f /work/.config-seed/favorites.json ]; then
        cp /work/.config-seed/favorites.json ${IMAGE_ROOTFS}/root/.config/radiotrope/favorites.json
    fi
}
ROOTFS_POSTPROCESS_COMMAND += "setup_kiosk;"
