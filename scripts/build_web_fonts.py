#!/usr/bin/env python3
"""Build the web terminal's fonts from Kaku's bundled ones.

    python3 scripts/build_web_fonts.py

Writes WOFF2 copies of JetBrains Mono (Regular and Medium) and the private
use area of Symbols Nerd Font Mono into crates/kaku-remote/assets/web, and
prints the symbol font's unicode-range for index.html. Needs fontTools with
brotli (`pip install fonttools brotli`).
"""

import os
import sys

from fontTools import subset
from fontTools.ttLib import TTFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "assets", "fonts")
OUT = os.path.join(ROOT, "crates", "kaku-remote", "assets", "web")


def private_use(cp):
    return 0xE000 <= cp <= 0xF8FF or 0xF0000 <= cp <= 0xFFFFD


def ranges(cps):
    out = []
    for cp in sorted(cps):
        if out and cp == out[-1][1] + 1:
            out[-1][1] = cp
        else:
            out.append([cp, cp])
    return out


def woff2(source, target, unicodes=None):
    options = subset.Options()
    options.flavor = "woff2"
    options.layout_features = ["*"]
    options.name_IDs = ["*"]
    options.notdef_outline = True
    options.glyph_names = False
    font = TTFont(source)
    subsetter = subset.Subsetter(options)
    if unicodes is None:
        unicodes = list(font.getBestCmap())
    subsetter.populate(unicodes=unicodes)
    subsetter.subset(font)
    font.flavor = "woff2"
    font.save(target)
    print("%s %d bytes" % (os.path.relpath(target, ROOT), os.path.getsize(target)))


def main():
    woff2(os.path.join(SRC, "JetBrainsMono-Regular.ttf"), os.path.join(OUT, "font-regular.woff2"))
    woff2(os.path.join(SRC, "JetBrainsMono-Medium.ttf"), os.path.join(OUT, "font-medium.woff2"))
    symbols = os.path.join(SRC, "SymbolsNerdFontMono-Regular.ttf")
    icons = [cp for cp in TTFont(symbols).getBestCmap() if private_use(cp)]
    woff2(symbols, os.path.join(OUT, "font-symbols.woff2"), icons)
    # Two spans cover every icon; gaps inside them hold no glyph in this
    # font, so the browser still falls back past it there.
    spans = ranges(icons)
    bmp = [s for s in spans if s[1] <= 0xFFFF]
    astral = [s for s in spans if s[0] > 0xFFFF]
    print("unicode-range: U+%04X-%04X, U+%X-%X" % (bmp[0][0], bmp[-1][1], astral[0][0], astral[-1][1]))


if __name__ == "__main__":
    sys.exit(main())
