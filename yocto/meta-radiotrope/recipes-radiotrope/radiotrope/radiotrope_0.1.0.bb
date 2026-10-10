SUMMARY = "Radiotrope Internet Radio"
DESCRIPTION = "Embedded internet radio player with Slint UI, running in kiosk mode on DRM/KMS"
HOMEPAGE = "https://github.com/goten002/radiotrope"
LICENSE = "GPL-3.0-or-later"
LIC_FILES_CHKSUM = "file://LICENSE;md5=1ebbd3e34237af26da5dc08a4e440464"

# Use cargo_bin from meta-rust-bin (modern Rust toolchain, standard Yocto Rust pattern)
inherit cargo_bin systemd useradd

# The player runs as its own user (see radiotrope.service), with its home,
# settings and recordings in /var/lib/radiotrope
USERADD_PACKAGES = "${PN}"
GROUPADD_PARAM:${PN} = "-r audio; -r video; -r input; -r render"
USERADD_PARAM:${PN} = "--system --home-dir /var/lib/radiotrope --no-create-home \
    --shell ${base_sbindir}/nologin --user-group --groups audio,video,input,render radiotrope"

# Systemd service file
SRC_URI = "file://radiotrope.service"

S = "${WORKDIR}/radiotrope"

# Copy project source into an isolated workdir to avoid Cargo workspace
# conflicts with other Rust recipes (rust-native bootstrap finds /work/Cargo.toml)
do_unpack() {
    mkdir -p ${S}
    cp -a /work/Cargo.toml /work/Cargo.lock /work/LICENSE ${S}/ 2>/dev/null || true
    cp -a /work/crates ${S}/
    # The UI and the station flags are compiled in from assets/
    cp -a /work/assets ${S}/
    # Copy service files and splash data to WORKDIR
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope.service ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/seatd.service ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope-splash.service ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope-splash.sh ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/splash-16.fb ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/splash-32.fb ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope-iwd.conf ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope-poweroff.path ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope-poweroff.service ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope-reboot.path ${WORKDIR}/
    cp -a /work/yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files/radiotrope-reboot.service ${WORKDIR}/
}

# Allow cargo to fetch crates from crates.io (standard Yocto Rust pattern per meta-slint)
do_compile[network] = "1"

# LAME (MP3 recording) builds with autoconf, which only cross-compiles when
# told the target; it reads the triple from the compiler name, and Yocto's
# compiler is a wrapper script
export MP3LAME_SYS_OVERRIDE_HOST = "${HOST_SYS}"

# Build with embedded feature flags (linuxkms backend, software renderer)
CARGO_FEATURES = "embedded"
EXTRA_CARGO_FLAGS = "--no-default-features"

# Build the radiotrope-app binary (workspace member)
CARGO_MANIFEST_PATH = "${S}/crates/radiotrope-app/Cargo.toml"

DEPENDS = " \
    alsa-lib \
    cmake-native \
    fontconfig \
    libinput \
    seatd \
    libxkbcommon \
    libdrm \
    openssl \
"

RDEPENDS:${PN} = " \
    alsa-lib \
    alsa-utils \
    fontconfig \
    libinput \
    seatd \
    libxkbcommon \
    libdrm \
    ca-certificates \
"

SYSTEMD_SERVICE:${PN} = "radiotrope.service seatd.service radiotrope-splash.service radiotrope-poweroff.path radiotrope-reboot.path"
SYSTEMD_AUTO_ENABLE = "enable"

do_install() {
    install -d ${D}${bindir}
    install -m 0755 ${B}/${RUST_TARGET}/release/radiotrope ${D}${bindir}/radiotrope
    install -m 0755 ${WORKDIR}/radiotrope-splash.sh ${D}${bindir}/radiotrope-splash

    install -d ${D}${datadir}/radiotrope
    install -m 0644 ${WORKDIR}/splash-16.fb ${D}${datadir}/radiotrope/splash-16.fb
    install -m 0644 ${WORKDIR}/splash-32.fb ${D}${datadir}/radiotrope/splash-32.fb

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${WORKDIR}/radiotrope.service ${D}${systemd_system_unitdir}/radiotrope.service
    install -m 0644 ${WORKDIR}/seatd.service ${D}${systemd_system_unitdir}/seatd.service
    install -m 0644 ${WORKDIR}/radiotrope-splash.service ${D}${systemd_system_unitdir}/radiotrope-splash.service
    install -m 0644 ${WORKDIR}/radiotrope-poweroff.path ${D}${systemd_system_unitdir}/radiotrope-poweroff.path
    install -m 0644 ${WORKDIR}/radiotrope-poweroff.service ${D}${systemd_system_unitdir}/radiotrope-poweroff.service
    install -m 0644 ${WORKDIR}/radiotrope-reboot.path ${D}${systemd_system_unitdir}/radiotrope-reboot.path
    install -m 0644 ${WORKDIR}/radiotrope-reboot.service ${D}${systemd_system_unitdir}/radiotrope-reboot.service

    install -d ${D}${datadir}/dbus-1/system.d
    install -m 0644 ${WORKDIR}/radiotrope-iwd.conf ${D}${datadir}/dbus-1/system.d/radiotrope-iwd.conf
}

# Cargo release profile strips the binary; skip Yocto's already-stripped QA check
INSANE_SKIP:${PN} += "already-stripped"

FILES:${PN} += " \
    ${systemd_system_unitdir}/radiotrope.service \
    ${systemd_system_unitdir}/seatd.service \
    ${systemd_system_unitdir}/radiotrope-splash.service \
    ${systemd_system_unitdir}/radiotrope-poweroff.path \
    ${systemd_system_unitdir}/radiotrope-poweroff.service \
    ${systemd_system_unitdir}/radiotrope-reboot.path \
    ${systemd_system_unitdir}/radiotrope-reboot.service \
    ${datadir}/radiotrope/splash-16.fb \
    ${datadir}/radiotrope/splash-32.fb \
    ${datadir}/dbus-1/system.d/radiotrope-iwd.conf \
"
