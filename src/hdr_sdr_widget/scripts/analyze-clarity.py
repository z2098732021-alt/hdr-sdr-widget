"""Compare the same screen-space center in native GPU shape captures (Pillow required)."""
from pathlib import Path
import json
import sys
from PIL import Image, ImageChops, ImageStat, ImageDraw

folder = Path(sys.argv[1])
stages = [(2, 'Normal'), (4, 'Hover'), (6, 'Pressed'), (8, 'Drag')]
images = []
metrics = []
for stage, label in stages:
    image = Image.open(folder / f'shape-{stage}.ppm').convert('RGB')
    image.save(folder / f'shape-{stage}.png')
    images.append(image)
# Fixed physical coordinates, inside the unrefracted, unfilled center at 175% DPI.
roi = (52, 80, 64, 150)
reference = images[0].crop(roi)
for (_, label), image in zip(stages, images):
    delta = ImageStat.Stat(ImageChops.difference(reference, image.crop(roi)))
    metrics.append({'shape': label, 'meanAbsoluteRgbDelta': sum(delta.mean) / 3,
                    'maximumChannelDelta': max(e[1] for e in delta.extrema)})
canvas = Image.new('RGB', (sum(i.width for i in images), images[0].height + 40), '#141820')
draw = ImageDraw.Draw(canvas)
x = 0
for (_, label), image in zip(stages, images):
    canvas.paste(image, (x, 40)); draw.text((x + 12, 12), label, fill='white'); x += image.width
canvas.save(folder / 'shape-comparison.png')
(folder / 'clarity-metrics.json').write_text(json.dumps({'roiPhysicalPixels': roi, 'metrics': metrics,
    'scope': 'Same native shader test card, fixed desktop coordinates. Not a photograph or a legacy-version benchmark.'}, indent=2), encoding='utf-8')
print(json.dumps(metrics, indent=2))
