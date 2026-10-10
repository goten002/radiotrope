#!/usr/bin/env python3
"""Turn the boot splash PNG into raw framebuffer images.

The Pi's /dev/fb0 is 16 bits per pixel (RGB565) under vc4's KMS driver
and 32 bits (XRGB8888) under the firmware framebuffer, so both are
shipped and radiotrope-splash.sh picks the one that matches. Run this
after changing splash.png:

    python3 yocto/scripts/make-splash.py
"""
from pathlib import Path

import numpy as np
from PIL import Image

FILES = Path(__file__).resolve().parents[1] / "meta-radiotrope/recipes-radiotrope/radiotrope/files"

rgb = np.array(Image.open(FILES / "splash.png").convert("RGB"))
h, w, _ = rgb.shape
assert (w, h) == (800, 480), f"the splash is {w}x{h}, the screen is 800x480"

# 32 bpp: B, G, R, X in memory
bgrx = rgb[:, :, [2, 1, 0, 0]].copy()
bgrx[:, :, 3] = 255
bgrx.tofile(FILES / "splash-32.fb")

# 16 bpp: RGB565, little endian
r = rgb[:, :, 0].astype(np.uint16) >> 3
g = rgb[:, :, 1].astype(np.uint16) >> 2
b = rgb[:, :, 2].astype(np.uint16) >> 3
((r << 11) | (g << 5) | b).astype("<u2").tofile(FILES / "splash-16.fb")

print("wrote splash-32.fb and splash-16.fb")
