#!/usr/bin/env python3
"""Draw the project mark.

Two ideas, no borrowed brand: the fan of arcs is the transmission, and the
square wave under it is what this project actually moves — raw on/off
timings. Everything is drawn at 4x and downsampled, then trimmed to its own
bounding box and centred, which is also what Home Assistant's brand images
ask for (square, transparent, minimal empty space at the edges).

    uv run --with pillow brand/generate.py

Outputs `icon.png` (256px) and `icon@2x.png` (512px) next to this file. The
same two files are vendored into the Home Assistant integration at
https://github.com/Aetf/hass-hackrf-proxy under `custom_components/
hackrf_proxy/brand/`, since Home Assistant serves an integration's brand
images from the integration itself.
"""

from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw

SUPERSAMPLE = 4
INK = (30, 84, 138, 255)  # deep blue: legible on light and dark backgrounds
ACCENT = (240, 138, 44, 255)  # amber: the signal itself


def draw(size: int) -> Image.Image:
    """Draw the mark at `size`, untrimmed."""
    n = size * SUPERSAMPLE
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    u = n / 256.0  # design units, so the geometry below reads at 256px

    apex = (128 * u, 158 * u)

    # Fan of arcs, opening upward (in Pillow, 270 degrees is up).
    for radius, width in ((44, 15), (78, 14), (112, 13)):
        d.arc(
            [
                apex[0] - radius * u,
                apex[1] - radius * u,
                apex[0] + radius * u,
                apex[1] + radius * u,
            ],
            start=203,
            end=337,
            fill=INK,
            width=int(width * u),
        )

    # Feed point.
    r = 14 * u
    d.ellipse([apex[0] - r, apex[1] - r, apex[0] + r, apex[1] + r], fill=ACCENT)

    # The burst: an on/off pulse train, flat-topped like real OOK timings.
    hi, lo = 190 * u, 224 * u
    x = 26 * u
    widths = [36, 22, 28, 22, 44, 22, 36]
    points, up = [(x, lo)], True
    for w in widths:
        points.append((x, hi if up else lo))
        x += w * u
        points.append((x, hi if up else lo))
        up = not up
    points.append((x, lo))
    d.line(points, fill=ACCENT, width=int(13 * u), joint="curve")

    return img


def render(size: int, margin: int) -> Image.Image:
    """Trim the mark to its content and centre it with a uniform margin."""
    art = draw(size)
    art = art.crop(art.getbbox())
    span = size - 2 * margin
    scale = min(span / art.width, span / art.height)
    # Pillow types resize's size parameter against numpy, which is not
    # installed here, so the whole member reads as partially unknown.
    art = art.resize(  # pyright: ignore[reportUnknownMemberType]
        (max(1, round(art.width * scale)), max(1, round(art.height * scale))),
        Image.Resampling.LANCZOS,
    )
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(art, ((size - art.width) // 2, (size - art.height) // 2), art)
    return out


def main() -> None:
    """Write both sizes next to this file."""
    here = Path(__file__).resolve().parent
    for size, margin, name in ((256, 8, "icon.png"), (512, 16, "icon@2x.png")):
        image = render(size, margin)
        image.save(here / name, optimize=True)
        print(f"{name}: {image.size[0]}x{image.size[1]}, content {image.getbbox()}")


if __name__ == "__main__":
    main()
