#!/usr/bin/env python3
"""Render the Pi's splash screens and turn them into framebuffer images.

Three screens share one layout (logo, name, a line of text): boot
("Booting..."), reboot ("Restarting...") and poweroff ("Shutting
down..."). Each is written as a PNG to look at and as gzipped raw
framebuffer images in both depths the Pi's /dev/fb0 takes: 16 bits per
pixel (RGB565) under vc4's KMS driver and 32 bits (XRGB8888) under the
firmware framebuffer; radiotrope-splash.sh picks the matching one and
pipes it through zcat. Run this after changing the logo, the text or the
layout:

    python3 yocto/scripts/make-splash.py
"""
import gzip
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[2]
FILES = ROOT / "yocto/meta-radiotrope/recipes-radiotrope/radiotrope/files"
LOGO = ROOT / "assets/logo/radiotrope-logo.png"

SIZE = (800, 480)
BACKGROUND = (26, 26, 46)
NAME_COLOUR = (236, 236, 240)
TEXT_COLOUR = (140, 140, 160)
LOGO_HEIGHT = 120

SCREENS = {
    "boot": "Booting...",
    "reboot": "Restarting...",
    "poweroff": "Shutting down...",
}

FONTS = [
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
]


def font(size):
    for path in FONTS:
        if Path(path).exists():
            return ImageFont.truetype(path, size)
    raise SystemExit("no Liberation Sans or DejaVu Sans found; install one")


def render(text):
    image = Image.new("RGB", SIZE, BACKGROUND)
    logo = Image.open(LOGO).convert("RGBA")
    logo = logo.resize(
        (round(logo.width * LOGO_HEIGHT / logo.height), LOGO_HEIGHT),
        Image.LANCZOS,
    )
    width, height = SIZE
    logo_top = 140
    image.paste(logo, ((width - logo.width) // 2, logo_top), logo)

    draw = ImageDraw.Draw(image)
    name_font, text_font = font(28), font(15)
    name_top = logo_top + LOGO_HEIGHT + 16
    draw.text((width / 2, name_top), "Radiotrope", NAME_COLOUR, name_font, anchor="mt")
    draw.text((width / 2, name_top + 44), text, TEXT_COLOUR, text_font, anchor="mt")
    return image


def write_framebuffers(image, stem):
    rgb = np.array(image.convert("RGB"))

    # 32 bpp: B, G, R, X in memory
    bgrx = rgb[:, :, [2, 1, 0, 0]].copy()
    bgrx[:, :, 3] = 255
    with gzip.GzipFile(FILES / f"{stem}-32.fb.gz", "wb", mtime=0) as out:
        out.write(bgrx.tobytes())

    # 16 bpp: RGB565, little endian
    r = rgb[:, :, 0].astype(np.uint16) >> 3
    g = rgb[:, :, 1].astype(np.uint16) >> 2
    b = rgb[:, :, 2].astype(np.uint16) >> 3
    with gzip.GzipFile(FILES / f"{stem}-16.fb.gz", "wb", mtime=0) as out:
        out.write(((r << 11) | (g << 5) | b).astype("<u2").tobytes())


for screen, text in SCREENS.items():
    image = render(text)
    stem = f"splash-{screen}"
    image.save(FILES / f"{stem}.png", optimize=True)
    write_framebuffers(image, stem)
    print(f"wrote {stem}.png, {stem}-16.fb.gz and {stem}-32.fb.gz")
