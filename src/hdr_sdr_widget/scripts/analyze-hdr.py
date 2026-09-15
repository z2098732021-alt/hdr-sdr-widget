"""Read actual FP16-target exports; never infer HDR luminance from PNG."""
from pathlib import Path
import json, struct, sys
from PIL import Image, ImageDraw
folder=Path(sys.argv[1])
reports=[]
for file in sorted(folder.glob('material-*.rgba32f')):
 meta=json.loads(file.with_suffix('.json').read_text())
 w,h=meta['width'],meta['height']
 pixels=list(struct.iter_unpack('<ffff',file.read_bytes()))
 opaque=[(.2126*r+.7152*g+.0722*b)*80 for r,g,b,a in pixels if a>.99]
 if not opaque:
  reports.append({'file':file.name,'error':'No opaque pixels; invalid capture'});continue
 peak=meta['peakNits']; white=meta['sdrWhiteNits']
 index=int(file.stem.split('-')[-1])
 report={'stage':index,'whiteNits':white,'peakNits':peak,'maximumNits':max(opaque),
 'nearPeakAreaPercent':100*sum(v>=.9*peak for v in opaque)/len(opaque)}
 if index<5 and index%5 in (1,2,3):
  # Known native acceptance geometry at this machine's 175% DPI.
  hh=175*(1.012 if index>=3 else 1)
  top=199.5+hh-2*hh*(index%5)/4
  values=[]
  for y in range(max(0,int(top)),min(h,int(top)+6)):
   for x in range(54,64):
    r,g,b,a=pixels[y*w+x]
    if a>.99: values.append((.2126*r+.7152*g+.0722*b)*80)
  report['whiteBoundaryContrast']=(white*1.05)/(min(values)+white*.05)
 reports.append(report)
(folder/'hdr-analysis.json').write_text(json.dumps(reports,indent=2),encoding='utf-8')
print(json.dumps(reports,indent=2))
files=[folder/f'material-{i:02}.ppm' for i in range(20)]
if all(f.exists() for f in files):
 canvas=Image.new('RGB',(750,1760),'#1c2028');draw=ImageDraw.Draw(canvas)
 for i,f in enumerate(files):
  row,col=divmod(i,5)
  im=Image.open(f).convert('RGB').crop((0,0,150,399))
  canvas.paste(im,(col*150,row*440+30))
  draw.text((col*150+8,row*440+8),f'{["White","Gray","Black","Card"][row]} {col*25}%',fill='white')
 canvas.save(folder/'material-grid.png')
