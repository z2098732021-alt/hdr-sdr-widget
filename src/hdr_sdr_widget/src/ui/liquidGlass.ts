//! liquidGlass.ts —— Liquid Glass 光学管线（v0.3）
//!
//! 用 Canvas 2D + JS 逐像素位移实现真实背景折射 + 边缘色散。不依赖 SVG feImage
//! （WebView2 不渲染）、不依赖 WebGL（透明窗口下会变不透明黑底）。
//!
//! 光学：
//!   - rounded-rectangle SDF 计算到玻璃边缘的有符号距离（内部为负，IQ 约定）；
//!   - `strength = 1 - smoothstep(0, band, dist)`：边缘 1 → 向内衰减 0 → 中心清晰；
//!   - 采样向中心偏移 = 透明圆角透镜（放大 / 边缘弯曲）；
//!   - R/B 通道沿径向做相反微小偏移 = 边缘色散（optical dispersion）；
//!   - 迎光侧**加性**细高光 + 紧贴内侧**乘性**压暗（见 tokens.css 的
//!     `--glass-spec-gain` / `--glass-inner-shade`。**不要**退回乘法增益 ——
//!     那会随背景亮度缩放，整圈过曝发白）；
//!   - 磨砂 / 饱和 / 亮度 / 对比仍由 CSS `filter` 标准函数叠加。
//!
//! 所有光学参数（位移 / 距离带 / 色散 / 增益）**只在 tokens.css 定义**，本模块
//! 启动时读取，不硬编码 —— 早期两处各写一份，令牌改了代码没改，调参静默失效。
//!
//! # v0.3 修复的关键缺陷
//!
//! 1. **不再把空帧渲染成不透明黑块**。旧实现对每个输出像素无条件写
//!    `alpha = 255`；当后端给出的是全 0 帧（历史上因 DDA 首帧内容未定义而
//!    常态化发生）时，胶囊就变成一块不透明黑板。现在先做空白检测，
//!    空白则清空 canvas 并上报 `blank`，由 CSS 近似底兜住，绝不出现黑块。
//! 2. **不再用字节数反推缩放系数**。旧实现 `scale = sqrt(bytes/4/(W*H))` 再
//!    `Math.round` 校验 `bytes === w*h*4`，非整数缩放必然失配并**静默丢帧**。
//!    现在帧头（16 字节）直接携带宽高与序号，无舍入歧义。
//! 3. **不再对整帧做哈希**。旧实现每帧对 ~190KB 做 FNV-1a 只为判断"是否新帧"；
//!    现在直接用帧头里的 `seq`（后端只在内容变化时自增）。
//!
//! 性能：胶囊 40×200 DIP（175% → 70×350 = 24500 像素），逐像素位移仅在新帧
//! 到达时执行。
//!
//! # v0.3.1 修复
//!
//! - 帧状态经 `reportGlassStatus` 上报 Rust，供诊断窗口读取。面板与诊断窗口
//!   是两个独立 webview，没有共享内存，只能走 IPC。
//! - **帧数据也改走 IPC（`pull_frame`）**，不再走 `glass://` 自定义协议：
//!   真机实测该协议在约 10～19 次响应后会让 WebView2 渲染进程的 JS 整体停止
//!   执行（进程存活但定时器全静默）→ `rendered` 永远为 0。详见 FIX-PLAN §4.0.2.3。
//! - `GlassFrameStatus` 改由 `bridge.ts` 唯一定义（IPC 契约优先），本模块只
//!   导入，避免两处枚举漂移。

import {
  pullFrame,
  reportGlassStatus,
  FRAME_HEADER_BYTES,
  lastFrameTransfer,
  lastFrameTransferMs,
  frameTransferAvgMs,
  frameTransferSamples,
  type GlassFrameStatus,
} from "../bridge";

export type { GlassFrameStatus };

const CAPSULE_W = 40; // DIP，与 tokens.css --capsule-w 一致
const CAPSULE_H = 200;
const CAPSULE_RADIUS = 20;

/**
 * 光学参数，**全部从 tokens.css 读取**，本文件不再硬编码。
 *
 * 为什么改成读 CSS 令牌：v0.3.1 之前这里是 `const DISP_MAX = 8` 之类，而
 * tokens.css 里同时躺着一份 `--glass-disp-max`。两份值一旦不同（实际发生过：
 * 令牌改成 9px，代码还是 8），就会出现"按文档调参毫无效果"的静默漂移 ——
 * 属于本项目反复踩的那类坑。现在令牌是唯一真源。
 *
 * 附带好处：`refreshMaps()` 会重读令牌，于是改 CSS 变量 + 调一次 refreshMaps
 * 就能实时看效果，不必重编译。
 */
interface GlassOptics {
  /** 边缘最大折射位移（DIP） */
  dispMax: number;
  /** 折射从边缘向内衰减的距离带（DIP） */
  dispBand: number;
  /** 色散最大偏移（DIP） */
  caMag: number;
  /**
   * 镜面高光的加性峰值（0 = 无高光）。
   *
   * ⚠️ 这是**加性**的、且只作用在迎光侧 1–2px。旧实现是"乘性增益"
   * （把边缘采样乘到 1.55 倍），那会把整圈抬到饱和、比背景亮出一大截，
   * 看起来像描了发光边（用户实测反馈："一圈亮白色非常奇怪"）。
   */
  specGain: number;
  /** 紧贴内侧的压暗强度（0 = 无）。制造玻璃厚度感。 */
  innerShade: number;
  /**
   * 磨砂模糊半径（DIP）。
   *
   * ⚠️ 这个令牌**曾经是死的** —— `tokens.css` 里声明了 `--glass-blur`，但这里既没读、
   * shader 也没用过，所以"磨砂"从来没被实现过（用户反馈"再加一点毛玻璃效果"时才查到）。
   * 现在它是真的：每帧把整帧模糊一次，折射从模糊后的帧取样，见 `frost`。
   */
  blur: number;
}

/** 令牌读不到时的兜底值（应与 tokens.css 保持一致，仅作最后防线）。
 *  ⚠️ 必须随 tokens 同步：曾经漂移过（这里的 specGain 还是 0.45、blur 还是 1.2），
 *  一旦 getComputedStyle 读不到令牌就会突然冒出强高光 —— 本项目栽过"死令牌/双份参数"。 */
const OPTICS_FALLBACK: GlassOptics = {
  dispMax: 8,
  dispBand: 10,
  caMag: 0.7,
  specGain: 0.12,
  innerShade: 0.1,
  blur: 0.45,
};

/**
 * 主光方向（屏幕坐标，y 向下）：左上方偏上。
 *
 * 决定镜面高光落在哪一段弧上 —— 真玻璃的倒角反光是**方向性**的，
 * 全周均匀发亮立刻变成塑料/贴纸的观感。
 */
const LIGHT_X = -0.51;
const LIGHT_Y = -0.86;

/** 从 `:root` 读一个带单位的数值令牌；缺失或非法则取兜底值。 */
function readNumberToken(name: string, fallback: number): number {
  const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const v = parseFloat(raw);
  return Number.isFinite(v) ? v : fallback;
}

/** 读取当前光学令牌。 */
function readOptics(): GlassOptics {
  return {
    dispMax: readNumberToken("--glass-disp-max", OPTICS_FALLBACK.dispMax),
    dispBand: readNumberToken("--glass-disp-band", OPTICS_FALLBACK.dispBand),
    caMag: readNumberToken("--glass-ca-mag", OPTICS_FALLBACK.caMag),
    specGain: readNumberToken("--glass-spec-gain", OPTICS_FALLBACK.specGain),
    innerShade: readNumberToken("--glass-inner-shade", OPTICS_FALLBACK.innerShade),
    blur: readNumberToken("--glass-blur", OPTICS_FALLBACK.blur),
  };
}

/** 状态上报最小间隔（ms）。诊断面板 500ms 轮询，4Hz 足够且不淹 IPC。 */
const REPORT_INTERVAL_MS = 250;

/** 帧诊断信息快照。 */
export interface GlassFrameInfo {
  status: GlassFrameStatus;
  /** 后端帧序号（0 = 尚未收到有效帧）。 */
  seq: number;
  /** 帧物理宽（0 = 未知）。 */
  frameW: number;
  /** 帧物理高。 */
  frameH: number;
  /** 帧物理宽 / 窗口 DIP 宽 = DPI 缩放系数。 */
  scale: number;
  /** 最近一次错误/异常说明。 */
  detail: string;
  /** 已成功上屏的帧数。 */
  rendered: number;
  /** 收到的空白帧数。 */
  blank: number;
  /**
   * 单帧渲染耗时的滚动平均（ms，含模糊 + 裁剪 + 逐像素折射）。
   *
   * 存在的理由：想提高采样率就必须知道"一帧花多久" —— 没有这个数，
   * 判断"能不能跑 30fps"只能靠感觉。0 表示尚未渲染过。
   */
  renderMs: number;
}

/** rounded-rectangle SDF：0 边缘，负内部，正外部（IQ 约定）。 */
function roundedRectSDF(x: number, y: number, hw: number, hh: number, r: number): number {
  const qx = Math.abs(x) - (hw - r);
  const qy = Math.abs(y) - (hh - r);
  const ox = Math.max(qx, 0);
  const oy = Math.max(qy, 0);
  return Math.min(Math.max(qx, qy), 0) + Math.hypot(ox, oy) - r;
}

/** smoothstep（标准方向，edge0 < edge1）。 */
function smoothStep(a: number, b: number, t: number): number {
  t = Math.max(0, Math.min(1, (t - a) / (b - a)));
  return t * t * (3 - 2 * t);
}

/**
 * 判断帧是否全黑（四通道全 0）。
 *
 * 用途：DDA 在会话建立初期可能给出内容未定义的帧。后端已按
 * `LastPresentTime != 0` 过滤，这里作为最后一道防线 —— 一旦判定为空，
 * 宁可不上屏（保持透明 + CSS 近似底），也绝不渲染成黑板。
 *
 * 用固定步长抽样（覆盖整幅图）而非全量扫描，避免每帧 19 万次比较。
 * 副作用：桌面该区域本来就是纯黑时会误判为空 —— 视觉上透明与纯黑在
 * 深色背景下几乎无差别，可接受。
 */
function isBlank(d: Uint8ClampedArray): boolean {
  const step = Math.max(4, Math.floor(d.length / 4096 / 4) * 4);
  for (let i = 0; i < d.length; i += step) {
    if ((d[i] ?? 0) !== 0 || (d[i + 1] ?? 0) !== 0 || (d[i + 2] ?? 0) !== 0) {
      return false;
    }
  }
  return true;
}

/** 成功上屏时的诊断串；纵横缩放不一致时追加提示（不升级为错误）。
 *  末尾附上帧的**传输形态** —— `renderMs` 只含渲染，不含传输，
 *  只看它判断不出"是不是又退回了 JSON 数字数组那条慢路径"。 */
function okDetail(fw: number, fh: number, scale: number, scaleY: number, skewed: boolean): string {
  const base = `${fw}×${fh} @${scale.toFixed(2)}x`;
  const s = skewed ? `${base}（纵横缩放不一致 ${scaleY.toFixed(2)}x）` : base;
  // 真实帧的传输滚动平均才是"卡不卡"的判据；没有样本时明确写"未量到"，
  // 不许用一个漂亮的数字糊过去。
  const avg =
    frameTransferSamples > 0
      ? `真实帧均 ${frameTransferAvgMs.toFixed(1)}ms/${frameTransferSamples}帧`
      : "真实帧传输 未量到";
  return `${s}｜传输 ${lastFrameTransfer} 末次 ${lastFrameTransferMs.toFixed(1)}ms｜${avg}`;
}

/**
 * 对胶囊帧做逐像素折射 + 色散 + 边缘增益，输出到 dstCanvas。
 *
 * `scale` = 帧物理像素 / CSS px；`optics` 来自 tokens.css（见 readOptics）。
 *
 * 光学三步（每像素）：
 *   1. **折射**：按到边缘的距离算 strength，向中心推采样点 → 边缘透镜/放大
 *   2. **色散**：R / B 通道沿径向做相反微小偏移 → 边缘彩虹（真实玻璃的色散）
 *   3. **边缘增益**：按 `strength²` 给采样值乘系数 → 迎光边缘汇聚变亮，
 *      直到顶到 255 饱和。这一步就是"HDR 感高光"的本体：它不是叠一层白色
 *      蒙版（那会把颜色洗掉），而是把**真实背景像素**抬到过曝，所以高光里
 *      仍然带着环境色。
 */
/**
 * 预计算的位移映射。
 *
 * 折射 / 色散的**几何部分只依赖像素位置与光学令牌，与帧内容无关** ——
 * 所以算一次即可逐帧复用。每像素存三通道的**源数组绝对下标**（已含行宽与
 * 通道偏移），逐帧循环因此退化为纯查表 + 乘加，不再重算 `roundedRectSDF` /
 * `Math.hypot` / `smoothStep`。旧实现在 24500 像素上每帧重跑一遍解析几何，
 * CPU 全耗在这里 —— 这是「一动就卡」的第二大主因。
 */
interface DispMap {
  w: number;
  h: number;
  /** 每像素 R 通道源下标。 */
  idxR: Int32Array;
  /** 每像素 G 通道源下标。 */
  idxG: Int32Array;
  /** 每像素 B 通道源下标。 */
  idxB: Int32Array;
  /** 每像素镜面高光加性增量（0–255）。 */
  spec: Float32Array;
  /** 每像素内侧压暗乘数（≤1）。 */
  shade: Float32Array;
}

/** 把浮点像素坐标钳到 `[0, n-1]` 并取整。 */
function clampIdx(v: number, n: number): number {
  const r = Math.round(v);
  return r < 0 ? 0 : r > n - 1 ? n - 1 : r;
}

/**
 * 构建位移映射。`w/h` = 帧像素尺寸，`scale` = 帧物理像素 / CSS px。
 * 只在帧尺寸或光学令牌变化时调用（见 `dispKey`），静态下零成本。
 */
function buildDispMap(w: number, h: number, scale: number, optics: GlassOptics): DispMap {
  const idxR = new Int32Array(w * h);
  const idxG = new Int32Array(w * h);
  const idxB = new Int32Array(w * h);
  const spec = new Float32Array(w * h);
  const shade = new Float32Array(w * h);
  const halfW = CAPSULE_W / 2;
  const halfH = CAPSULE_H / 2;
  for (let py = 0; py < h; py++) {
    for (let px = 0; px < w; px++) {
      const i = py * w + px;
      const cssX = px / scale - halfW;
      const cssY = py / scale - halfH;
      const d = roundedRectSDF(cssX, cssY, halfW, halfH, CAPSULE_RADIUS);
      const dist = -d;
      const strength = 1 - smoothStep(0, optics.dispBand, dist);
      const dx = -cssX;
      const dy = -cssY;
      const len = Math.hypot(dx, dy) || 1e-6;
      const nx = dx / len;
      const ny = dy / len;
      const offX = nx * strength * optics.dispMax * scale;
      const offY = ny * strength * optics.dispMax * scale;
      const caX = nx * strength * optics.caMag * scale;
      const caY = ny * strength * optics.caMag * scale;
      // 边缘处理：`strength^5` 把镜面高光收到最外 1–2px；`lambert = n·L`
      // 只让迎光那一段弧亮（全周均匀发亮就是塑料/贴纸的观感）。
      const s2 = strength * strength;
      const rim = s2 * s2 * strength;
      const lambert = nx * LIGHT_X + ny * LIGHT_Y;
      spec[i] = rim * (lambert > 0 ? lambert : 0) * optics.specGain * 255;
      shade[i] = 1 - s2 * optics.innerShade;
      // 源下标（钳到帧内，已含 `*4` 与通道偏移）—— 逐帧不再 round / 钳位。
      const gx = clampIdx(px + offX, w);
      const gy = clampIdx(py + offY, h);
      const rx = clampIdx(px + offX + caX, w);
      const ry = clampIdx(py + offY + caY, h);
      const bx = clampIdx(px + offX - caX, w);
      const by = clampIdx(py + offY - caY, h);
      idxG[i] = (gy * w + gx) * 4 + 1;
      idxR[i] = (ry * w + rx) * 4;
      idxB[i] = (by * w + bx) * 4 + 2;
    }
  }
  return { w, h, idxR, idxG, idxB, spec, shade };
}

/**
 * 按预计算映射把 `src` 折射到 `dst`（纯查表 + 乘加，无解析几何）。
 *
 * 光学：
 *   1. **折射** —— 源下标已按「向中心偏移」预存 → 边缘透镜 / 放大
 *   2. **色散** —— R / B 的下标各自带相反偏移 → 边缘彩虹
 *   3. **边缘增益** —— `spec` 加性抬亮迎光侧最外 1–2px（HDR 感高光本体），
 *      `shade` 把紧贴内侧压暗一点（玻璃厚度）
 */
function displace(src: ImageData, dst: HTMLCanvasElement, map: DispMap): void {
  const s = src.data;
  const out = new ImageData(map.w, map.h);
  const o = out.data;
  const { idxR, idxG, idxB, spec, shade } = map;
  const n = map.w * map.h;
  for (let i = 0; i < n; i++) {
    const oi = i * 4;
    const sh = shade[i] ?? 1;
    const sp = spec[i] ?? 0;
    const r = (s[idxR[i] ?? 0] ?? 0) * sh + sp;
    const g = (s[idxG[i] ?? 0] ?? 0) * sh + sp;
    const b = (s[idxB[i] ?? 0] ?? 0) * sh + sp;
    o[oi] = r > 255 ? 255 : r; // R
    o[oi + 1] = g > 255 ? 255 : g; // G
    o[oi + 2] = b > 255 ? 255 : b; // B
    o[oi + 3] = 255;
  }
  dst.width = map.w;
  dst.height = map.h;
  dst.getContext("2d")!.putImageData(out, 0, 0);
}

/** 清空画布到透明（帧无效时的降级：绝不留下上一帧残影或黑板）。 */
function clearCanvas(dst: HTMLCanvasElement): void {
  if (dst.width !== 0 || dst.height !== 0) {
    dst.width = 0;
    dst.height = 0;
  }
}

export interface GlassElements {
  capsule: HTMLElement;
  backdrop: HTMLCanvasElement;
  fill: HTMLElement;
  pressLight: HTMLElement;
  hdrDot: HTMLElement;
}

export interface GlassApi {
  setValue(pct: number, animate: boolean, distance?: number): void;
  setHdr(on: boolean): void;
  showPress(x: number, y: number): void;
  hidePress(): void;
  /** 启动取帧链（幂等）。挂载时调用一次，之后常驻。 */
  startFrameLoop(): void;
  /**
   * 相位驱动：可见时 ≈30fps 取帧，不可见时降到 1Hz 保活。
   *
   * 语义说明：**不停循环**。隐藏态的低频保活代价可忽略，换来的是
   * "事件丢失也不会永久停摆"与"唤出即时出画"。
   */
  setVisible(on: boolean): void;
  /**
   * 冻结/解冻背衬。**调亮度期间必须冻结** —— 调的是显示器的 SDR 白电平，
   * 会把屏幕上的 SDR 内容整体提亮，被捕捉的像素与叠在其上的高光、液面遮罩
   * 会一起变亮，内容像被白色糊住。详见 `hold` 的说明。
   */
  setHold(on: boolean): void;
  /** 彻底停止（仅销毁时用）。 */
  stopFrameLoop(): void;
  refreshMaps(): void;
  /** 订阅帧摄取状态变化（诊断 HUD 用）。 */
  onStatus(cb: (info: GlassFrameInfo) => void): void;
  /** 取当前帧诊断信息快照。 */
  getFrameInfo(): GlassFrameInfo;
}

/** 装配 Liquid Glass 光学管线（纯 2D），返回控制 API。 */
export function mountGlass(els: GlassElements): GlassApi {
  const { backdrop } = els;
  const offscreen = document.createElement("canvas");
  const offCtx = offscreen.getContext("2d", { willReadFrequently: true })!;
  /**
   * 磨砂层：整帧模糊一次，折射从这层取样。
   *
   * 为什么**不**在 shader 里对每个输出像素做多点平均：胶囊 70×350 = 24500 像素，
   * 每像素多采 4 次就是 +10 万次取样/帧，30fps 下 JS 扛不住。
   * 先整帧模糊一次是 **O(1)/帧**，而 Canvas2D 的 `filter: blur()` 走合成器，代价可忽略。
   *
   * ⚠️ **必须** `willReadFrequently`：本层每帧被 `getImageData` 回读（供折射取样）。
   * 不设的话 Chromium 会把画布放在 GPU，每次回读都触发一次昂贵的 GPU→CPU 同步 ——
   * 这是「一有动态内容就卡」的成因之一。
   */
  const frost = document.createElement("canvas");
  const frostCtx = frost.getContext("2d", { willReadFrequently: true })!;

  let lastSeq = 0;
  /** 光学参数（来自 tokens.css）。`refreshMaps()` 会重读，便于实时调参。 */
  let optics = readOptics();
  /** 预计算的位移映射（帧尺寸 / 令牌不变时复用）。 */
  let dispMap: DispMap | null = null;
  /** 映射对应的键（尺寸 + 令牌），用于判断是否需要重建。 */
  let dispKey = "";
  /**
   * 上一次真正处理过的帧的处置结果。
   *
   * 存在的唯一理由：防止"状态纠正"撒谎。没有它的话，当 `seq` 未变（内容没变）
   * 而状态是 `blank` 时，下面那条纠正分支会把它改成 `ok` —— 画布明明是空的，
   * 诊断面却报"链路正常"，正是本项目前几轮空转时"看不出问题在哪"的成因。
   */
  let lastOutcome: "none" | "rendered" | "blank" = "none";
  /** 连续空白帧计数（`info.blank` 是累计值，措辞上不能混用）。 */
  let blankStreak = 0;
  let stopped = false;
  let running = false;
  let timer = 0;
  let statusCb: ((info: GlassFrameInfo) => void) | null = null;
  /**
   * 当前是否处于"可见"相位。
   *
   * ⚠️ 语义在 v0.3.1 变了：**不再用来停掉帧循环**，只用来切换轮询频率。
   *
   * 旧设计是"不可见 → `stopFrameLoop()` 彻底停采样"，省电但有两个致命后果：
   *   1. 帧循环一旦停就不会自己活过来 —— 只要漏掉一次"回到可见态"的事件
   *      （事件在 webview 订阅之前发出就会永久丢失），它就永久停摆。真机实测：
   *      采集端 DDA 取得 245 帧、拷贝全成功，而前端 `rendered = 0`，
   *      胶囊完全没有背景内容，在深色桌面上近乎不可见。
   *   2. 即使事件没丢，唤出瞬间也必然没内容（要等第一次取帧回来才有画面）。
   *
   * 现在改为：循环**常驻**，不可见时降到 `IDLE_DELAY_MS` 低频轮询。
   * 代价是隐藏态每秒一次 190KB 的帧拷贝（可忽略），换来的是"绝不停摆"
   * 与"唤出即时出画"。
   */
  let visible = true;
  /** 已发起的取帧请求次数（含失败的）。 */
  let polls = 0;
  /** 是否有请求在飞（防止并发链）。 */
  let inFlight = false;
  /** 最近一次实际发起取帧的时刻（看门狗判据）。 */
  let lastPollAt = 0;
  /**
   * 是否冻结背衬（调亮度期间）。
   *
   * # 为什么必须冻结
   *
   * 调亮度写的是显示器的 **SDR 白电平**（`SetSdrWhiteLevel`），它会把屏幕上的 SDR
   * 内容**整体提亮**。而被捕捉的正是这块屏幕 —— 于是调亮的同时，玻璃里的背景像素、
   * 叠在其上的边缘高光、液面遮罩**一起变亮**，内容像被白色糊住。
   * 用户实测原话："调亮的时候它的边缘高光和遮罩也会跟着变亮导致内容被白色糊上"。
   *
   * 冻结后：调整期间玻璃保持稳定（也让拖拽过程中游标/滑条不闪进玻璃），
   * 松手后自然跟上新的实际亮度 —— 玻璃显示真实桌面，这一点不该变。
   */
  let hold = false;

  const info: GlassFrameInfo = {
    status: "idle",
    seq: 0,
    frameW: 0,
    frameH: 0,
    scale: 0,
    detail: "",
    rendered: 0,
    blank: 0,
    renderMs: 0,
  };

  let lastReportAt = 0;

  /**
   * 更新状态并广播。
   *
   * 两条出口：
   * - `statusCb`：同 webview 内的订阅者（当前无，留给后续面板内 HUD）；
   * - `reportGlassStatus`：跨 webview 上报 Rust，供诊断窗口读取。
   *   节流到 `REPORT_INTERVAL_MS`；`ok`↔异常之间的**变化**永不节流，
   *   否则关键跳变会被 250ms 窗口吞掉。
   */
  function setStatus(status: GlassFrameStatus, detail = ""): void {
    const changed = info.status !== status;
    info.status = status;
    info.detail = detail;
    statusCb?.(info);

    const now = Date.now();
    if (!changed && now - lastReportAt < REPORT_INTERVAL_MS) return;
    lastReportAt = now;
    // 上报路径**绝不允许抛错**：`invoke` 在 `__TAURI_INTERNALS__` 缺失等情况下是
    // **同步抛出**而不是返回 rejected promise。同步抛错会一路穿过 poll() 的 catch、
    // 再穿出 loop()，使 `schedule()` 永不执行 —— 整条取帧链静默死掉。
    // 真机实测过这种"跑了十几次就再也不动"的表现。
    try {
      void reportGlassStatus({
        status: info.status,
        seq: info.seq,
        frameW: info.frameW,
        frameH: info.frameH,
        scale: info.scale,
        detail: info.detail,
        rendered: info.rendered,
        blank: info.blank,
        renderMs: Math.round(info.renderMs * 10) / 10,
        polls,
        // 前端自己看到的相位。与 Rust 状态机对照即可发现"事件丢失导致前端卡住"。
        phase: els.capsule.dataset.phase ?? "",
        updatedAt: 0, // 服务端打戳，此处占位
      }).catch(() => {
        /* 诊断上报失败不影响渲染 */
      });
    } catch {
      /* 同步抛错同样吞掉：诊断是旁路，不是主链路 */
    }
  }

  /** 取帧节奏常量（ms）。节奏由 `loop()` 依据 `visible` 决定。 */
  /**
   * 可见态取帧间隔（ms）＝ 16ms ≈ **60fps**。
   *
   * ⚠️ 必须与后端采集节奏（`CaptureConfig::interval_ms`，同样 16ms）对齐 ——
   * **后端那一行才是玻璃跟随桌面的上限**：后端不产出新的 `seq`，前端问得再勤也只会
   * 拿到"没有新帧"。两处不一致只会浪费调用或降低流畅度，不影响正确性。
   *
   * 60fps 的代价：单帧 119×399×4 ≈ 190KB，×60 即约 11.4MB/s 的拷贝，外加 base64
   * 编解码与逐像素折射。**单帧耗时由 `renderMs` 上报 —— 那是判断还能不能再提的唯一依据。**
   */
  const ACTIVE_DELAY_MS = 16;
  /** 隐藏态低频保活 —— 只保活，不追求实时。 */
  const IDLE_DELAY_MS = 1000;

  // ⚠️ 这里**刻意没有"静止退避"**。
  //
  // 曾经加过一版：连续空帧就把间隔翻倍到 1s。加它的依据是"空 `pull_frame` 会让宿主
  // 进程涨约 112 字节/次"，但那个归因被 30 分钟长跑推翻了 —— 实测内存是**锯齿式**
  // 增长（有回落、还有一次 +5.4MB 跳变），不是线性泄漏，空调用不是元凶。
  // 而退避的代价是真实的：静止后桌面一动，最多要等一个退避周期才发现（用户实测反馈
  // "非常卡顿"）。**消除一个不存在的成本、却引入可感的延迟，是笔坏买卖。**
  // 要再加回来，先拿数据证明空调用确实有害。
  /**
   * 单次取帧的硬超时（ms）。
   *
   * ⚠️ 这不是优化，是**必需**：真机实测出现过"取帧调用一去不回"——一旦它永不
   * settle，`loop()` 的 `inFlight` 就永久为真，整条取帧链自锁，之后再也不取帧
   * （表现与"没启动"完全一样，但病因相反）。有超时兜底后，最坏情况退化为一次
   * 抖动，循环自动恢复。
   */
  const IPC_TIMEOUT_MS = 2000;

  /** 给任意 promise 加超时，避免它永久挂住调度链。 */
  function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const t = window.setTimeout(() => reject(new Error(`取帧超时（${ms}ms）`)), ms);
      p.then(
        (v) => {
          window.clearTimeout(t);
          resolve(v);
        },
        (e: unknown) => {
          window.clearTimeout(t);
          reject(e instanceof Error ? e : new Error(String(e)));
        },
      );
    });
  }

  async function poll(): Promise<void> {
    if (stopped) return;
    lastPollAt = Date.now();
    polls += 1;
    try {
      // 走 IPC（`pull_frame`），**不再走 `glass://` 自定义协议**。
      //
      // 真机实测后者在约 10～19 次响应之后会让本渲染进程的 JS 整体停止执行：
      // 进程存活、未崩溃，但定时器 / IPC / 请求全部静默 —— 于是 `rendered`
      // 永远是 0，胶囊没有任何背景内容。与响应体积无关（16B～190KB 均复现），
      // 只与响应次数有关。详见 FIX-PLAN §4.0.2.3。
      //
      // 后端只在"存在比 info.seq 更新的帧"时才回数据、否则回空 `ArrayBuffer`，
      // 于是平均带宽与**桌面变化频率**同阶，静态桌面上几乎为零。
      const buf = await withTimeout(pullFrame(info.seq), IPC_TIMEOUT_MS);
      if (buf.byteLength < FRAME_HEADER_BYTES) {
        // 没有新帧。**这里必须照常调 `setStatus`** —— 报告是诊断面板判断
        // "面板是否还活着"的唯一依据，而"一切正常、只是桌面没变"恰恰是最常见的状态。
        //
        // 早期版本在这条分支里只在状态需要**纠正**时才调 setStatus，后果是：
        // 跑了 4 分钟一切正常，诊断面板却报"已 243 秒未上报"（实测）。
        // 误报警比不报还糟 —— 它会把用户和排查者引向"前端卡死了"的错误方向。
        if (lastOutcome === "rendered") {
          setStatus("ok", okDetail(info.frameW, info.frameH, info.scale, info.scale, false));
        } else if (info.status !== "no-frame") {
          setStatus("no-frame", "后端暂无新帧");
        }
        return;
      }
      // 帧头（小端）：`seq: u64 | w: u32 | h: u32`，其后紧跟 RGBA 像素。
      const head = new DataView(buf);
      const seq = Number(head.getBigUint64(0, true));
      const fw = head.getUint32(8, true);
      const fh = head.getUint32(12, true);
      // 零拷贝取像素视图（跳过 16 字节帧头）。
      const pixels = new Uint8ClampedArray(buf, FRAME_HEADER_BYTES);
      if (fw === 0 || fh === 0) {
        setStatus("bad-payload", `帧尺寸为 0：${fw}×${fh}`);
        return;
      }
      if (pixels.length !== fw * fh * 4) {
        setStatus("bad-payload", `像素字节数不符：${pixels.length} ≠ ${fw * fh * 4}`);
        return;
      }
      info.seq = seq;
      info.frameW = fw;
      info.frameH = fh;
      // 缩放系数**用高度反推**（`fh / CAPSULE_H`），不用宽度。
      //
      // 为什么：真机实测面板窗口宽度曾被 Windows 强制撑宽一倍（请求 68 DIP、
      // 实得 236 物理像素），高度才是可靠的那个轴。帧现在是**胶囊区域**
      // （40×200 DIP → 70×350 物理 @1.75x），故用 `fh / 200` 反推。
      const scaleX = fw / CAPSULE_W;
      info.scale = fh / CAPSULE_H;
      // 两个轴本应等比（Per-Monitor DPI 不会只缩放一个轴）。不等说明窗口几何
      // 或裁剪矩形有问题 —— 只提示，不当作致命错误。
      const scaleY = info.scale;
      const skewed = Math.abs(scaleX - scaleY) / scaleY > 0.02;
      // seq 由后端在内容变化时自增，是"是否新帧"的权威判据，无需对整帧做哈希。
      if (seq !== lastSeq) {
        lastSeq = seq;
        const t0 = performance.now();
        offscreen.width = fw;
        offscreen.height = fh;
        offCtx.putImageData(new ImageData(pixels, fw, fh), 0, 0);

        // ---- 磨砂：整帧模糊一次（`--glass-blur` 是 DIP，按 scale 换成设备像素）----
        const blurPx = optics.blur * info.scale;
        if (frost.width !== fw || frost.height !== fh) {
          frost.width = fw;
          frost.height = fh;
        }
        frostCtx.clearRect(0, 0, fw, fh);
        frostCtx.filter = blurPx > 0.05 ? `blur(${blurPx.toFixed(2)}px)` : "none";
        frostCtx.drawImage(offscreen, 0, 0);
        frostCtx.filter = "none";

        const scale = info.scale;
        // 帧现在就是**胶囊区域**（见 `edge.rs::update_capture`），无需裁剪 ——
        // 直接读磨砂层整幅做折射。省掉的这步就是旧版每帧的固定开销之一。
        const bw = fw;
        const bh = fh;
        const frame = frostCtx.getImageData(0, 0, bw, bh);
        if (isBlank(frame.data)) {
          // 空帧：清空画布并上报，由 CSS 近似底兜住。绝不渲染黑板。
          clearCanvas(backdrop);
          info.blank += 1;
          blankStreak += 1;
          lastOutcome = "blank";
          setStatus("blank", `连续 ${blankStreak} 张空白帧（累计 ${info.blank}）`);
        } else {
          // 位移映射只在帧尺寸或光学令牌变化时重建（静态下零成本）。
          const key = `${bw}x${bh}@${scale.toFixed(3)}|${optics.dispMax},${optics.dispBand},${optics.caMag},${optics.specGain},${optics.innerShade}`;
          if (!dispMap || dispKey !== key) {
            dispMap = buildDispMap(bw, bh, scale, optics);
            dispKey = key;
          }
          displace(frame, backdrop, dispMap);
          info.rendered += 1;
          blankStreak = 0;
          lastOutcome = "rendered";
          setStatus("ok", okDetail(fw, fh, scale, scaleY, skewed));
        }
        // 单帧渲染耗时（含模糊、裁剪、逐像素折射）。**滚动平均**，避免抖动。
        // 这是"采样率能不能再往上提"的唯一判据 —— 想跑 30fps 就得知道一帧花多久。
        const cost = performance.now() - t0;
        info.renderMs = info.renderMs === 0 ? cost : info.renderMs * 0.85 + cost * 0.15;
      } else if (lastOutcome === "rendered" && info.status !== "ok") {
        // 内容未变（同一 seq），而上一帧**确实上屏过**，只是状态被别的事件
        // 改写过 → 纠正回 ok。必须判 `lastOutcome`：上一帧若是空白，这里
        // 纠正成 ok 就是撒谎 —— canvas 是空的，屏幕上看不到任何东西。
        setStatus("ok", okDetail(fw, fh, info.scale, scaleY, skewed));
      }
    } catch (e) {
      // 超时也走这里 —— 状态标红，但**循环照常继续**，下一次轮询自动重试。
      // 这正是"调用挂死也不停摆"的保证。
      setStatus("error", e instanceof Error ? e.message : String(e));
    }
  }

  /**
   * 调度下一次取帧。**总是先清掉上一个定时器**，保证任何时刻只有一条待执行链。
   */
  function schedule(delayMs: number): void {
    if (timer) window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      timer = 0;
      void loop();
    }, delayMs);
  }

  /**
   * 单条轮询链。用 `inFlight` 防止并发（`setVisible(true)` 会插入一次立即调度，
   * 若此时已有请求在飞，就让它落地后自己续上，不再另起一条链）。
   *
   * ⚠️ `poll()` 之外**再包一层 catch**：任何逃出 poll 内部处理的异常都不能让
   * `schedule()` 被跳过 —— 跳过一次就是永久停摆。这是本项目最贵的一课。
   */
  async function loop(): Promise<void> {
    if (stopped || inFlight) return;
    inFlight = true;
    try {
      await poll();
    } catch (e) {
      try {
        setStatus("error", e instanceof Error ? e.message : String(e));
      } catch {
        /* 连上报都失败也不能影响续链 */
      }
    } finally {
      inFlight = false;
    }
    if (!stopped) {
      schedule(visible ? ACTIVE_DELAY_MS : IDLE_DELAY_MS);
    }
  }

  // ---- 看门狗 ----
  //
  // 兜底目的：**取帧链无论因何而死，都要能自己活过来**。
  // 真机实测过两种死法：① 请求一去不回、`inFlight` 永久为真；
  // ② 某个异常穿出 `loop()` 使 `schedule()` 被跳过。
  // 两者的外部表现完全相同（"跑了十几次就再也不动"），而且都不会自愈。
  //
  // 判据用"最近一次实际取帧的时刻"而不是标志位：标志位本身正是可能被卡住的东西。
  const STALL_MS = 4000;
  const WATCHDOG_MS = 1500;
  window.setInterval(() => {
    if (stopped) return;
    if (Date.now() - lastPollAt < STALL_MS) return;
    // 强制重建调度链：清挂起的定时器、解除 inFlight 自锁、立刻补一次。
    if (timer) {
      window.clearTimeout(timer);
      timer = 0;
    }
    inFlight = false;
    schedule(0);
  }, WATCHDOG_MS);

  // ---- 装配心跳 ----
  //
  // 必须在**取帧循环之外**单独发一次上报，否则诊断面上分不清下面三种情况
  // （它们的表现都是"没有画面"）：
  //   ① 页面 JS 根本没跑 / IPC 不通   → 完全没有上报（updatedAt === 0）
  //   ② 模块跑了但循环一次都没取帧     → 有上报但 polls === 0
  //   ③ 循环在跑但帧取不到            → polls 持续增长而 rendered === 0
  // 少了这次心跳，②与①在诊断面上完全一样 —— 而它们的修法毫无关系。
  setStatus("idle", "光学管线已装配，等待首次取帧");

  const api: GlassApi = {
    setValue(pct: number, animate: boolean, distance = 1): void {
      const p = Math.max(0, Math.min(100, pct));
      if (animate) {
        const dur = Math.round(Math.max(160, Math.min(300, 160 + distance * 0.7)));
        els.fill.style.transition = `height ${dur}ms cubic-bezier(0.32, 0.72, 0.24, 1)`;
      } else {
        els.fill.style.transition = "none";
      }
      els.fill.style.height = `${p}%`;
    },
    setHdr(on: boolean): void {
      els.capsule.dataset.hdr = on ? "on" : "off";
    },
    showPress(x: number, y: number): void {
      els.pressLight.style.left = `${x}px`;
      els.pressLight.style.top = `${y}px`;
      els.pressLight.classList.add("on");
    },
    hidePress(): void {
      els.pressLight.classList.remove("on");
    },
    startFrameLoop(): void {
      if (running) return;
      running = true;
      stopped = false;
      void loop();
    },
    /**
     * 相位驱动：切换取帧频率（可见 ≈30fps / 隐藏 1Hz 保活）。
     *
     * **注意：本方法不再停掉帧循环**（v0.3.1 起）。旧实现里"不可见 → 停采样"
     * 是导致胶囊无背景内容的根因之一：漏一次事件就永久停摆，且唤出瞬间必然空屏。
     */
    setVisible(on: boolean): void {
      if (visible === on) return;
      visible = on;
      // 切到可见态时立刻补一次取帧，避免"唤出后要等一个周期才出画面"。
      if (on) schedule(0);
    },
    setHold(on: boolean): void {
      if (hold === on) return;
      hold = on;
      // 解冻立刻补一帧，别让用户在松手后看到一个周期内的旧画面。
      if (!on) schedule(0);
    },
    stopFrameLoop(): void {
      stopped = true;
      running = false;
      if (timer) {
        window.clearTimeout(timer);
        timer = 0;
      }
      // 不重置 lastSeq：同一个 seq 的帧内容仍然有效，重新进入时可直接复用，
      // 避免"唤出瞬间无帧 → 透明"的闪烁。
    },
    refreshMaps(): void {
      // 纯 2D 下无需重建贴图，但重读光学令牌 —— 这样改完 tokens.css 的 CSS 变量
      // 再调一次本方法即可实时看到效果，不必重新构建（调参闭环）。
      optics = readOptics();
    },
    onStatus(cb: (i: GlassFrameInfo) => void): void {
      statusCb = cb;
      cb(info);
    },
    getFrameInfo(): GlassFrameInfo {
      return { ...info };
    },
  };

  // ---- 外观验收出口 ----
  //
  // 为什么需要它：本胶囊的外观**无法在真机上目视验收** ——
  //   ① 面板窗口带 `WDA_EXCLUDEFROMCAPTURE` 时对所有截屏 API 隐身；
  //   ② 即便关掉，WebView2 走 DirectComposition，`PrintWindow` 也抓不到内容；
  //   ③ 帧循环由相位事件驱动，在普通浏览器里永远不启动（没有 Tauri 事件）。
  // 结果就是"只能改常量 → 打包 → 让用户看"，正是本项目空转多轮的根因。
  //
  // 因此留一个显式出口：带 `?glassDebug` 访问时把光学 API 挂到 `window.__glass`，
  // 于是可以在普通浏览器里喂合成帧、驱动帧循环、放大截图来验收光学效果
  // （做法见 tools 里的预览服务说明）。**不带该参数时行为完全不变**，不进生产路径。
  if (new URLSearchParams(window.location.search).has("glassDebug")) {
    (window as unknown as Record<string, unknown>).__glass = api;
  }

  return api;
}
