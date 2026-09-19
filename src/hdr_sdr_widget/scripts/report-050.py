"""Build a five-fixture optical comparison from GPU readbacks (Pillow required)."""
from pathlib import Path
import sys
from PIL import Image, ImageDraw

root = Path(sys.argv[1])
canvas = Image.new("RGB", (1150, 1040), "#20242b")
draw = ImageDraw.Draw(canvas)
for row, folder in enumerate(["optics-before", "optics-after"]):
    for column, name in enumerate(["Grid", "Rings", "Checker", "Bars", "Text"]):
        picture = Image.open(root / folder / f"optics-{column}.ppm").convert("RGB")
        assert picture.getbbox() is not None, f"Empty GPU capture: {folder}/{name}"
        picture.thumbnail((220, 475))
        x, y = column * 230 + 5, row * 520
        canvas.paste(picture, (x, y + 35))
        draw.text((x, y + 10), f'{"Before" if row == 0 else "0.5.0"} - {name}', fill="white")
canvas.save(root / "optics-comparison.png")
