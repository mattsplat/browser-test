"""Stitch each page's screenshots side by side: Chrome | Servo | Blitz | Gosub.

Every tile is cropped to the shared 1280x1400 viewport (Gosub captures the full page)
and scaled to half size. A missing screenshot shows as a red tile.
run: uv run --with pillow python compare.py
"""
import pathlib

from PIL import Image, ImageDraw

ENGINES = ("chrome", "servo", "blitz", "gosub")
W, H = 1280, 1400
root = pathlib.Path(__file__).parent
out_dir = root / "out" / "compare"
out_dir.mkdir(parents=True, exist_ok=True)

for chrome in sorted((root / "out" / "chrome").glob("*.png")):
    name = chrome.stem
    sheet = Image.new("RGB", (len(ENGINES) * (W // 2 + 10), H // 2), "#888")
    for i, engine in enumerate(ENGINES):
        path = root / "out" / engine / f"{name}.png"
        if path.exists():
            shot = Image.open(path).convert("RGB")
            tile = Image.new("RGB", (W, H), "white")
            tile.paste(shot.crop((0, 0, min(shot.width, W), min(shot.height, H))))
            tile = tile.resize((W // 2, H // 2))
            label = engine.upper()
        else:
            tile = Image.new("RGB", (W // 2, H // 2), "#f88")
            label = engine.upper() + " (none)"
        draw = ImageDraw.Draw(tile)
        draw.rectangle([0, 0, 110, 22], fill="black")
        draw.text((6, 5), label, fill="white")
        sheet.paste(tile, (i * (W // 2 + 10), 0))
    sheet.save(out_dir / f"{name}.png")
print(f"wrote {out_dir}")
