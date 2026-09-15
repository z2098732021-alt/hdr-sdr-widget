"""Build an SDR structural preview from raw GPU frames (not HDR proof)."""
from pathlib import Path
from PIL import Image, ImageDraw
import csv, subprocess, sys
p=Path(sys.argv[1]).resolve()
rows=[[float(v) for v in r] for r in csv.reader((p/'motion.csv').open())]
# The native diagnostic trace appends across repeated runs; select the latest run.
start=max(i for i,r in enumerate(rows) if r[0]==0)
rows=rows[start:]
assert len(rows)==180, 'Incomplete frame sequence'
# Use the second reveal; initial capture may begin after startup motion has started.
times=[2.40,2.46,2.53,2.62,2.76,2.95]
selected=[min(rows,key=lambda r:abs(r[1]-t)) for t in times]
assert max(r[4] for r in selected)>.98, 'Recording never fully reveals in the selected cycle'
assert min(r[4] for r in selected)<.75, 'Recording is missing the small reveal state'
canvas=Image.new('RGB',(6*130,435),'#171c25'); d=ImageDraw.Draw(canvas)
for i,r in enumerate(selected):
 frame=Image.open(p/f'frame-{int(r[0]):04d}.ppm').crop((0,0,110,398))
 canvas.paste(frame,(i*130,32))
 d.text((i*130+3,5),f'{r[1]:.2f}s / {r[4]:.0%}',fill='white')
canvas.save(p/'contact.png')
subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-y','-framerate','30.3030303','-i',str(p/'frame-%04d.ppm'),'-vf','crop=110:398:0:0,pad=320:440:100:20:color=0x171c25,drawbox=x=210:y=0:w=1:h=440:color=gray:t=fill','-c:v','libx264','-crf','18','-pix_fmt','yuv420p','-movflags','+faststart',str(p/'reveal.mp4')],check=True)
