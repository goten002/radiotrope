FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI += "file://80-wireless.network"

do_install:append() {
    install -d ${D}${systemd_unitdir}/network
    install -m 0644 ${WORKDIR}/80-wireless.network ${D}${systemd_unitdir}/network/80-wireless.network
}

FILES:${PN} += "${systemd_unitdir}/network/80-wireless.network"
