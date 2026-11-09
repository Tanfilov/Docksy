"""Builds Docksy's icon files from the logo artwork (logo.png, transparent):
  docksy.ico                      app/exe icon, 16..256 px
  ../../installer/wizard*.bmp     installer wizard images (100/150/200% DPI)
Needs Pillow. Run: python make_icon.py"""
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).parent
INSTALLER = HERE.parent.parent / "installer"
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]


def logo():
    im = Image.open(HERE / "logo.png").convert("RGBA")
    # drop the faint glow speckles around the artwork, then crop to it
    a = im.getchannel("A").point(lambda v: 0 if v < 40 else v)
    im.putalpha(a)
    im = im.crop(a.getbbox())
    side = max(im.size)
    sq = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    sq.paste(im, ((side - im.width) // 2, (side - im.height) // 2))
    return sq


def fit(img, w, h, margin):
    """img scaled to fit w x h minus a margin, centred on a transparent canvas."""
    s = min((w - 2 * margin) / img.width, (h - 2 * margin) / img.height)
    r = img.resize((max(1, round(img.width * s)), max(1, round(img.height * s))), Image.LANCZOS)
    out = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    out.alpha_composite(r, ((w - r.width) // 2, (h - r.height) // 2))
    return out


def vertical_gradient(w, h, top, bottom):
    g = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(g)
    for y in range(h):
        t = y / max(1, h - 1)
        d.line([(0, y), (w, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return g.convert("RGBA")


def main():
    art = logo()

    # app icon: every size resampled from the full artwork
    ico = HERE / "docksy.ico"
    art.resize((256, 256), Image.LANCZOS).save(ico, sizes=[(n, n) for n in ICO_SIZES])
    print("wrote", ico)

    # wizard images (BMP, 24-bit): the big one on a dark gradient (the glow
    # needs a dark backdrop), the small header one on the header's white
    for scale, suffix in [(1.0, "100"), (1.5, "150"), (2.0, "200")]:
        w, h = round(164 * scale), round(314 * scale)
        big = vertical_gradient(w, h, (20, 22, 44), (8, 9, 20))
        big.alpha_composite(fit(art, w, w, round(14 * scale)), (0, (h - w) // 2 - round(20 * scale)))
        big.convert("RGB").save(INSTALLER / f"wizard-{suffix}.bmp")

        s = round(55 * scale), round(58 * scale)
        small = Image.new("RGBA", s, (255, 255, 255, 255))
        clean = art.copy()
        clean.putalpha(clean.getchannel("A").point(lambda v: 0 if v < 110 else v))
        small.alpha_composite(fit(clean, *s, round(2 * scale)))
        small.convert("RGB").save(INSTALLER / f"wizard-small-{suffix}.bmp")
    print("wrote wizard images to", INSTALLER)


if __name__ == "__main__":
    main()
