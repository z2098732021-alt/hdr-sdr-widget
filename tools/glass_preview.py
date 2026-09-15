#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""glass_preview.py —— 在普通浏览器里预览胶囊的**真实光学管线**。

# 为什么需要这个工具

悬浮物胶囊的外观在真机上**无法目视验收**，三条路都是死的：

1. 面板窗口调了 `WDA_EXCLUDEFROMCAPTURE`，该标志不是"只对桌面复制隐藏"，
   而是让窗口从**所有**捕获路径消失 —— Desktop Duplication、Windows Graphics
   Capture、`PrintWindow`、GDI `BitBlt`（mss 等截图库走的就是这条）、系统截图
   工具，全都拍不到。症状极有迷惑性：`EnumWindows` / `tasklist` 能看到窗口、
   位置尺寸都对，截出来的图里那块却是纯桌面，看起来像"没渲染"。
2. 即使关掉该标志，WebView2 走 DirectComposition 渲染，`PrintWindow` 仍抓不到
   内容（返回全黑）。
3. 帧循环由相位事件（`widget:state`）驱动，只在可见态采样；普通浏览器里没有
   Tauri 事件，相位永远停在 `hidden`，循环永不启动。

结果就是只能"改常量 → 打包 → 让用户看"，这正是本项目反复空转的根因。

# 它怎么绕过

把 `dist/` 静态吐出来，并在 `index.html` 注入一小段样式与脚本：

- 把 `#app` 挪到固定位置（便于按固定坐标裁图）、手动驱动帧循环。
- **把测试卡同时铺成页面背景**（`/card.png`，位置与胶囊的页面矩形精确对齐）——
  同一张卡在胶囊「内」（被折射）与「外」（未折射）同时可见，硬边的错位量就是
  折射量的直接读数。没有这一条，只能看出"有没有内容"，看不出**直边弯没弯**。
- 帧数据走 `invoke('pull_frame')`（下方 IPC 桩），**不再需要改写任何 URL**。

于是 **shader / CSS / DOM 全是真代码**，只有 DDA 与 IPC 被换掉 ——
正好把"光学层"与"采集层"隔离开，两边毛病各自独立排查。

# 用法

    # 起服务
    python tools/glass_preview.py --port 8899
    # 取图（确定性强、可放大、无窗口干扰）
    msedge --headless=new --hide-scrollbars \
      --force-device-scale-factor=1.75 --window-size=200,300 \
      --virtual-time-budget=6000 --screenshot=out.png \
      "http://127.0.0.1:8899/?glassDebug=1"

# URL 参数

- `glassDebug=1`：必需。`liquidGlass.ts` 只在带该参数时把光学 API 挂到
  `window.__glass`（不带则行为完全不变），本工具的注入脚本靠它驱动帧循环。
- `legacy=1`：把光学令牌切回 v0.3.0 改造前的取值（关掉边缘折射增益与全部泛光、
  还原被压低的漫反射），用于同一 harness 内做 A/B 对照。

# IPC 桩（重要）

前端取帧走的是 `invoke('pull_frame')` 而**不是** `glass://` 自定义协议
（原因见 `FIX-PLAN.md` §4.0.2.3），所以本服务会在注入脚本里补一个**最小 Tauri
IPC 桩**：只实现 `invoke` 与 `transformCallback`，把 `pull_frame` 接到本服务的
`/frame` 上，其余命令一律返回 `null`（光学渲染与它们无关）。

⚠️ 不要为了迁就本文件去改前端的传输方式 —— 桩存在的意义正是让**前端保持生产路径**，
否则 harness 验的就不是真代码了。

# 帧素材

默认用同目录的 `glass_testcard.png`；缺失则**自动生成**一张光学测试卡
（硬边竖条测折射与色散、细棋盘测磨砂与倍率、高亮球测泛光过曝、深色区测反差）。
用 `--frame <png>` 可换成真实桌面截图（尺寸必须正好 70×350，否则会重新生成测试卡）。
"""

from __future__ import annotations

import argparse
import os
import struct
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

from PIL import Image, ImageDraw

# 仓库根 → src/hdr_sdr_widget/dist
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
DIST = os.path.join(REPO, "src", "hdr_sdr_widget", "dist")

DEFAULT_FRAME = os.path.join(HERE, "glass_testcard.png")

# 帧尺寸 = **胶囊区域**物理像素：胶囊 40×200 DIP @175% = 70×350。
# v0.3.2 起 Rust 只抓胶囊区域（`edge.rs::update_capture`），前端据 `fh / 200` 反推
# 缩放（=1.75）。若这里仍写 119×399（整窗口），前端会算出 399/200 = 1.995 →
# 位移映射整体按错误 scale 构建，量出来的折射是假的。
FRAME_W = 70
FRAME_H = 350

INJECT = """
<style id="glass-preview-harness">
  /* 关键：让**页面背景 = 帧内容**，并额外放一条**未折射参照条**。
     胶囊页面矩形 = #app(60,60) + .capsule margin(14) = (74,74) 40×200 CSS。
     两张同尺寸同缩放的卡：
       ① x=122 → 右边那条「参照条」，**不在胶囊下面**，所见即未折射的原图；
       ② x=74  → 正压在胶囊下面（会被折射）。
     两者逐像素对照，「直边有没有被弯折」一眼可判。
     旧版这里是无关的深色径向渐变，根本看不出弯没弯（这是之前几轮
     "看不出问题在哪"的直接原因）。 */
  html, body {
    background-color: #16181d !important;
    background-image: url('/card.png'), url('/card.png') !important;
    background-repeat: no-repeat, no-repeat !important;
    background-position: 122px 74px, 74px 74px !important;
    background-size: 40px 200px, 40px 200px !important;
  }
  html.bright body { background-image: url('/card.png?bright=1'), url('/card.png?bright=1') !important; }
  /* 固定位置，便于按固定坐标裁图 */
  #app { position: fixed !important; left: 60px !important; top: 60px !important; }
</style>
<script>
  // `?legacy=1` = **v0.3.2 现状**（"光学效果全没了"那一版），用于本轮修复前/后的真对照。
  // 旧表引用的 --glass-rim-gain / --fill-hi / --fill-lo / --fill-dome / --fill-veil
  // 都已被 09-14 的令牌审计清掉或改名 → 那份 A/B 早就失真，别再拿它当基准。
  var LEGACY = {
    '--glass-disp-max': '6px',
    '--glass-disp-band': '9px',
    '--glass-ca-mag': '0.5px',
    '--glass-spec-gain': '0',
    '--glass-inner-shade': '0',
    '--glass-blur': '0.3px',
    '--glass-rim-hi': 'rgba(255,255,255,0.30)',
    '--glass-rim-lo': 'rgba(0,0,0,0.10)',
    '--fill-color': 'rgba(255,255,255,0.46)',
    '--fill-blur': '8px',
    '--fill-edge': 'rgba(255,255,255,0.7)',
    '--fill-edge-fade': '0px',
    '--fill-meniscus-dark': 'rgba(0,0,0,0)'
  };
  var params = new URLSearchParams(location.search);
  var applied = false;
  setInterval(function () {
    var g = window.__glass;
    if (!g) return;               // 等 liquidGlass.ts 在 ?glassDebug 下挂出 API
    g.startFrameLoop();           // 幂等；相位事件不存在时也保证循环在跑
    if (applied) return;
    applied = true;
    // 浅色卡：页面背景也要跟着切，否则「胶囊内是浅卡、胶囊外是深卡」
    if (params.has('bright')) document.documentElement.classList.add('bright');
    // 液面高度是本项目唯一的亮度读数，验收它必须先让它有值 ——
    // 真实运行时由 JS 按读数驱动，harness 里没有读数，所以手动摆一个中间值。
    // `?fill=<pct>` 可覆盖，用来验「不同液位下边缘折射是否都被让位遮罩救回来」。
    var fill = document.querySelector('.fill');
    if (fill && !fill.dataset.harnessSet) {
      fill.dataset.harnessSet = '1';
      fill.style.transition = 'none';
      fill.style.height = (params.get('fill') || '58') + '%';
      var cap = document.querySelector('.capsule');
      if (cap) cap.dataset.hdr = 'on';
    }
    // 令牌覆盖：`?legacy=1` 打底，`?set=--k:v,--k2:v2` 逐项追加（后者优先）。
    // 改完调 refreshMaps() 重读 —— liquidGlass.ts 的调参闭环靠这个。
    var ov = {};
    if (params.has('legacy')) { for (var k in LEGACY) ov[k] = LEGACY[k]; }
    if (params.has('set')) {
      params.get('set').split(',').forEach(function (pair) {
        var i = pair.indexOf(':');
        if (i > 0) ov[pair.slice(0, i).trim()] = pair.slice(i + 1).trim();
      });
    }
    for (var k2 in ov) document.documentElement.style.setProperty(k2, ov[k2]);
    if (Object.keys(ov).length) g.refreshMaps();
  }, 200);
</script>
<script>
  // ---- 最小 Tauri IPC 桩 ----
  //
  // 前端的帧数据走 `invoke('pull_frame')`（不走 glass:// 协议，原因见 FIX-PLAN
  // §4.0.2.3）。普通浏览器里没有 Tauri 运行时，不补这个桩的话 `invoke` 会抛错、
  // 取帧链永远拿不到帧，本 harness 就彻底失效了。
  //
  // 这里只实现 `invoke` 与 `transformCallback` 两个入口：前者把 `pull_frame`
  // 接到本服务的 /frame 上（字节格式不变：16 字节帧头 + RGBA），其余命令一律
  // 返回 null —— 光学渲染与它们无关。
  //
  // ⚠️ 不要改前端的传输方式来迁就本文件；桩就是为"前端保持生产路径"而存在的。
  window.__TAURI_INTERNALS__ = {
    invoke: function (cmd, args) {
      if (cmd === 'pull_frame') {
        return window.__harnessPullFrame((args && args.sinceSeq) || 0);
      }
      if (cmd === 'get_widget_state') {
        // 相位驱动器：`?phase=dragging` 时先报 dragging（模拟正在调亮度），
        // 4 秒后改报 visible —— 模拟"拖拽结束"。期间**不发任何事件**，
        // 于是只有"秒级保活重拉"能把它救回来：这正是要验的自愈路径。
        var ph = window.__harnessPhase || 'visible';
        return Promise.resolve({ docked: null, expanded: true, phase: ph });
      }
      if (cmd === 'get_toast_payload') {
        // 用真实场景的文案（热键冲突）—— Toast 当初就是因为"胶囊里放不下中文提示"
        // 才独立开窗的，所以这里必须验它一行到底放得下几个字。
        return Promise.resolve({
          id: 1,
          message: '热键 Ctrl+Alt+B 已被其他程序占用，请在配置里换一个组合键',
          level: 'warn',
          durationMs: 20000
        });
      }
      // ---- 诊断面板需要的命令 ----
      // 刻意构造"采集端健康、但渲染端取帧循环一次都没执行"这一组值：
      // 它正是 FIX-PLAN §4.0.2.4 新增判定规则（polls === 0）要识别的状态。
      if (cmd === 'plugin:app|version') return Promise.resolve('0.3.2');
      if (cmd === 'get_capture_stats') {
        return Promise.resolve({
          sessionCreated: 1, sessionFailed: 0,
          frameAcquired: 245, frameEmpty: 1, frameTimeout: 0,
          accessLost: 0, copyOk: 245, copyError: 0, unsupportedFormat: 0,
          lastFormat: 87, lastError: 0,
          regionW: 70, regionH: 350, hasFrame: true
        });
      }
      if (cmd === 'get_glass_status') {
        return Promise.resolve({
          status: 'no-frame', seq: 0, frameW: 0, frameH: 0, scale: 0,
          detail: '后端返回 204', rendered: 0, blank: 0,
          polls: 0, phase: 'hidden', updatedAt: Date.now() - 24000
        });
      }
      return Promise.resolve(null);
    },
    transformCallback: function () { return 0; }
  };

  // `?bright=1`：让服务端改发**浅色**测试卡。液面是白色的，浅背景下最容易糊掉 ——
  // 用户最初"可视性太低"的抱怨最可能就是在浅色区域遇到的，必须能单独验。
  var HARNESS_BRIGHT = new URLSearchParams(location.search).has('bright');
  // 相位驱动：`?phase=dragging` → 前 4 秒报 dragging（模拟调亮度中），之后报 visible。
  // **刻意不发事件** —— 只有秒级保活重拉能解冻，这样才验得到自愈。
  window.__harnessPhase = new URLSearchParams(location.search).get('phase') || 'visible';
  if (window.__harnessPhase === 'dragging') {
    setTimeout(function () { window.__harnessPhase = 'visible'; }, 4000);
  }
  // 取帧计数：每秒上报一次到服务端并落日志 —— 冻没冻住、有没有解冻，
  // 从外部读日志就能判定，不必去读页面内部状态。
  window.__harnessCount = 0;
  setInterval(function () {
    fetch('/probe?polls=' + window.__harnessCount + '&phase=' + (window.__harnessPhase || 'visible'))
      .catch(function () {});
  }, 1000);

  window.__harnessPullFrame = async function (since) {
    window.__harnessCount += 1;
    var res = await fetch('/frame' + (HARNESS_BRIGHT ? '?bright=1' : ''), { cache: 'no-store' });
    if (res.status !== 200) return new ArrayBuffer(0);
    var ab = await res.arrayBuffer();
    if (ab.byteLength < 16) return new ArrayBuffer(0);
    // 帧头与 commands.rs::pull_frame 严格一致：seq(u64) | w(u32) | h(u32)，小端，无魔数。
    var seq = Number(new DataView(ab).getBigUint64(0, true));
    // ⚠️ 无新帧必须回**空 ArrayBuffer**：前端判据是 `buf.byteLength < 16`，
    // 返回 null 会让 `null.byteLength` 抛 TypeError → 状态变 error。
    if (seq <= since) return new ArrayBuffer(0);
    return ab;   // 前端零拷贝：new Uint8ClampedArray(buf, 16)
  };
</script>
"""

_lock = threading.Lock()
_seq = 0
_frame_bytes: bytes | None = None
_frame_bytes_bright: bytes | None = None


def make_testcard(path: str, bright: bool = False) -> None:
    """生成光学测试卡：每一项都对应一个要判断的光学行为。

    `bright=True` 生成**浅色**版本。为什么必须有这一版：液面是白色的，
    **在浅色背景上才最容易糊掉** —— 用户最初"可视性太低"的抱怨最可能就是在
    浅色区域遇到的。只在深色卡上验，等于把最容易翻车的场景漏掉了。

    深/浅两版的**结构完全一致**，只有调色板不同，所以两版的读数可直接对比。
    """
    img = Image.new("RGB", (FRAME_W, FRAME_H))
    d = ImageDraw.Draw(img)
    if bright:
        # 刻意做到**接近纯白** —— 白色液面在纯白页面上是最坏情况，
        # 只测"中等浅色"等于没测到最容易翻车的地方。
        # 8 根等宽竖条；最左/最右取最高对比，因为折射在胶囊左右边缘最强。
        cols = [
            (255, 255, 255), (150, 153, 158), (255, 196, 196), (196, 255, 214),
            (200, 215, 255), (255, 244, 190), (150, 153, 158), (255, 255, 255),
        ]
        hbar, checker, dome = (60, 62, 66), (120, 122, 126), (255, 255, 255)
    else:
        cols = [
            (250, 250, 250), (0, 0, 0), (255, 60, 60), (60, 255, 120),
            (70, 120, 255), (255, 240, 120), (0, 0, 0), (250, 250, 250),
        ]
        hbar, checker, dome = (255, 255, 255), (255, 255, 255), (255, 255, 255)

    # 硬边竖条：**铺满全高**（不是只占中间一段）—— 折射会在整条左/右边上把竖边
    # 横向推开、色散把边缘染出彩边。这是位移 / 色散的主读数来源。
    cw = FRAME_W / 8.0
    for i, c in enumerate(cols):
        x0 = int(i * cw)
        x1 = (FRAME_W - 1) if i == 7 else (int((i + 1) * cw) - 1)
        d.rectangle([x0, 0, x1, FRAME_H - 1], fill=c)

    # 水平硬带：测**上下方向**的折射（含圆角帽）—— 竖条只覆盖水平方向位移。
    for y in (78, 178, 278):
        d.rectangle([0, y, FRAME_W - 1, y + 5], fill=hbar)

    # 细棋盘：看磨砂强度与放大倍率（相邻像素差的标准差 = 内容细节量）。
    for yy in range(300, 341, 3):
        for xx in range(22, 50, 3):
            if ((xx // 3) + (yy // 3)) % 2 == 0:
                d.rectangle([xx, yy, xx + 2, yy + 2], fill=checker)

    # 高亮球：过曝 / 泛光判据。
    d.ellipse([26, 214, 44, 232], fill=dome)
    img.save(path)


def load_frame(path: str, bright: bool = False) -> bytes:
    if not os.path.exists(path):
        print(f"[info] 测试卡不存在，自动生成：{path}")
        make_testcard(path, bright)
    img = Image.open(path).convert("RGBA")
    if img.size != (FRAME_W, FRAME_H):
        # 尺寸不符时**重新生成**，不要缩放：LANCZOS 会把硬边磨圆，而硬边正是
        # 折射 / 色散唯一可靠的度量基准（缩过之后读数全部失真）。
        print(f"[info] 帧素材 {img.size} ≠ {FRAME_W}×{FRAME_H}，重新生成测试卡")
        make_testcard(path, bright)
        img = Image.open(path).convert("RGBA")
    return img.tobytes()


def build_payload(bright: bool = False) -> bytes:
    """一帧 = 16 字节帧头 + RGBA 像素。seq 自增，前端才认作新帧。"""
    global _seq
    with _lock:
        _seq += 1
        seq = _seq
    data = _frame_bytes_bright if bright else _frame_bytes
    assert data is not None
    # 帧头与 commands.rs::pull_frame 严格一致：seq(u64) | w(u32) | h(u32)，小端，无魔数。
    return struct.pack("<QII", seq, FRAME_W, FRAME_H) + data


class Handler(SimpleHTTPRequestHandler):
    server_version = "glass-preview/1.0"

    def __init__(self, *a, **kw):
        super().__init__(*a, directory=DIST, **kw)

    def log_message(self, fmt, *args):
        pass  # 安静

    def do_GET(self) -> None:  # noqa: N802
        path = self.path.split("?")[0]
        if path == "/frame":
            # `?bright=1` → 浅色测试卡（液面是白的，浅背景下最容易糊掉）
            body = build_payload(bright="bright=1" in self.path)
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(body)
            return
        if path == "/probe":
            # 验收探针：把 harness 的取帧计数与当前相位记到 stdout（读日志判定冻结/解冻）
            print(f"[probe] {self.path.split('?', 1)[-1]}", flush=True)
            self.send_response(204)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            return
        if path == "/card.png":
            # 测试卡原样吐成 PNG，给页面背景用（见 INJECT 的 html,body 规则）。
            self._serve_card(bright="bright=1" in self.path)
            return
        if path in ("/", "/index.html"):
            self._serve_html("index.html")
            return
        if path.endswith(".html"):
            # toast.html / diag.html 也需要注入（桩定义在里面，否则它们的 IPC 全废）。
            self._serve_html(path.lstrip("/"))
            return
        if path.startswith("/assets/") and path.endswith(".js"):
            self._serve_js(path)
            return
        super().do_GET()

    def _serve_html(self, name: str) -> None:
        with open(os.path.join(DIST, name), "r", encoding="utf-8") as f:
            html = f.read()
        raw = html.replace("</head>", INJECT + "</head>").encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(raw)

    def _serve_card(self, bright: bool = False) -> None:
        """把测试卡原样吐成 PNG，供页面背景使用。

        为什么要这一步：只让胶囊里是测试卡，**看不出折射有没有把直边弯折** ——
        没有参照物。把同一张卡同时铺在胶囊外（未折射），硬边的错位量就是折射量
        的直接读数。
        """
        path = DEFAULT_FRAME.replace(".png", "-bright.png") if bright else DEFAULT_FRAME
        if not os.path.exists(path):
            make_testcard(path, bright)
        with open(path, "rb") as f:
            raw = f.read()
        self.send_response(200)
        self.send_header("Content-Type", "image/png")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(raw)

    def _serve_js(self, path: str) -> None:
        # 帧已走 IPC（`invoke('pull_frame')`），bundle 里**不再有**
        # `glass.localhost/frame` 这个串 —— 所以原样吐出即可。旧版会去 replace 它，
        # 每次都刷一条误导性的 warn（现在已删）。
        fs_path = os.path.join(DIST, path.lstrip("/").replace("/", os.sep))
        with open(fs_path, "r", encoding="utf-8") as f:
            raw = f.read().encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/javascript; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(raw)

    def do_OPTIONS(self) -> None:  # noqa: N802
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.end_headers()


def main() -> int:
    global _frame_bytes, _frame_bytes_bright
    ap = argparse.ArgumentParser(description="胶囊光学管线浏览器预览")
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--frame", default=DEFAULT_FRAME, help="帧素材 PNG（默认自动生成测试卡）")
    args = ap.parse_args()

    if not os.path.isdir(DIST):
        print(f"找不到构建产物目录：{DIST}\n请先在 src/hdr_sdr_widget 下跑 `npx vite build`")
        return 1

    _frame_bytes = load_frame(args.frame)
    # 浅色版：同一张卡换调色板，用来验"浅背景下液面还读不读得出"
    _frame_bytes_bright = load_frame(args.frame.replace(".png", "-bright.png"), bright=True)
    print(f"帧素材 → {FRAME_W}×{FRAME_H} RGBA，{len(_frame_bytes)} 字节")
    print(f"预览地址: http://127.0.0.1:{args.port}/?glassDebug=1")
    print(f"  改前对照: http://127.0.0.1:{args.port}/?glassDebug=1&legacy=1")
    print(f"  静态根  : {DIST}")
    print("Ctrl+C 退出")
    try:
        ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
    except KeyboardInterrupt:
        print("\nbye")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
