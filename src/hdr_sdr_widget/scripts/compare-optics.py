"""Compare native GPU test-card dumps between versions; no UI capture or injection."""
from pathlib import Path
import sys
from PIL import Image, ImageDraw
old,new,out=map(Path,sys.argv[1:4])
labels=sys.argv[4:6] if len(sys.argv)>=6 else ['0.4.0','0.4.1']
out.mkdir(parents=True,exist_ok=True)
stages=[(2,'Rest'),(4,'Hover'),(6,'Press'),(8,'Drag')]
tile_w=150
canvas=Image.new('RGB',(tile_w*4,880),'#171b24')
draw=ImageDraw.Draw(canvas)
for row,(folder,version) in enumerate([(old,labels[0]),(new,labels[1])]):
 for col,(sec,name) in enumerate(stages):
  im=Image.open(folder/f'shape-{sec}.ppm').convert('RGB')
  im.save(out/f'{version}-{name.lower()}.png')
  # Crop empty host margin only. Keep pixels at native 1:1 size.
  crop=im.crop((0,0,min(tile_w,im.width),im.height))
  y=row*440
  canvas.paste(crop,(col*tile_w,y+30))
  draw.text((col*tile_w+8,y+8),f'{version} {name}',fill='white')
canvas.save(out/'optics-comparison.png')
