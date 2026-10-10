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

# The unit files, splash and D-Bus policy next to this recipe
SRC_URI = " \
    file://radiotrope.service \
    file://seatd.service \
    file://radiotrope-splash.service \
    file://radiotrope-splash.sh \
    file://splash-16.fb \
    file://splash-32.fb \
    file://radiotrope-iwd.conf \
    file://radiotrope-poweroff.path \
    file://radiotrope-poweroff.service \
    file://radiotrope-reboot.path \
    file://radiotrope-reboot.service \
"

S = "${WORKDIR}/radiotrope"

# The player's source is the checkout kas-container mounts at /work. It is
# copied into its own workdir so cargo doesn't see the workspace at /work
# (rust-native's bootstrap finds /work/Cargo.toml otherwise). bitbake
# checksums what is copied, so a change to the Rust or Slint sources
# rebuilds the player; a plain pull-and-build is enough
RADIOTROPE_SRC ?= "/work"
RADIOTROPE_SRC_PARTS = "Cargo.toml Cargo.lock LICENSE crates assets"
do_unpack[file-checksums] += "${@' '.join('%s/%s:True' % (d.getVar('RADIOTROPE_SRC'), p) for p in d.getVar('RADIOTROPE_SRC_PARTS').split())}"

python do_unpack:append() {
    bb.build.exec_func("radiotrope_copy_source", d)
}

radiotrope_copy_source() {
    rm -rf ${S}
    mkdir -p ${S}
    for part in ${RADIOTROPE_SRC_PARTS}; do
        cp -a ${RADIOTROPE_SRC}/$part ${S}/
    done
}

# Allow cargo to fetch crates from crates.io (standard Yocto Rust pattern per meta-slint)
do_compile[network] = "1"

# LAME (MP3 recording) builds with autoconf, which only cross-compiles when
# told the target; it reads the triple from the compiler name, and Yocto's
# compiler is a wrapper script
export MP3LAME_SYS_OVERRIDE_HOST = "${HOST_SYS}"

# Build with the embedded feature only (linuxkms backend, software
# renderer). The class adds --features but not --no-default-features, and
# the crate's default is the desktop feature (winit, tray icon, file
# dialogs), which has no business on the Pi
CARGO_FEATURES = "embedded"
EXTRA_CARGO_FLAGS = "--no-default-features"

# Rust writes source paths into the binary (panic messages, debug info),
# and Yocto flags anything under TMPDIR as a leaked build path
# [buildpaths]. Map the build tree to a fixed name, as poky's own rust
# class does
EXTRA_RUSTFLAGS = "--remap-path-prefix=${TMPDIR}=/usr/src/debug"

# The C parts of the crates (aws-lc, opus, lame) compile out of the cargo
# registry under cargo_home and keep __FILE__ in their error strings. The
# default DEBUG_PREFIX_MAP covers S, B and the sysroots only, so map
# cargo_home too, or the binary still refers to TMPDIR
DEBUG_PREFIX_MAP += " \
    -fmacro-prefix-map=${CARGO_HOME}=${TARGET_DBGSRC_DIR}/cargo_home \
    -fdebug-prefix-map=${CARGO_HOME}=${TARGET_DBGSRC_DIR}/cargo_home \
"

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
