SUMMARY = "Radiotrope embedded internet radio image"
DESCRIPTION = "Minimal Linux image for Raspberry Pi 3B+ running Radiotrope in kiosk mode"

inherit core-image

# SSH: the release image has none, unless a public key is given in
# RADIOTROPE_SSH_KEYS (an authorized_keys file); then root may log in with
# that key only. kas-radiotrope-pi3-dev.yml adds passwordless root login for
# development; never ship that image.
RADIOTROPE_SSH_KEYS ?= "/work/.config-seed/authorized_keys"
IMAGE_FEATURES += "${@'ssh-server-dropbear allow-root-login' if os.path.isfile(d.getVar('RADIOTROPE_SSH_KEYS')) else ''}"
# Rebuild the root file system when the key file changes
do_rootfs[file-checksums] += "${@'${RADIOTROPE_SSH_KEYS}:True' if os.path.isfile(d.getVar('RADIOTROPE_SSH_KEYS')) else ''}"

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

    # Pre-load favorites from desktop config, into the player's own home
    # (the radiotrope user, see the radiotrope recipe)
    install -d -m 0700 ${IMAGE_ROOTFS}/var/lib/radiotrope
    install -d -m 0700 ${IMAGE_ROOTFS}/var/lib/radiotrope/.config
    install -d -m 0700 ${IMAGE_ROOTFS}/var/lib/radiotrope/.config/radiotrope
    if [ -f /work/.config-seed/favorites.json ]; then
        install -m 0600 /work/.config-seed/favorites.json ${IMAGE_ROOTFS}/var/lib/radiotrope/.config/radiotrope/favorites.json
    fi
    chown -R radiotrope:radiotrope ${IMAGE_ROOTFS}/var/lib/radiotrope

    # SSH with a key only, when a key was given
    if [ -f "${RADIOTROPE_SSH_KEYS}" ]; then
        install -d -m 0700 ${IMAGE_ROOTFS}/root/.ssh
        install -m 0600 "${RADIOTROPE_SSH_KEYS}" ${IMAGE_ROOTFS}/root/.ssh/authorized_keys
    fi
    if [ "${@bb.utils.contains('IMAGE_FEATURES', 'allow-empty-password', 'dev', 'release', d)}" = "release" ] \
        && [ -e ${IMAGE_ROOTFS}${sysconfdir}/default/dropbear ]; then
        # -s: no password logins at all
        if grep -q '^DROPBEAR_EXTRA_ARGS=' ${IMAGE_ROOTFS}${sysconfdir}/default/dropbear; then
            sed -i 's/^DROPBEAR_EXTRA_ARGS="*\([^"]*\)"*/DROPBEAR_EXTRA_ARGS="\1 -s"/' ${IMAGE_ROOTFS}${sysconfdir}/default/dropbear
        else
            printf '\nDROPBEAR_EXTRA_ARGS="-s"\n' >> ${IMAGE_ROOTFS}${sysconfdir}/default/dropbear
        fi
    fi
}
ROOTFS_POSTPROCESS_COMMAND += "setup_kiosk;"
