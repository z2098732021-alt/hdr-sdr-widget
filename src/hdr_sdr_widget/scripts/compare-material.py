"""Compare material captures; source rows are native GPU output, not HDR proof."""
from pathlib import Path
from PIL import Image,ImageDraw
import sys
old,new,out=map(Path,sys.argv[1:4])
canvas=Image.new('RGB',(600,880),'#171c25');d=ImageDraw.Draw(canvas)
for row,(folder,label) in enumerate([(old,'Before'),(new,'Contour')]):
 for col,(stage,bg) in enumerate([(2,'White'),(7,'Light gray'),(12,'Dark'),(17,'Pattern')]):
  im=Image.open(folder/f'material-{stage:02d}.ppm').crop((0,0,150,398))
  canvas.paste(im,(col*150,row*440+30))
  d.text((col*150+5,row*440+8),f'{label} / {bg}',fill='white')
canvas.save(out)
