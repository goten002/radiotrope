FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI += "file://merus-amp.cfg"
SRC_URI += "file://splash-boot.cfg"

# Add the merus-amp overlay to the kernel build
RPI_KERNEL_DEVICETREE_OVERLAYS:append = " overlays/merus-amp.dtbo"
