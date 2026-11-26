"""Draws the app icons (python scripts\\make-icons.py; needs Pillow) and writes `app.ico` next to each app.
One look for all: a graphite rounded tile with a soft shadow and a faint top light, one glyph in the app's colour
(the same colours as the page tiles of rust-settings). The file manager is the exception: its folder alone, filling the
icon, like the system's folder icons (on the tile it looked small in the taskbar). Everything is drawn 4x too big and shrunk with LANCZOS: smooth edges,
and every ico frame is made from the master, not stretched from a small one."""
import io, math, struct, sys
from pathlib import Path
from PIL import Image, ImageChops, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
M = 1024  # master canvas
SIZES = [16, 24, 32, 48, 64, 128, 256]


def mask(draw_fn):
    im = Image.new("L", (M, M), 0)
    draw_fn(ImageDraw.Draw(im))
    return im


def rr(box, r):
    return lambda d: d.rounded_rectangle(box, r, fill=255)


def union(*fns):
    def f(d):
        for fn in fns:
            fn(d)
    return f


def poly_line(points, w):
    """A polyline with round caps and joins."""
    def f(d):
        d.line(points, fill=255, width=w, joint="curve")
        for x, y in points:
            d.ellipse((x - w / 2, y - w / 2, x + w / 2, y + w / 2), fill=255)
    return f


def gradient(top, bottom):
    g = Image.new("RGB", (1, M))
    for y in range(M):
        t = y / (M - 1)
        g.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return g.resize((M, M))


def lighter(c, k):
    return tuple(min(255, round(v + (255 - v) * k)) for v in c)


def darker(c, k):
    return tuple(round(v * (1 - k)) for v in c)


def paint(canvas, m, color, spread=0.22):
    """Fills mask m with a colour that is a little lighter on top and darker below."""
    canvas.paste(gradient(lighter(color, spread), darker(color, spread * 0.8)), (0, 0), m)


def cut(c, m):
    """Erases mask m from the glyph layer: the tile shows through."""
    c.putalpha(ImageChops.subtract(c.getchannel("A"), m))


def tile():
    t = Image.new("RGBA", (M, M), (0, 0, 0, 0))
    box = (56, 40, M - 56, M - 72)
    shape = mask(rr(box, 224))
    # soft drop shadow
    sh = Image.new("RGBA", (M, M), (0, 0, 0, 0))
    sh.paste((0, 0, 0, 120), (0, 26), shape.filter(ImageFilter.GaussianBlur(26)))
    t = Image.alpha_composite(t, sh)
    body = gradient((40, 43, 53), (21, 23, 29)).convert("RGBA")
    body.putalpha(shape)
    t = Image.alpha_composite(t, body)
    # hairline light along the rim, strongest at the top
    inner = mask(rr((box[0] + 5, box[1] + 5, box[2] - 5, box[3] - 5), 219))
    rim = ImageChops.subtract(shape, inner)
    fade = gradient((255, 255, 255), (60, 60, 60)).convert("L").point(lambda v: v * 38 // 255)
    rim_a = ImageChops.multiply(rim, fade)
    rim_img = Image.new("RGBA", (M, M), (255, 255, 255, 0))
    rim_img.putalpha(rim_a)
    return Image.alpha_composite(t, rim_img), shape


# ---- glyphs: each draws onto `c` (RGB master) and returns nothing; the tile colour is `ink` for cut-outs ----


def folder(c, col):
    back = mask(union(rr((250, 300, 500, 470), 56), rr((250, 360, 774, 740), 64)))
    paint(c, back, darker(col, 0.28))
    paint(c, mask(rr((250, 438, 774, 740), 64)), col)


def folder_full(c, col):
    """The file manager's folder on its own, as big as the icon (no tile): a soft shadow, the back with its tab, the front."""
    l, r, top, bottom = 52, M - 52, 150, M - 150
    back = mask(union(rr((l, top, 470, top + 190), 84), rr((l, top + 80, r, bottom), 92)))
    front = mask(rr((l, top + 196, r, bottom), 92))
    sh = Image.new("RGBA", (M, M), (0, 0, 0, 0))
    sh.paste((0, 0, 0, 110), (0, 22), ImageChops.lighter(back, front).filter(ImageFilter.GaussianBlur(22)))
    c.alpha_composite(sh)
    paint(c, back, darker(col, 0.28))
    paint(c, front, col)
    # a faint light along the top edge of the front
    edge = ImageChops.subtract(front, mask(rr((l, top + 214, r, bottom), 92)))
    hl = Image.new("RGBA", (M, M), (255, 255, 255, 0))
    hl.putalpha(edge.point(lambda v: v * 70 // 255))
    c.alpha_composite(hl)


def gear(c, col):
    n, ro, ri = 8, 330, 262
    pts = []
    for i in range(n):
        a = 2 * math.pi * i / n
        for da, r in ((-0.20, ri), (-0.13, ro), (0.13, ro), (0.20, ri)):
            pts.append((512 + r * math.cos(a + da), 512 + r * math.sin(a + da)))
    g = mask(lambda d: (d.polygon(pts, fill=255), d.ellipse((512 - ri - 8, 512 - ri - 8, 512 + ri + 8, 512 + ri + 8), fill=255)))
    hole = mask(lambda d: d.ellipse((512 - 108, 512 - 108, 512 + 108, 512 + 108), fill=255))
    paint(c, ImageChops.subtract(g, hole), col)


def bar(c, col):
    for i, (x0, x1) in enumerate(((252, 500), (524, 772))):  # two ghost windows above the bar
        paint(c, mask(rr((x0, 270, x1, 560), 44)), darker(col, 0.62), 0.1)
    paint(c, mask(rr((236, 600, 788, 740), 70)), col)
    cuts = mask(lambda d: [d.ellipse((x - 26, 670 - 26, x + 26, 670 + 26), fill=255) for x in (352, 432)] + [d.rounded_rectangle((584, 650, 700, 690), 20, fill=255)])
    cut(c, cuts)


def treemap(c, col):
    rects = [((246, 250, 536, 774), 0.0), ((568, 250, 778, 470), 0.12), ((568, 502, 664, 774), 0.26), ((696, 502, 778, 774), 0.4)]
    for box, k in rects:
        paint(c, mask(rr(box, 34)), lighter(col, k) if k else col)


def picture(c, col):
    card = mask(rr((236, 290, 788, 734), 76))
    paint(c, card, col)
    sun = mask(lambda d: d.ellipse((600 - 54, 410 - 54, 600 + 54, 410 + 54), fill=255))
    hills = mask(lambda d: d.polygon([(236, 760), (236, 650), (380, 510), (480, 610), (590, 530), (788, 710), (788, 760)], fill=255))
    paint(c, ImageChops.multiply(hills, card), darker(col, 0.38), 0.1)
    paint(c, sun, lighter(col, 0.75), 0.05)


def package(c, col):
    paint(c, mask(poly_line([(268, 590), (268, 736), (756, 736), (756, 590)], 64)), darker(col, 0.25))
    paint(c, mask(union(poly_line([(512, 262), (512, 590)], 68), poly_line([(372, 462), (512, 602), (652, 462)], 68))), col)


APPS = {
    "plugins/rust-explorer": (folder_full, (10, 132, 255), False),
    "plugins/rust-settings": (gear, (174, 180, 194)),
    "plugins/topbar": (bar, (124, 134, 255)),
    "plugins/wiztree": (treemap, (255, 159, 10)),
    "plugins/photoview": (picture, (191, 90, 242)),
    ".": (package, (48, 209, 88)),
}


def render(glyph, col, with_tile=True):
    base = tile()[0] if with_tile else Image.new("RGBA", (M, M), (0, 0, 0, 0))
    layer = Image.new("RGBA", (M, M), (0, 0, 0, 0))
    glyph(layer, col)
    return Image.alpha_composite(base, layer)


def frame_bytes(img, s):
    buf = io.BytesIO()
    img.resize((s, s), Image.LANCZOS).save(buf, "PNG")
    return buf.getvalue()


def write_ico(path, img):
    frames = [(s, frame_bytes(img, s)) for s in SIZES]
    head = struct.pack("<HHH", 0, 1, len(frames))
    off = 6 + 16 * len(frames)
    entries, data = b"", b""
    for s, b in frames:
        entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(b), off + len(data))
        data += b
    path.write_bytes(head + entries + data)


if __name__ == "__main__":
    sheet = Image.new("RGBA", (256 * len(APPS), 256), (255, 255, 255, 0))
    for i, (rel, (glyph, col, *opt)) in enumerate(APPS.items()):
        img = render(glyph, col, *opt)
        write_ico(ROOT / rel / "app.ico", img)
        sheet.paste(img.resize((256, 256), Image.LANCZOS), (256 * i, 0))
    if len(sys.argv) > 1:
        sheet.save(sys.argv[1])
    print("wrote", len(APPS), "icons")
