# Usage: python3 -m venv .venv && .venv/bin/pip install pillow==12.* fonttools
#        cargo test -- --ignored export_hero_screen
#        .venv/bin/python -I tools/hero.py target/hero docs/hero.png FONTS_DIR
# FONTS_DIR holds JetBrainsMono-Regular.ttf, JetBrainsMono-Bold.ttf (Ghostty's
# default font) and SymbolsNerdFontMono-Regular.ttf (its built-in icons).
"""Draws the frame of made-up news the `export_hero_screen` test exported,
the way Ghostty shows it: every cell's character in JetBrains Mono, box
lines and blocks drawn as Ghostty draws them, and a picture painted here for
each card, as a news site's photo of its story would be. As tuitube's."""

import json
import math
import os
import random
import sys

from fontTools.ttLib import TTFont
from PIL import Image, ImageDraw, ImageFilter, ImageFont

SCREEN, OUT, FONTS = sys.argv[1], sys.argv[2], sys.argv[3]
PAD = 28
RADIUS = 26

screen = json.load(open(os.path.join(SCREEN, "screen.json")))
COLS, ROWS = screen["cols"], screen["rows"]
CW, CH = screen["cell"]
BG, FG = screen["bg"], screen["fg"]

regular = ImageFont.truetype(os.path.join(FONTS, "JetBrainsMono-Regular.ttf"), 26.6)
bold = ImageFont.truetype(os.path.join(FONTS, "JetBrainsMono-Bold.ttf"), 26.6)
icons = ImageFont.truetype(os.path.join(FONTS, "SymbolsNerdFontMono-Regular.ttf"), 26.6)
covered = set(TTFont(os.path.join(FONTS, "JetBrainsMono-Regular.ttf")).getBestCmap())
icon_cmap = set(TTFont(os.path.join(FONTS, "SymbolsNerdFontMono-Regular.ttf")).getBestCmap())
# What neither has comes from a system font, as Ghostty falls back.
FALLBACK = "/System/Library/Fonts/Apple Symbols.ttf"
fallback = ImageFont.truetype(FALLBACK, 30)
ascent, descent = regular.getmetrics()
BASELINE = (CH - (ascent + descent)) // 2 + ascent

width, height = COLS * CW + 2 * PAD, ROWS * CH + 2 * PAD
canvas = Image.new("RGB", (width, height), BG)
draw = ImageDraw.Draw(canvas)


def cell_box(x, y):
    left, top = PAD + x * CW, PAD + y * CH
    return left, top, left + CW, top + CH


def line_shape(ch, x, y, color):
    """Box lines and blocks, drawn to the cell's edges as Ghostty does, so
    they join up; True if `ch` is one."""
    left, top, right, bottom = cell_box(x, y)
    mx, my = left + CW // 2, top + CH // 2
    thin = 2

    def rectangle(box):
        if box[2] >= box[0] and box[3] >= box[1]:
            draw.rectangle(box, fill=color)

    if ch == "─":
        rectangle([left, my - 1, right - 1, my])
    elif ch == "│":
        rectangle([mx - 1, top, mx, bottom - 1])
    elif ch in "├┤":
        # A border going down, with a line into the box from it.
        rectangle([mx - 1, top, mx, bottom - 1])
        if ch == "├":
            rectangle([mx, my - 1, right - 1, my])
        else:
            rectangle([left, my - 1, mx, my])
    elif ch in "╭╮╰╯":
        r = CW // 2
        # The arc's center, and the straight bits from its ends to the
        # cell's edges.
        cx = mx + r if ch in "╭╰" else mx - r
        cy = my + r if ch in "╭╮" else my - r
        box = [cx - r, cy - r, cx + r, cy + r]
        start = {"╭": 180, "╮": 270, "╰": 90, "╯": 0}[ch]
        draw.arc(box, start, start + 90, fill=color, width=thin)
        if ch in "╭╮":
            rectangle([mx - 1, cy, mx, bottom - 1])
        else:
            rectangle([mx - 1, top, mx, cy])
        if ch in "╭╰":
            rectangle([cx, my - 1, right - 1, my])
        else:
            rectangle([left, my - 1, cx, my])
    elif ch == "▀":
        rectangle([left, top, right - 1, top + CH // 2 - 1])
    elif ch == "▄":
        rectangle([left, top + CH // 2, right - 1, bottom - 1])
    elif ch == "▌":
        rectangle([left, top, left + CW // 2 - 1, bottom - 1])
    elif ch == "▐":
        rectangle([left + CW // 2, top, right - 1, bottom - 1])
    elif ch == "█":
        rectangle([left, top, right - 1, bottom - 1])
    else:
        return False
    return True


def draw_background(x, y, cell):
    bg = cell[2]
    left, top, right, bottom = cell_box(x, y)
    if bg:
        draw.rectangle([left, top, right - 1, bottom - 1], fill=bg)


def blank(x, y):
    return x >= COLS or cells[y * COLS + x][0] in ("", " ")


def draw_icon(x, y, ch, color):
    """A Nerd Font icon, as Ghostty draws one: as big as a cell's height
    allows, over two cells when the next one is empty, centered."""
    left, top, _, _ = cell_box(x, y)
    room = 2 * CW if blank(x + 1, y) else CW
    size = 26.6
    font = ImageFont.truetype(os.path.join(FONTS, "SymbolsNerdFontMono-Regular.ttf"), size)
    l, t, r, b = font.getbbox(ch)
    scale = min(room * 0.78 / max(r - l, 1), CH * 0.6 / max(b - t, 1))
    font = ImageFont.truetype(os.path.join(FONTS, "SymbolsNerdFontMono-Regular.ttf"), size * scale)
    l, t, r, b = font.getbbox(ch)
    gx = left + (room - (r - l)) / 2 - l
    gy = top + (CH - (b - t)) / 2 - t
    draw.text((gx, gy), ch, font=font, fill=color)


def draw_glyph(x, y, cell):
    ch, fg, _, heavy = cell
    if ch in ("", " "):
        return
    left, top, _, _ = cell_box(x, y)
    color = fg or FG
    if line_shape(ch, x, y, color):
        return
    point = ord(ch[0])
    if 0xE000 <= point <= 0xF8FF or point >= 0xF0000:
        draw_icon(x, y, ch, color)
        return
    font = bold if heavy else regular
    if point not in covered:
        font = icons if point in icon_cmap else fallback
    draw.text((left, top + BASELINE), ch, font=font, fill=color, anchor="ls")


def rect_px(r):
    x, y, w, h = r
    return PAD + x * CW, PAD + y * CH, w * CW, h * CH


# The pictures: a news photo's subject for each story, painted flat.
W, H = 1280, 720


def rgb(color):
    return tuple(int(color[i : i + 2], 16) for i in (1, 3, 5))


def gradient(top, bottom, size=(W, H)):
    """From `top` to `bottom`, down."""
    a, b = rgb(top), rgb(bottom)
    column = Image.new("RGB", (1, size[1]))
    for y in range(size[1]):
        t = y / max(size[1] - 1, 1)
        column.putpixel((0, y), tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3)))
    return column.resize(size)


def glow(art, center, radius, color, strength=200):
    """A soft light round `center`."""
    layer = Image.new("RGBA", art.size, (0, 0, 0, 0))
    x, y = center
    ImageDraw.Draw(layer).ellipse(
        [x - radius, y - radius, x + radius, y + radius], fill=rgb(color) + (strength,)
    )
    layer = layer.filter(ImageFilter.GaussianBlur(radius / 2))
    art.paste(layer, (0, 0), layer)


def overlay(art, paint):
    """Draws `paint(pen)` on a see-through layer over `art`."""
    layer = Image.new("RGBA", art.size, (0, 0, 0, 0))
    paint(ImageDraw.Draw(layer))
    art.paste(layer, (0, 0), layer)


def phone():
    art = gradient("#1c1f45", "#4b2f7f")
    for i in range(14):
        rnd = random.Random(i)
        glow(art, (rnd.randrange(W), rnd.randrange(H)), rnd.randrange(30, 90), "#8f7cff", 70)
    pen = ImageDraw.Draw(art)
    pen.rounded_rectangle([470, 70, 810, 690], 56, fill="#101326")
    pen.rounded_rectangle([490, 100, 790, 660], 40, fill="#1e2b4f")
    # A battery, nearly full.
    pen.rounded_rectangle([560, 300, 720, 420], 22, outline="#e8ecff", width=12)
    pen.rounded_rectangle([720, 335, 742, 385], 6, fill="#e8ecff")
    pen.rounded_rectangle([580, 320, 690, 400], 10, fill="#4ade80")
    pen.rounded_rectangle([560, 470, 720, 486], 8, fill="#3b4a7a")
    pen.rounded_rectangle([560, 470, 690, 486], 8, fill="#4ade80")
    return art


def lanterns():
    art = gradient("#0d1433", "#2c1d52")
    pen = ImageDraw.Draw(art)
    # The river, and the stalls along it.
    pen.rectangle([0, 470, W, H], fill="#101a3a")
    for i, x in enumerate(range(-40, W, 150)):
        top = 360 + (i % 3) * 18
        pen.polygon([(x, 470), (x + 20, top), (x + 130, top), (x + 150, 470)], fill="#1a1230")
        pen.rectangle([x + 30, top + 40, x + 120, 470], fill="#ffb347")
    # A string of lanterns across the top.
    pen.line([(0, 120), (W / 2, 200), (W, 120)], fill="#3a2b55", width=4, joint="curve")
    for i in range(11):
        t = i / 10
        x = t * W
        y = 120 + 80 * (1 - abs(2 * t - 1))
        color = ["#ff5a4e", "#ffb347", "#ff7a59"][i % 3]
        glow(art, (x, y + 40), 70, color, 150)
        pen.ellipse([x - 26, y + 10, x + 26, y + 72], fill=color)
        pen.rectangle([x - 12, y + 4, x + 12, y + 12], fill="#2a1d3a")
    # Their lights on the water.
    for x in range(40, W, 116):
        overlay(art, lambda p, x=x: p.rectangle([x, 500, x + 40, 506], fill=(255, 180, 90, 120)))
    return art


def car():
    art = gradient("#ff8a65", "#ffd59e")
    pen = ImageDraw.Draw(art)
    pen.ellipse([820, 120, 1040, 340], fill="#fff1c9")
    pen.polygon([(0, 420), (300, 330), (620, 400), (980, 320), (W, 380), (W, H), (0, H)], fill="#c46b6b")
    pen.polygon([(0, 520), (W, 480), (W, H), (0, H)], fill="#3b3f4a")
    for x in range(0, W, 160):
        pen.rectangle([x, 590, x + 80, 600], fill="#f5d76e")
    # A small hatchback.
    pen.rounded_rectangle([330, 400, 900, 540], 60, fill="#13a89e")
    pen.polygon([(430, 410), (520, 320), (740, 320), (830, 410)], fill="#13a89e")
    pen.polygon([(470, 405), (540, 340), (620, 340), (620, 405)], fill="#bde7ff")
    pen.polygon([(640, 405), (640, 340), (720, 340), (790, 405)], fill="#bde7ff")
    for cx in (460, 770):
        pen.ellipse([cx - 62, 480, cx + 62, 604], fill="#1d1f24")
        pen.ellipse([cx - 28, 514, cx + 28, 570], fill="#9aa3ad")
    pen.ellipse([860, 440, 892, 470], fill="#fff4b3")
    return art


def coffee():
    art = gradient("#7b5236", "#3a2418")
    pen = ImageDraw.Draw(art)
    rnd = random.Random(3)
    for _ in range(40):
        x, y = rnd.randrange(W), rnd.randrange(380, H)
        angle = rnd.randrange(180)
        bean = Image.new("RGBA", (90, 60), (0, 0, 0, 0))
        bp = ImageDraw.Draw(bean)
        bp.ellipse([0, 0, 89, 59], fill="#3b2314")
        bp.arc([20, -20, 70, 79], 100, 260, fill="#1f120a", width=6)
        bean = bean.rotate(angle, expand=True)
        art.paste(bean, (x, y), bean)
    # Prices going up.
    points = [(80, 420), (260, 390), (420, 400), (580, 320), (760, 300), (940, 200), (1180, 120)]
    overlay(art, lambda p: p.line(points, fill=(255, 236, 200, 90), width=40, joint="curve"))
    pen.line(points, fill="#ffe8c2", width=12, joint="curve")
    for x, y in points:
        pen.ellipse([x - 16, y - 16, x + 16, y + 16], fill="#ffe8c2")
    pen.polygon([(1180, 80), (1230, 150), (1140, 150)], fill="#ffe8c2")
    return art


def stadium():
    art = gradient("#06122a", "#16305a")
    pen = ImageDraw.Draw(art)
    for x in (120, 1160):
        glow(art, (x, 70), 140, "#ffffff", 210)
    # The stands, full.
    pen.polygon([(0, 180), (W, 180), (W, 360), (0, 360)], fill="#1b2442")
    rnd = random.Random(5)
    colors = ["#f87171", "#facc15", "#ffffff", "#60a5fa", "#f472b6"]
    for _ in range(2600):
        x, y = rnd.randrange(W), rnd.randrange(190, 355)
        pen.ellipse([x, y, x + 5, y + 5], fill=rnd.choice(colors))
    # The pitch, striped, going off into the distance.
    for i in range(10):
        y0 = 360 + i * 36
        y1 = y0 + 36
        shade = "#2f9e44" if i % 2 else "#37b24d"
        spread0, spread1 = 80 - i * 8, 80 - (i + 1) * 8
        pen.polygon([(spread0, y0), (W - spread0, y0), (W - spread1, y1), (spread1, y1)], fill=shade)
    pen.line([(80, 360), (W - 80, 360)], fill="#e9fbe9", width=5)
    pen.ellipse([520, 470, 760, 560], outline="#e9fbe9", width=5)
    pen.line([(W / 2, 360), (W / 2, H)], fill="#e9fbe9", width=5)
    pen.ellipse([700, 600, 740, 640], fill="#ffffff")
    return art


def island():
    art = gradient("#7cc8ff", "#e3f6ff")
    pen = ImageDraw.Draw(art)
    pen.ellipse([980, 60, 1130, 210], fill="#fff6c2")
    sea = gradient("#22b8cf", "#0b7285", (W, 320))
    art.paste(sea, (0, 400))
    pen.ellipse([300, 360, 980, 470], fill="#f8e3a3")
    pen.ellipse([380, 350, 900, 430], fill="#ffeebb")
    # A palm.
    pen.line([(640, 400), (660, 300), (700, 210)], fill="#8a5a2b", width=22, joint="curve")
    for angle in (200, 240, 290, 330, 20):
        leaf = Image.new("RGBA", (260, 70), (0, 0, 0, 0))
        ImageDraw.Draw(leaf).ellipse([0, 0, 259, 69], fill="#2f9e44")
        leaf = leaf.rotate(angle, expand=True)
        art.paste(leaf, (700 - leaf.width // 2 + int(110 * math.cos(math.radians(angle))),
                         210 - leaf.height // 2 - int(110 * math.sin(math.radians(angle)))), leaf)
    for y in range(500, H, 40):
        overlay(art, lambda p, y=y: p.line([(0, y), (W, y + 10)], fill=(255, 255, 255, 50), width=4))
    return art


def bus():
    art = gradient("#ffc89a", "#fff3dc")
    pen = ImageDraw.Draw(art)
    rnd = random.Random(8)
    x = -20
    while x < W:
        w, h = rnd.randrange(80, 160), rnd.randrange(160, 380)
        pen.rectangle([x, 470 - h, x + w, 470], fill=rnd.choice(["#8c9bb5", "#a3aec4", "#77869f"]))
        for wy in range(470 - h + 20, 450, 34):
            for wx in range(x + 14, x + w - 20, 28):
                pen.rectangle([wx, wy, wx + 14, wy + 18], fill="#dfe6f2")
        x += w + rnd.randrange(4, 20)
    pen.rectangle([0, 470, W, H], fill="#596273")
    pen.rectangle([0, 470, W, 490], fill="#c9ced8")
    # An electric bus, green.
    pen.rounded_rectangle([180, 360, 1100, 600], 40, fill="#2f9e44")
    for wx in range(240, 1000, 130):
        pen.rounded_rectangle([wx, 390, wx + 110, 470], 14, fill="#d3f9d8")
    pen.rounded_rectangle([1010, 390, 1080, 560], 14, fill="#d3f9d8")
    pen.rectangle([180, 520, 1100, 540], fill="#e6fcf5")
    pen.polygon([(560, 330), (600, 330), (580, 358)], fill="#2f9e44")
    pen.line([(520, 330), (640, 330)], fill="#495057", width=6)
    for cx in (330, 900):
        pen.ellipse([cx - 56, 550, cx + 56, 662], fill="#1d1f24")
        pen.ellipse([cx - 24, 582, cx + 24, 630], fill="#adb5bd")
    return art


def delta():
    art = Image.new("RGB", (W, H), "#4f9d69")
    pen = ImageDraw.Draw(art)
    rnd = random.Random(11)
    for _ in range(500):
        x, y, r = rnd.randrange(W), rnd.randrange(H), rnd.randrange(10, 34)
        pen.ellipse([x - r, y - r, x + r, y + r], fill=rnd.choice(["#2b8a3e", "#37b24d", "#2f9e44"]))
    # The river winding through, and its arms.
    path = [(x, 360 + 140 * math.sin(x / 190)) for x in range(-20, W + 40, 20)]
    pen.line(path, fill="#a5d8ff", width=150, joint="curve")
    pen.line(path, fill="#4dabf7", width=110, joint="curve")
    arm = [(x, 360 + 140 * math.sin(x / 190) + (x - 700) * 0.9) for x in range(700, W + 40, 20)]
    pen.line(arm, fill="#4dabf7", width=46, joint="curve")
    pen.polygon([(560, 330), (640, 330), (620, 350), (580, 350)], fill="#fff3bf")
    return art


def walk():
    art = gradient("#bfe9ff", "#f1fbff")
    pen = ImageDraw.Draw(art)
    pen.ellipse([960, 90, 1100, 230], fill="#ffe066")
    pen.ellipse([-200, 330, 900, 900], fill="#8ce99a")
    pen.ellipse([500, 380, 1600, 980], fill="#69db7c")
    pen.ellipse([-100, 470, 1400, 1100], fill="#51cf66")
    pen.polygon([(560, H), (760, H), (660, 470), (630, 470)], fill="#f3d9a4")
    for x, y, s in [(200, 420, 1.0), (330, 450, 0.8), (1000, 440, 1.1), (1130, 480, 0.9)]:
        pen.rectangle([x - 10 * s, y, x + 10 * s, y + 90 * s], fill="#8a5a2b")
        pen.ellipse([x - 60 * s, y - 110 * s, x + 60 * s, y + 20 * s], fill="#2b8a3e")
    return art


def cinema():
    art = Image.new("RGB", (W, H), "#120d1c")
    screen_art = gradient("#74c0fc", "#b197fc", (760, 330))
    art.paste(screen_art, (260, 60))
    glow(art, (640, 225), 330, "#9775fa", 60)
    overlay(art, lambda p: p.polygon([(640, 720), (260, 390), (1020, 390)], fill=(200, 200, 255, 28)))
    pen = ImageDraw.Draw(art)
    for row, y in enumerate((470, 560, 650)):
        for x in range(-40 + 30 * (row % 2), W, 90):
            pen.ellipse([x, y, x + 78, y + 110], fill="#07050c")
    return art


def map_art():
    art = Image.new("RGB", (W, H), "#eef2e6")
    pen = ImageDraw.Draw(art)
    pen.polygon([(0, 520), (420, 470), (700, 560), (1280, 500), (1280, 720), (0, 720)], fill="#a5d8ff")
    for box in ([80, 80, 330, 260], [820, 110, 1110, 300], [430, 300, 620, 420]):
        pen.rounded_rectangle(box, 20, fill="#b2f2bb")
    for line in ([(0, 330), (1280, 280)], [(380, 0), (520, 720)], [(760, 0), (980, 520)]):
        pen.line(line, fill="#ced4da", width=44)
        pen.line(line, fill="#ffffff", width=32)
    for x, y in [(470, 250), (900, 360), (300, 420)]:
        pen.ellipse([x - 34, y - 90, x + 34, y - 22], fill="#fa5252")
        pen.polygon([(x - 28, y - 46), (x + 28, y - 46), (x, y)], fill="#fa5252")
        pen.ellipse([x - 13, y - 69, x + 13, y - 43], fill="#ffffff")
    return art


def harbor():
    art = gradient("#2b3a67", "#f4a259")
    pen = ImageDraw.Draw(art)
    pen.ellipse([560, 300, 720, 460], fill="#ffd8a8")
    sea = gradient("#5c4b6b", "#1f2a44", (W, 340))
    art.paste(sea, (0, 380))
    for y in range(400, H, 22):
        overlay(art, lambda p, y=y: p.line([(560, y), (720, y)], fill=(255, 216, 168, 110), width=6))
    for x, s in [(160, 1.0), (430, 0.8), (880, 1.2), (1110, 0.7)]:
        pen.polygon([(x - 90 * s, 430), (x + 90 * s, 430), (x + 60 * s, 470), (x - 60 * s, 470)], fill="#141a2e")
        pen.line([(x, 430), (x, 300 * s + 100)], fill="#141a2e", width=6)
        pen.polygon([(x + 4, 320 * s + 90), (x + 4, 420), (x + 70 * s, 420)], fill="#141a2e")
    return art


def keyboard():
    art = gradient("#2d3142", "#1b1e2b")
    pen = ImageDraw.Draw(art)
    pen.rounded_rectangle([120, 180, 1160, 600], 40, fill="#3f4458")
    accents = {(0, 0): "#ff6b6b", (3, 6): "#ffd43b", (2, 11): "#69db7c"}
    for r in range(4):
        for c in range(12):
            x, y = 160 + c * 82, 220 + r * 92
            pen.rounded_rectangle([x, y, x + 70, y + 78], 14, fill=accents.get((r, c), "#e9ecef"))
    pen.rounded_rectangle([406, 590 - 10, 874, 590 - 4], 3, fill="#3f4458")
    return art


def garden():
    art = gradient("#a5d8ff", "#e7f5ff")
    pen = ImageDraw.Draw(art)
    for i, x in enumerate(range(-60, W, 260)):
        top = 300 + (i % 3) * 40
        pen.rectangle([x, top, x + 240, H], fill=["#e8590c", "#d9480f", "#f08c00"][i % 3])
        for wy in range(top + 80, H, 90):
            for wx in range(x + 30, x + 210, 70):
                pen.rectangle([wx, wy, wx + 40, wy + 50], fill="#fff4e6")
        for gx in range(x + 10, x + 240, 46):
            pen.ellipse([gx - 26, top - 46, gx + 26, top + 6], fill="#40c057")
    return art


LOOKS = {
    "phone": phone,
    "lanterns": lanterns,
    "car": car,
    "coffee": coffee,
    "stadium": stadium,
    "island": island,
    "bus": bus,
    "delta": delta,
    "walk": walk,
    "cinema": cinema,
    "map": map_art,
    "harbor": harbor,
    "keyboard": keyboard,
    "garden": garden,
}


def fill(image, w, h):
    """`image` cropped from its middle to fill w×h, as tuinews does."""
    scale = max(w / image.width, h / image.height)
    resized = image.resize((round(image.width * scale), round(image.height * scale)), Image.LANCZOS)
    left, top = (resized.width - w) // 2, (resized.height - h) // 2
    return resized.crop((left, top, left + w, top + h))


in_picture = set()
for picture in screen["pictures"]:
    x, y, w, h = picture["shown"]
    in_picture.update((cx, cy) for cx in range(x, x + w) for cy in range(y, y + h))

cells = screen["cells"]
# Backgrounds first, then characters, so an icon wider than its cell isn't
# painted over by the next cell's background.
for y in range(ROWS):
    for x in range(COLS):
        if (x, y) not in in_picture:
            draw_background(x, y, cells[y * COLS + x])
for y in range(ROWS):
    for x in range(COLS):
        if (x, y) not in in_picture:
            draw_glyph(x, y, cells[y * COLS + x])

for picture in screen["pictures"]:
    ax, ay, aw, ah = rect_px(picture["area"])
    sx, sy, sw, sh = rect_px(picture["shown"])
    art = fill(LOOKS[picture["look"]](), aw, ah)
    canvas.paste(art.crop((sx - ax, sy - ay, sx - ax + sw, sy - ay + sh)), (sx, sy))

# Rounded corners, like a window.
mask = Image.new("L", canvas.size, 0)
ImageDraw.Draw(mask).rounded_rectangle([0, 0, width - 1, height - 1], RADIUS, fill=255)
out = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
out.paste(canvas, (0, 0), mask)
os.makedirs(os.path.dirname(OUT) or ".", exist_ok=True)
out.save(OUT, optimize=True)
print(f"wrote {OUT} ({width}×{height})")
