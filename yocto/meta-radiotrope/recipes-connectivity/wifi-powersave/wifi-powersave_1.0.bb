SUMMARY = "Keep Wi-Fi power save off"
DESCRIPTION = "udev rule that turns 802.11 power save off on every wireless interface, for a stable streaming link on the Pi"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://80-wifi-powersave.rules"

S = "${WORKDIR}"

RDEPENDS:${PN} = "iw"

do_install() {
    install -d ${D}${nonarch_base_libdir}/udev/rules.d
    install -m 0644 ${WORKDIR}/80-wifi-powersave.rules ${D}${nonarch_base_libdir}/udev/rules.d/80-wifi-powersave.rules
}

FILES:${PN} = "${nonarch_base_libdir}/udev/rules.d/80-wifi-powersave.rules"
