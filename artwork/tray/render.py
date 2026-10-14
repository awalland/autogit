"""Render tray PNGs and a preview. Requires CairoSVG and Pillow."""

from pathlib import Path

import cairosvg
from PIL import Image, ImageDraw, ImageFont


SOURCE = Path(__file__).resolve().parent
ICONS = SOURCE.parents[1] / "autogit-daemon" / "assets" / "icons"
STATES = ("idle", "syncing", "suspended", "error")


def font(size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype("DejaVuSans.ttf", size)


def main() -> None:
    for state in STATES:
        cairosvg.svg2png(
            url=str(SOURCE / f"{state}.svg"),
            write_to=str(ICONS / f"{state}.png"),
            output_width=64,
            output_height=64,
        )

    preview = Image.new("RGB", (1120, 590), "#eef0f4")
    draw = ImageDraw.Draw(preview)
    draw.text((32, 23), "Autogit tray icons", fill="#20242e", font=font(26))
    draw.text((32, 61), "One identity, four distinct states", fill="#647080", font=font(16))
    centers = (164, 428, 692, 956)
    for state, center in zip(STATES, centers):
        draw.rounded_rectangle((center - 116, 106, center + 116, 264), radius=16, fill="#20242e")
        icon = Image.open(ICONS / f"{state}.png").convert("RGBA")
        hero = icon.resize((96, 96), Image.Resampling.LANCZOS)
        preview.paste(hero, (center - 48, 119), hero)
        draw.text((center, 235), state.capitalize(), fill="#eef0f4", font=font(18), anchor="mm")

    for top, background, foreground, label in (
        (295, "#ffffff", "#20242e", "Light background"),
        (438, "#20242e", "#eef0f4", "Dark background"),
    ):
        draw.rounded_rectangle((32, top, 1088, top + 125), radius=14, fill=background)
        draw.text((52, top + 15), label, fill=foreground, font=font(14))
        for state, center in zip(STATES, centers):
            icon = Image.open(ICONS / f"{state}.png").convert("RGBA")
            for size, offset in ((16, -54), (22, 0), (32, 54)):
                small = icon.resize((size, size), Image.Resampling.LANCZOS)
                position = (center + offset - size // 2, top + 64 - size // 2)
                preview.paste(small, position, small)
                draw.text((center + offset, top + 99), str(size), fill=foreground, font=font(12), anchor="mm")
    preview.save(SOURCE / "preview.png")


if __name__ == "__main__":
    main()
