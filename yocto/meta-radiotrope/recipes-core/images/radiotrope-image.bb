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

# kernel-modules is every module the kernel builds (the machine recommends
# it too). Trim it to the modules in use once the image has run on the Pi:
# `lsmod` there gives the list. Wi-Fi firmware for the 3B+ (BCM43455) comes
# with the machine's recommendations.
IMAGE_INSTALL += " \
    radiotrope \
    alsa-utils \
    ca-certificates \
    kernel-modules \
    libinput \
    seatd \
    fontconfig \
    liberation-fonts \
    iwd \
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
