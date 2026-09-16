"""Measure emitted liquid luminance from native linear scRGB captures."""
from pathlib import Path
import json,struct,statistics,sys
p=Path(sys.argv[1]);out=[]
for i in range(5):
 f=p/f'material-{i:02d}.rgba32f';meta=json.loads(f.with_suffix('.json').read_text());w=meta['width'];h=meta['height']
 pixels=list(struct.iter_unpack('<ffff',f.read_bytes()))
 opaque=[(k%w,k//w) for k,v in enumerate(pixels) if v[3]>.99]
 assert opaque,'Missing captured capsule'
 top=min(y for x,y in opaque);bottom=max(y for x,y in opaque);cx=round((min(x for x,y in opaque)+max(x for x,y in opaque))/2)
 def sample(y):
  vals=[(.2126*r+.7152*g+.0722*b)*80 for yy in range(round(y)-2,round(y)+3) for xx in range(cx-2,cx+3) for r,g,b,a in [pixels[yy*w+xx]] if a>.99]
  return statistics.mean(vals)
 frac=i/4;r={'fillPercent':i*25,'sdrWhiteNits':meta['sdrWhiteNits'],'displayPeakNits':meta['peakNits']}
 if i>0:r['liquidBodyNits']=sample(bottom-(bottom-top)*frac*.5);r['liquidToSdrWhiteRatio']=r['liquidBodyNits']/r['sdrWhiteNits']
 if i<4:r['unfilledBodyNits']=sample(top+(bottom-top)*(1-frac)*.5)
 if i>0 and r['displayPeakNits']>=2*r['sdrWhiteNits']:assert r['liquidToSdrWhiteRatio']>=1.5,r
 if i==0:assert r['unfilledBodyNits']<=r['sdrWhiteNits']*1.05,r
 out.append(r)
(p/'liquid-hdr-analysis.json').write_text(json.dumps(out,indent=2),encoding='utf-8')
print(json.dumps(out,indent=2))
