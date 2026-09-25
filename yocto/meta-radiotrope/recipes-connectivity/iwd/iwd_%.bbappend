FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI += "file://main.conf"

do_install:append() {
    install -d ${D}${sysconfdir}/iwd
    install -m 0644 ${WORKDIR}/main.conf ${D}${sysconfdir}/iwd/main.conf
}

FILES:${PN} += "${sysconfdir}/iwd/main.conf"
