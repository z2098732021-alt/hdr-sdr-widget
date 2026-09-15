"""Same-card optical comparison: unchanged 0.3.4 Canvas code vs native GPU captures.

Only desktop capture/IPC and stable interaction state are supplied by this harness.
Open http://127.0.0.1:8901/compare in a browser at 100% zoom.
"""
from pathlib import Path
import importlib.util
import math
from http.server import ThreadingHTTPServer
from PIL import Image

ROOT=Path(__file__).resolve().parent.parent
QA=ROOT/'deliverables/v0.4.0-native/qa/acceptance-clarity'
spec=importlib.util.spec_from_file_location('legacy_preview',ROOT/'tools/glass_preview.py')
legacy=importlib.util.module_from_spec(spec);spec.loader.exec_module(legacy)
card=Image.new('RGBA',(70,350))
def srgb(c):return round(255*(12.92*c if c<=.0031308 else 1.055*c**(1/2.4)-.055))
for y in range(350):
    for x in range(70):
        px,py=941+x+.5,29+y+.5
        white=math.floor(px/2)%2 if py%160<80 else (math.floor(px/8)+math.floor(py/8))%2
        rgb=(.85,.88,.92) if white else (.06,.12,.23)
        card.putpixel((x,y),(*map(srgb,rgb),255))
card.save(QA/'shared-card.png')
legacy.DEFAULT_FRAME=str(QA/'shared-card.png')
legacy._frame_bytes=card.tobytes();legacy._frame_bytes_bright=legacy._frame_bytes
legacy.INJECT+='''<style>body{overflow:hidden!important}.capsule{transition:none!important}</style><script>
setInterval(()=>{const c=document.querySelector('.capsule');if(!c)return;
const s=new URLSearchParams(location.search).get('shape');
c.dataset.hover=s==='hover'?'true':'false';c.dataset.pressed=s==='press'?'true':'false';
c.style.transform=s==='hover'?'scale(1.04,1.012)':s==='press'?'scale(1.02,.985)':'none';
},50);</script>'''
for stage in [2,4,6]:
    im=Image.open(QA/f'shape-{stage}.ppm').convert('RGBA').crop((0,0,119,399))
    im.putdata([(r,g,b,0 if (r,g,b)==(0,0,0) else 255) for r,g,b,a in im.getdata()])
    im.save(QA/f'native-{stage}.png')
class Handler(legacy.Handler):
    def send(self,data,kind='text/html; charset=utf-8'):
        self.send_response(200);self.send_header('Content-Type',kind);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
    def do_GET(self):
        path=self.path.split('?')[0]
        if path=='/compare':
            panels=[]
            for row in ['old','new']:
                for state,title,stage in [('normal','原尺寸',2),('hover','稳定放大',4),('press','稳定按压',6)]:
                    url=f'/?glassDebug=1&fill=40&shape={state}' if row=='old' else f'/native/{stage}'
                    panels.append(f'<section><h2>{"0.3.4 Canvas" if row=="old" else "0.4.0 GPU"} · {title}</h2><div><iframe src="{url}"></iframe></div></section>')
            html='''<!doctype html><meta charset=utf-8><title>同测试卡 · 原生与旧版光学对照</title>
<style>body{background:#202632;color:white;font:14px Segoe UI;margin:16px}h1{font-size:20px;margin:0 0 8px}p{margin:8px 0}main{display:grid;grid-template-columns:repeat(3,300px);gap:8px}h2{font-size:14px;margin:6px}section>div{width:300px;height:435px;overflow:hidden}iframe{border:0;width:200px;height:290px;transform:scale(1.5);transform-origin:0 0}</style>
<h1>同一条纹 / 棋盘格测试卡 · 形变清晰度对照</h1><p>上：保留的真实 Canvas/CSS 光学代码；下：原生 GPU 测试输出。仅供光学比较，不是桌面端到端截图或帧率测试。</p><main>'''+''.join(panels)+'</main>'
            return self.send(html.encode())
        if path.startswith('/native/'):
            stage=path.rsplit('/',1)[1]
            if stage not in ['2','4','6']:return self.send(b'Not found','text/plain')
            return self.send(f'''<!doctype html><style>body{{margin:0;background:#16181d url('/card.png') no-repeat 74px 74px;background-size:40px 200px}}img{{position:absolute;left:60px;top:60px;width:68px;height:228px}}aside{{position:absolute;left:122px;top:74px;width:40px;height:200px;background:url('/card.png');background-size:40px 200px}}</style><aside></aside><img src="/native-{stage}.png">'''.encode())
        if path in ['/native-2.png','/native-4.png','/native-6.png']:
            return self.send((QA/path[1:]).read_bytes(),'image/png')
        return super().do_GET()
print('http://127.0.0.1:8901/compare',flush=True)
ThreadingHTTPServer(('127.0.0.1',8901),Handler).serve_forever()
