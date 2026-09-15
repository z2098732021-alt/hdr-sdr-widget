//! diag.ts —— 诊断面板（v0.3）
//!
//! # 为什么要有这一页
//!
//! v0.3 之前，"玻璃不亮"这件事没有任何观测手段：后端静默吞错，前端静默丢帧，
//! 唯一的现象是"胶囊是黑的"。于是每次排查都退化成改一版、编一版、看一眼。
//!
//! 本页给出三个**互相独立**的信号，让故障自己暴露在哪一段：
//!
//! | 信号 | 来源 | 回答的问题 |
//! |------|------|-----------|
//! | 采集端统计 | Rust `CaptureStatsSnapshot` | DDA 会话建了吗？拷贝成功了吗？格式对吗？ |
//! | 渲染端状态 | 面板窗口 `report_glass_status` | 前端解码了吗？几帧被判空白？ |
//! | 原始帧预览 | 本页直接 `fetch glass://frame` | 后端此刻到底在发什么像素？ |
//!
//! 三者一致 → 链路真的通了；采集说有帧、渲染说空白 → 问题在帧头协议；
//! 采集说拷贝失败、预览也是空 → 问题在 D3D。不用再二分。

import { getVersion } from "@tauri-apps/api/app";

import { pullFrame,
  FRAME_HEADER_BYTES,
  closeDiagWindow,
  getCaptureStats,
  getGlassStatus,
  type CaptureStatsSnapshot,
  type GlassStatusSnapshot,
} from "../bridge";


/** 统计轮询间隔（ms）。 */
const POLL_MS = 500;
/**
 * 原始帧拉取间隔（ms）。
 *
 * 比统计慢一档：一帧 3840×2160 RGBA 是 33MB，即便本机环回也不该 2Hz 地拖。
 * 700ms 足够看出"是否在变"。
 */
const FRAME_MS = 700;
/** 预览画布的最大边长（像素）。超出按整数步长抽样，不做插值。 */
const PREVIEW_MAX_W = 240;
const PREVIEW_MAX_H = 150;

// ---------------------------------------------------------------------------
// 名称表：把裸数字翻译成人能读的东西
// ---------------------------------------------------------------------------

/** DXGI_FORMAT 关键取值 → 名称。只列与捕获链路相关的。 */
const FORMAT_NAMES: Record<number, string> = {
  0: "DXGI_FORMAT_UNKNOWN",
  2: "R32G32B32A32_FLOAT",
  6: "R32G32B32_FLOAT",
  10: "R16G16B16A16_FLOAT",
  11: "R16G16B16A16_UNORM",
  28: "R8G8B8A8_UNORM",
  29: "R8G8B8A8_UNORM_SRGB",
  87: "B8G8R8A8_UNORM",
  88: "B8G8R8A8_UNORM_SRGB",
};

/** 常见 HRESULT → 名称 + 人话。 */
const HRESULT_NAMES: Record<number, string> = {
  0x00000000: "S_OK",
  0x80004005: "E_FAIL",
  0x80070005: "E_ACCESSDENIED",
  0x8007000e: "E_OUTOFMEMORY",
  0x80070057: "E_INVALIDARG",
  0x887a0001: "DXGI_ERROR_INVALID_CALL",
  0x887a0002: "DXGI_ERROR_NOT_FOUND",
  0x887a0004: "DXGI_ERROR_UNSUPPORTED",
  0x887a0005: "DXGI_ERROR_DEVICE_REMOVED",
  0x887a0006: "DXGI_ERROR_DEVICE_HUNG",
  0x887a0020: "DXGI_ERROR_DRIVER_INTERNAL_ERROR",
  0x887a0022: "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE",
  0x887a0027: "DXGI_ERROR_WAIT_TIMEOUT",
  0x887a0028: "DXGI_ERROR_SESSION_DISCONNECTED",
};

/** 渲染端状态 → 中文短标签。 */
const GLASS_STATUS_LABEL: Record<string, string> = {
  idle: "空闲（未开始取样）",
  ok: "正常上屏",
  blank: "空白帧（已丢弃）",
  "no-frame": "后端无帧",
  "bad-payload": "帧载荷非法",
  error: "网络/解码异常",
};

/** 把 i32 形态的 HRESULT 还原成 8 位十六进制。 */
function hr(i: number): string {
  return `0x${(i >>> 0).toString(16).toUpperCase().padStart(8, "0")}`;
}

/** HRESULT → "0x887A0022 DXGI_ERROR_NOT_CURRENTLY_AVAILABLE"。 */
function hrName(i: number): string {
  if (i === 0) return "无";
  const known = HRESULT_NAMES[i >>> 0];
  return known ? `${hr(i)} ${known}` : hr(i);
}

/** DXGI_FORMAT → "87 B8G8R8A8_UNORM"。 */
function fmtName(f: number): string {
  if (f < 0) return "未知";
  const known = FORMAT_NAMES[f];
  return known ? `${f} ${known}` : String(f);
}

// ---------------------------------------------------------------------------
// 独立探针：本页自己拉一帧
// ---------------------------------------------------------------------------

interface FrameProbe {
  /** 是否成功拿到 200 响应。 */
  ok: boolean;
  httpStatus: number;
  /** 响应体总字节数（含帧头）。 */
  bytes: number;
  /** 像素区字节数。 */
  pixels: number;
  magicOk: boolean;
  headerSeq: number;
  headerW: number;
  headerH: number;
  /** 抽样统计的非零字节数。 */
  nonzero: number;
  /** 抽样总字节数（用于算比例）。 */
  sampled: number;
  /** 判定：整帧是否全零。 */
  blank: boolean;
  /** 失败原因。 */
  detail: string;
}

const EMPTY_PROBE: FrameProbe = {
  ok: false,
  httpStatus: 0,
  bytes: 0,
  pixels: 0,
  magicOk: false,
  headerSeq: 0,
  headerW: 0,
  headerH: 0,
  nonzero: 0,
  sampled: 0,
  blank: false,
  detail: "尚未取样",
};

/**
 * 抽样统计非零字节。
 *
 * 步长固定 4 的倍数，保证落点在同一个通道上（否则只测到 B 通道会漏判）。
 * 目标是"是不是整块全零"，不是精确占比 —— 全零帧在 DDA 里是真实现象，
 * 只要有一个非零样本就能否掉"全零"这个假设。
 */
function countNonzero(d: Uint8Array): { nonzero: number; sampled: number } {
  const step = Math.max(4, Math.floor(d.length / 8192 / 4) * 4);
  let nonzero = 0;
  let sampled = 0;
  for (let i = 0; i + 3 < d.length; i += step) {
    sampled += 4;
    if ((d[i] ?? 0) !== 0 || (d[i + 1] ?? 0) !== 0 || (d[i + 2] ?? 0) !== 0 || (d[i + 3] ?? 0) !== 0) {
      nonzero += 4;
    }
  }
  return { nonzero, sampled };
}

async function probeFrame(): Promise<{ probe: FrameProbe; payload: Uint8Array | null }> {
  try {
    // **走 IPC**（`pull_frame`）而不是 `glass://` 自定义协议 —— 后者在真机上
    // 约十几次响应后会让 WebView2 渲染进程的 JS 整体停止执行，本窗口也会僵住。
    //
    // `sinceSeq = 0`：诊断只关心"此刻帧槽里是什么"，不论新旧。
    // 本窗口与面板是两个独立 webview，各自走自己的 IPC 客户端读同一帧槽，
    // 所以这仍然是**独立取样**（只是不再独立于传输层）。
    const buf = await pullFrame(0);
    if (buf.byteLength < FRAME_HEADER_BYTES) {
      return {
        probe: { ...EMPTY_PROBE, httpStatus: 0, detail: "后端暂无帧（采集未产生或已停止）" },
        payload: null,
      };
    }
    // 帧头（小端）：seq(8) | w(4) | h(4)，其后紧跟 RGBA。
    const head = new DataView(buf);
    const headerSeq = Number(head.getBigUint64(0, true));
    const w = head.getUint32(8, true);
    const h = head.getUint32(12, true);
    const payload = new Uint8Array(buf, FRAME_HEADER_BYTES);
    const sizeOk = w > 0 && h > 0 && payload.byteLength === w * h * 4;
    const { nonzero, sampled } = sizeOk ? countNonzero(payload) : { nonzero: 0, sampled: 0 };
    return {
      probe: {
        ok: sizeOk,
        // 走 IPC 后没有 HTTP 状态：200 表示"拿到了帧"，0 表示"没有帧"。
        httpStatus: 200,
        bytes: payload.byteLength,
        pixels: payload.byteLength,
        magicOk: sizeOk,
        headerSeq,
        headerW: w,
        headerH: h,
        nonzero,
        sampled,
        blank: sizeOk && nonzero === 0,
        detail: sizeOk
          ? ""
          : `尺寸/长度自洽性失败：${w}×${h} 需 ${w * h * 4} 字节，实得 ${payload.byteLength}`,
      },
      payload: sizeOk ? payload : null,
    };
  } catch (e) {
    return {
      probe: { ...EMPTY_PROBE, detail: e instanceof Error ? e.message : String(e) },
      payload: null,
    };
  }
}

/**
 * 把原始像素降采样成可显示的 ImageData。
 *
 * **通道序：载荷已经是 RGBA**。桌面纹理是 `B8G8R8A8_UNORM`，但 Rust 侧
 * `capture_region` 在逐行读回时就把 B/R 换位了（见该函数注释），这里是
 * 直接搬运，不再换第二次 —— 换两次等于没换，预览会显示成反色相近的效果，
 * 而那种偏色**极容易被误判成"光学/色调参数不对"**，正是本项目前 8 轮
 * 空转的典型症状。
 *
 * 整数步长抽样而非 `drawImage` 缩放：源数据不是浏览器认识的图像格式，
 * 只能自己搬。整帧 33MB 按 1/16 步长即降到 ~30 万像素，够看清"是不是黑的"。
 */
function buildPreview(
  payload: Uint8Array,
  w: number,
  h: number,
): { data: ImageData; sw: number; sh: number; step: number } | null {
  const step = Math.max(1, Math.ceil(Math.max(w / PREVIEW_MAX_W, h / PREVIEW_MAX_H)));
  const sw = Math.floor(w / step);
  const sh = Math.floor(h / step);
  if (sw <= 0 || sh <= 0) return null;
  const out = new Uint8ClampedArray(sw * sh * 4);
  for (let y = 0; y < sh; y++) {
    const srcRow = y * step * w * 4;
    const dstRow = y * sw * 4;
    for (let x = 0; x < sw; x++) {
      const si = srcRow + x * step * 4;
      const di = dstRow + x * 4;
      out[di] = payload[si] ?? 0; // R
      out[di + 1] = payload[si + 1] ?? 0; // G
      out[di + 2] = payload[si + 2] ?? 0; // B
      out[di + 3] = 255; // 桌面帧的 A 多为 0，强制不透明才能看见
    }
  }
  return { data: new ImageData(out, sw, sh), sw, sh, step };
}

// ---------------------------------------------------------------------------
// 判读：把三路信号合成一句话
// ---------------------------------------------------------------------------

interface Verdict {
  tone: "ok" | "warn" | "bad" | "idle";
  text: string;
  hint: string;
}

function interpret(
  cap: CaptureStatsSnapshot,
  glass: GlassStatusSnapshot,
  probe: FrameProbe,
  glassAgeMs: number,
): Verdict {
  // ---- 1. 采集端都没起来 ----
  if (cap.sessionCreated === 0 && cap.sessionFailed === 0) {
    return {
      tone: "bad",
      text: "捕获线程未启动或尚未尝试建会话。",
      hint: "检查主进程是否走到 capture::spawn；也可能是程序刚启动不足一个轮询周期。",
    };
  }
  if (cap.sessionCreated === 0) {
    const notAvail = (cap.lastError >>> 0) === 0x887a0022;
    return {
      tone: "bad",
      text: `DDA 会话建立失败 ${cap.sessionFailed} 次，最近错误 ${hrName(cap.lastError)}。`,
      hint: notAvail
        ? "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE：同一输出已有会话占用，或全系统 Desktop Duplication 会话已满（上限 4）。关掉录屏/远程桌面/其他截图工具后重试。"
        : "会话创建失败通常来自显卡驱动或 D3D 设备创建阶段，看 <项目>/src-tauri/src/win32/capture.rs 的 session_failed 分支。",
    };
  }

  // ---- 2. 拿到了帧但从没拷成功 ----
  if (cap.copyOk === 0) {
    if (cap.unsupportedFormat > 0) {
      return {
        tone: "bad",
        text: `交付纹理格式不受支持（${cap.unsupportedFormat} 次，最近 ${fmtName(cap.lastFormat)}）。`,
        hint: "拷贝不做格式转换，staging 必须与交付纹理同格式。正确值应为 87 B8G8R8A8_UNORM。",
      };
    }
    if (cap.copyError > 0) {
      return {
        tone: "bad",
        text: `拷贝失败 ${cap.copyError} 次，最近错误 ${hrName(cap.lastError)}。`,
        hint: "常见于 staging 与交付纹理维度/格式不匹配（D3D11 非法调用默认不抛错，只在 debug layer 可见）。",
      };
    }
    if (cap.frameEmpty > 0 && cap.frameEmpty >= cap.frameAcquired) {
      return {
        tone: "bad",
        text: `已取到 ${cap.frameAcquired} 帧，但全部被判为空白。`,
        hint: "DDA 在会话初期会给出 LastPresentTime == 0 的帧，其内容未定义。若持续如此，说明过滤没生效（见 capture.rs::acquire_once）。",
      };
    }
    return {
      tone: "warn",
      text: `会话已建立，但到目前还没有成功拷贝过一帧（超时 ${cap.frameTimeout} 次）。`,
      hint: "悬浮物处于隐藏位时取样区为空，不产生帧——这是正常的。把滑条滑出来再看。",
    };
  }

  // ---- 3. 采集正常，看渲染端 ----
  if (glass.updatedAt === 0) {
    return {
      tone: "warn",
      text: `采集端已成功拷贝 ${cap.copyOk} 帧，但面板窗口从未上报渲染状态。`,
      hint: "面板 webview 没跑起来（页面加载失败），或 report_glass_status 未注册。查 src/index.html 与 capabilities。",
    };
  }
  // v0.3.1：装配心跳存在（updatedAt != 0）却一次取帧都没发生 ——
  // 这正是"帧循环没启动"的特征。旧版本会把这种情况混进"从未上报"，无从区分。
  if (glass.polls === 0) {
    return {
      tone: "bad",
      text: `面板页面已跑起来，但取帧循环一次都没执行（polls = 0）。`,
      hint: `前端认为相位是 ${JSON.stringify(glass.phase)}。相位事件在 webview 订阅前发出会被永久丢弃 → 循环不启动 → 胶囊没有背景内容。见 liquidGlass.ts 的 startFrameLoop/setVisible。`,
    };
  }
  if (glassAgeMs > 5000) {
    return {
      tone: "warn",
      text: `面板窗口已 ${(glassAgeMs / 1000).toFixed(1)} 秒未上报状态（已取帧 ${glass.polls} 次）。`,
      hint: "若相位为隐藏态，取帧会降到 1Hz 保活（v0.3.1 起不再停循环），上报随之变稀属正常。",
    };
  }

  const sizeMismatch =
    glass.frameW > 0 &&
    cap.regionW > 0 &&
    (glass.frameW !== cap.regionW || glass.frameH !== cap.regionH);
  if (sizeMismatch) {
    return {
      tone: "bad",
      text: `渲染端帧尺寸 ${glass.frameW}×${glass.frameH} 与采集端取样区 ${cap.regionW}×${cap.regionH} 不一致。`,
      hint: "帧头里的宽高和实际像素区对不上，问题在 backdrop.rs 写头或前端读头。",
    };
  }

  if (glass.status === "ok" && glass.rendered > 0) {
    if (probe.blank) {
      return {
        tone: "warn",
        text: `渲染端已上屏 ${glass.rendered} 帧，但本页此刻独立取样到的帧全零。`,
        hint: "两者取样时刻不同，若持续如此说明帧内容在两种读取路径下不一致，重点查 staging 的 Map 标志（必须是 MAP_READ）。",
      };
    }
    return {
      tone: "ok",
      text: `链路正常：采集拷贝 ${cap.copyOk} 帧，渲染上屏 ${glass.rendered} 帧，最近序号 ${glass.seq}。`,
      hint: `帧 ${glass.frameW}×${glass.frameH}，缩放 ${glass.scale.toFixed(3)}×（175% DPI 应为 1.75 左右）。`,
    };
  }
  if (glass.status === "blank") {
    return {
      tone: "bad",
      text: `采集端有帧（拷贝 ${cap.copyOk} 次），但渲染端把 ${glass.blank} 帧判为空白并丢弃。`,
      hint: "说明后端发的是全零像素。若 capture.rs 的 frame_empty 计数为 0，则空白是在拷贝阶段产生的，重点查 staging 与 Map 标志。",
    };
  }
  if (glass.status === "bad-payload") {
    return {
      tone: "bad",
      text: `渲染端拒收帧载荷：${glass.detail}`,
      hint: "帧头（magic/seq/w/h）与像素区不自洽，或后端换过协议而前端没跟上。",
    };
  }
  if (glass.status === "no-frame" || glass.status === "error") {
    return {
      tone: "bad",
      text: `渲染端取帧失败：${glass.detail}`,
      hint: "自定义协议 glass:// 未响应；检查 main.rs 的 register_uri_scheme_protocol 注册与 CSP 的 connect-src。",
    };
  }
  return {
    tone: "idle",
    text: "数据不足，等待下一次取样。",
    hint: "面板窗口刚刚启动时属正常。",
  };
}

// ---------------------------------------------------------------------------
// 视图
// ---------------------------------------------------------------------------

/** 一个可更新的字段槽。 */
interface Slot {
  row: HTMLElement;
  value: HTMLElement;
}

function makeRow(key: string): Slot {
  const row = document.createElement("div");
  row.className = "d-row";
  const k = document.createElement("span");
  k.className = "k";
  k.textContent = key;
  const v = document.createElement("span");
  v.className = "v";
  v.textContent = "—";
  row.append(k, v);
  return { row, value: v };
}

function setSlot(slot: Slot, text: string, tone: "ok" | "warn" | "bad" | "dim" | null = null): void {
  slot.value.textContent = text;
  if (tone) {
    slot.row.dataset.tone = tone;
  } else {
    delete slot.row.dataset.tone;
  }
}

/** 组装静态骨架，返回全部可写槽位与画布。 */
function buildSkeleton(root: HTMLElement) {
  root.innerHTML = `
    <div class="d-head">
      <span class="d-title">HDR SDR 诊断面板</span>
      <span class="d-pill" data-slot="pill" data-tone="idle"><span class="dot"></span><span data-slot="pillText">等待数据</span></span>
      <span class="d-spacer"></span>
      <span class="d-ver" data-slot="ver"></span>
    </div>
    <div class="d-verdict" data-slot="verdict" data-tone="idle">
      <span data-slot="verdictText">正在采集诊断数据…</span>
      <span class="hint" data-slot="verdictHint"></span>
    </div>
    <div class="d-grid">
      <div class="d-card">
        <h2>采集端 · DDA<span class="d-spacer"></span></h2>
        <div class="body" data-slot="capRows"></div>
        <div class="d-note">LastPresentTime == 0 的帧内容未定义，必须丢弃后重取。</div>
      </div>
      <div class="d-card">
        <h2>渲染端 · Liquid Glass<span class="d-spacer"></span></h2>
        <div class="body" data-slot="glassRows"></div>
        <div class="d-note">渲染端状态由面板窗口经 IPC 上报，250ms 节流。</div>
      </div>
      <div class="d-card d-wide">
        <h2>原始帧预览 · 本页独立取样<span class="d-spacer"></span>
          <span class="d-stamp" data-slot="probeStamp"></span>
        </h2>
        <div class="d-preview">
          <div class="d-stage" data-slot="stage">
            <div class="placeholder">等待第一帧…</div>
          </div>
          <div class="meta" data-slot="probeRows"></div>
        </div>
      </div>
    </div>
    <div class="d-foot">
      <button class="d-btn" data-act="refresh" type="button">立即取样</button>
      <button class="d-btn" data-act="copy" type="button">复制诊断文本</button>
      <span class="d-spacer"></span>
      <span class="d-stamp" data-slot="tickStamp"></span>
      <button class="d-btn" data-act="close" type="button">关闭</button>
    </div>`;

  const q = <T extends HTMLElement>(name: string): T =>
    root.querySelector<T>(`[data-slot="${name}"]`)!;

  const capRows = q<HTMLElement>("capRows");
  const glassRows = q<HTMLElement>("glassRows");
  const probeRows = q<HTMLElement>("probeRows");

  const capSlots = {
    sessionCreated: makeRow("会话建立成功"),
    sessionFailed: makeRow("会话建立失败"),
    frameAcquired: makeRow("取到帧（已过滤）"),
    frameEmpty: makeRow("空白帧（丢弃）"),
    frameTimeout: makeRow("取帧超时"),
    accessLost: makeRow("ACCESS_LOST"),
    copyOk: makeRow("拷贝成功"),
    copyError: makeRow("拷贝失败"),
    unsupportedFormat: makeRow("格式不支持"),
    lastFormat: makeRow("交付纹理格式"),
    lastError: makeRow("最近错误"),
    region: makeRow("取样区"),
    hasFrame: makeRow("当前持有帧"),
  };
  capRows.append(...Object.values(capSlots).map((s) => s.row));

  const glassSlots = {
    status: makeRow("状态"),
    seq: makeRow("帧序号"),
    size: makeRow("解码尺寸"),
    scale: makeRow("缩放系数"),
    rendered: makeRow("成功上屏"),
    blank: makeRow("判空丢弃"),
    detail: makeRow("最近说明"),
    age: makeRow("距上次上报"),
  };
  glassRows.append(...Object.values(glassSlots).map((s) => s.row));

  const probeSlots = {
    http: makeRow("HTTP"),
    bytes: makeRow("响应字节"),
    // 标签随传输层改过：帧改走 IPC 后**不再有帧头魔数**，
    // 这里校验的是"长度自洽性"（w×h×4 == 载荷字节数）。标签留着旧的会误导。
    magic: makeRow("帧载荷校验"),
    header: makeRow("帧头声明"),
    nonzero: makeRow("非零字节（抽样）"),
    verdict: makeRow("本页判读"),
  };
  probeRows.append(...Object.values(probeSlots).map((s) => s.row));

  return {
    pill: q<HTMLElement>("pill"),
    pillText: q<HTMLElement>("pillText"),
    ver: q<HTMLElement>("ver"),
    verdict: q<HTMLElement>("verdict"),
    verdictText: q<HTMLElement>("verdictText"),
    verdictHint: q<HTMLElement>("verdictHint"),
    stage: q<HTMLElement>("stage"),
    probeStamp: q<HTMLElement>("probeStamp"),
    tickStamp: q<HTMLElement>("tickStamp"),
    capSlots,
    glassSlots,
    probeSlots,
  };
}

// ---------------------------------------------------------------------------
// 装配
// ---------------------------------------------------------------------------

export function mountDiag(root: HTMLElement): void {
  const v = buildSkeleton(root);

  let latestCap: CaptureStatsSnapshot | null = null;
  let latestGlass: GlassStatusSnapshot | null = null;
  let latestProbe: FrameProbe = { ...EMPTY_PROBE };
  let lastProbedAt = 0;
  /** 运行时版本号（报告里带上，方便对号入座）。 */
  let appVersion = "?";

  const canvas = document.createElement("canvas");

  function renderCanvas(payload: Uint8Array | null, w: number, h: number): void {
    if (!payload || w === 0 || h === 0) {
      if (v.stage.contains(canvas)) canvas.remove();
      if (!v.stage.querySelector(".placeholder")) {
        const ph = document.createElement("div");
        ph.className = "placeholder";
        ph.textContent = "无有效像素区";
        v.stage.replaceChildren(ph);
      }
      return;
    }
    const built = buildPreview(payload, w, h);
    if (!built) return;
    canvas.width = built.sw;
    canvas.height = built.sh;
    canvas.getContext("2d")!.putImageData(built.data, 0, 0);
    if (!v.stage.contains(canvas)) v.stage.replaceChildren(canvas);
    v.probeStamp.textContent = `1/${built.step} 抽样 · ${built.sw}×${built.sh}`;
  }

  function render(): void {
    const cap = latestCap;
    const glass = latestGlass;
    const probe = latestProbe;

    // ---- 采集端 ----
    if (cap) {
      const c = v.capSlots;
      setSlot(c.sessionCreated, String(cap.sessionCreated), cap.sessionCreated > 0 ? "ok" : null);
      setSlot(c.sessionFailed, String(cap.sessionFailed), cap.sessionFailed > 0 ? "bad" : null);
      setSlot(c.frameAcquired, String(cap.frameAcquired));
      setSlot(c.frameEmpty, String(cap.frameEmpty), cap.frameEmpty > 0 ? "warn" : null);
      setSlot(c.frameTimeout, String(cap.frameTimeout), null);
      setSlot(c.accessLost, String(cap.accessLost), cap.accessLost > 0 ? "warn" : null);
      setSlot(c.copyOk, String(cap.copyOk), cap.copyOk > 0 ? "ok" : null);
      setSlot(c.copyError, String(cap.copyError), cap.copyError > 0 ? "bad" : null);
      setSlot(c.unsupportedFormat, String(cap.unsupportedFormat), cap.unsupportedFormat > 0 ? "bad" : null);
      setSlot(
        c.lastFormat,
        fmtName(cap.lastFormat),
        cap.lastFormat === 87 ? "ok" : cap.lastFormat < 0 ? "dim" : "warn",
      );
      setSlot(c.lastError, hrName(cap.lastError), cap.lastError !== 0 ? "bad" : "dim");
      setSlot(c.region, cap.regionW > 0 ? `${cap.regionW} × ${cap.regionH}` : "未设置", cap.regionW > 0 ? null : "dim");
      setSlot(c.hasFrame, cap.hasFrame ? "是" : "否", cap.hasFrame ? "ok" : "warn");
    }

    // ---- 渲染端 ----
    if (glass) {
      const g = v.glassSlots;
      const age = glass.updatedAt === 0 ? 0 : Date.now() - glass.updatedAt;
      setSlot(g.status, GLASS_STATUS_LABEL[glass.status] ?? glass.status, glass.status === "ok" ? "ok" : "bad");
      setSlot(g.seq, String(glass.seq), glass.seq > 0 ? "ok" : "dim");
      setSlot(
        g.size,
        glass.frameW > 0 ? `${glass.frameW} × ${glass.frameH}` : "未知",
        glass.frameW > 0 ? null : "dim",
      );
      setSlot(g.scale, glass.scale > 0 ? `${glass.scale.toFixed(3)} ×` : "未知", glass.scale > 0 ? null : "dim");
      setSlot(g.rendered, String(glass.rendered), glass.rendered > 0 ? "ok" : null);
      setSlot(g.blank, String(glass.blank), glass.blank > 0 ? "bad" : null);
      setSlot(g.detail, glass.detail || "—", glass.detail ? "dim" : null);
      setSlot(
        g.age,
        glass.updatedAt === 0 ? "从未上报" : `${age} ms 前`,
        glass.updatedAt === 0 ? "bad" : age > 5000 ? "warn" : "ok",
      );
    }

    // ---- 原始帧 ----
    {
      const p = v.probeSlots;
      setSlot(p.http, probe.httpStatus === 0 ? probe.detail || "—" : "IPC", probe.ok ? "ok" : "bad");
      setSlot(p.bytes, probe.bytes > 0 ? `${probe.bytes.toLocaleString()} 字节` : "—", null);
      setSlot(
        p.magic,
        probe.bytes > 0 ? (probe.magicOk ? "长度自洽（IPC）" : "不符") : "—",
        probe.bytes > 0 ? (probe.magicOk ? "ok" : "bad") : "dim",
      );
      setSlot(
        p.header,
        probe.headerW > 0
          ? `${probe.headerW} × ${probe.headerH} · seq ${probe.headerSeq} · 像素 ${probe.pixels.toLocaleString()}`
          : "—",
        null,
      );
      setSlot(
        p.nonzero,
        probe.sampled > 0
          ? `${probe.nonzero.toLocaleString()} / ${probe.sampled.toLocaleString()}`
          : "—",
        probe.sampled > 0 ? (probe.blank ? "bad" : "ok") : "dim",
      );
      setSlot(
        p.verdict,
        probe.sampled === 0 ? "—" : probe.blank ? "整帧全零" : "含有效像素",
        probe.sampled === 0 ? "dim" : probe.blank ? "bad" : "ok",
      );
    }

    // ---- 结论 ----
    if (cap && glass) {
      const age = glass.updatedAt === 0 ? Number.POSITIVE_INFINITY : Date.now() - glass.updatedAt;
      const verdict = interpret(cap, glass, probe, age);
      v.verdict.dataset.tone = verdict.tone;
      v.verdictText.textContent = verdict.text;
      v.verdictHint.textContent = verdict.hint;
      v.pill.dataset.tone = verdict.tone === "idle" ? "idle" : verdict.tone;
      v.pillText.textContent =
        verdict.tone === "ok"
          ? "链路正常"
          : verdict.tone === "warn"
            ? "需要注意"
            : verdict.tone === "bad"
              ? "链路故障"
              : "等待数据";
    }

    v.tickStamp.textContent = `更新于 ${new Date().toLocaleTimeString("zh-CN")}`;
  }

  // ---- 采集循环 ----
  async function tick(): Promise<void> {
    const [cap, glass] = await Promise.all([
      getCaptureStats().catch(() => null),
      getGlassStatus().catch(() => null),
    ]);
    if (cap) latestCap = cap;
    if (glass) latestGlass = glass;
    render();
  }

  async function probeTick(): Promise<void> {
    const { probe, payload } = await probeFrame();
    latestProbe = probe;
    lastProbedAt = Date.now();
    renderCanvas(
      payload,
      probe.headerW > 0 ? probe.headerW : 0,
      probe.headerH > 0 ? probe.headerH : 0,
    );
    render();
  }

  // ---- 交互 ----
  root.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("[data-act]");
    if (!btn) return;
    switch (btn.dataset.act) {
      case "refresh":
        void probeTick();
        void tick();
        break;
      case "copy":
        void copyReport();
        break;
      case "close":
        void closeDiagWindow().catch(() => window.close());
        break;
      default:
        break;
    }
  });

  /** 汇总一份纯文本报告，方便贴进 issue 或聊天。 */
  function buildReport(): string {
    const now = new Date().toISOString();
    const lines: string[] = [
      `# HDR SDR 诊断报告 v${appVersion} @ ${now}`,
      "",
      "## 渲染端（面板窗口上报）",
      latestGlass
        ? [
            `status      = ${latestGlass.status}`,
            `seq         = ${latestGlass.seq}`,
            `frame       = ${latestGlass.frameW}x${latestGlass.frameH}`,
            `scale       = ${latestGlass.scale}`,
            `rendered    = ${latestGlass.rendered}`,
            `blank       = ${latestGlass.blank}`,
            `detail      = ${latestGlass.detail || "(空)"}`,
            `updatedAt   = ${latestGlass.updatedAt} (${latestGlass.updatedAt === 0 ? "从未上报" : `${Date.now() - latestGlass.updatedAt}ms 前`})`,
          ].join("\n")
        : "(尚未取到)",
      "",
      "## 采集端（Rust DDA）",
      latestCap
        ? [
            `sessionCreated      = ${latestCap.sessionCreated}`,
            `sessionFailed       = ${latestCap.sessionFailed}`,
            `frameAcquired       = ${latestCap.frameAcquired}`,
            `frameEmpty          = ${latestCap.frameEmpty}`,
            `frameTimeout        = ${latestCap.frameTimeout}`,
            `accessLost          = ${latestCap.accessLost}`,
            `copyOk              = ${latestCap.copyOk}`,
            `copyError           = ${latestCap.copyError}`,
            `unsupportedFormat   = ${latestCap.unsupportedFormat}`,
            `lastFormat          = ${fmtName(latestCap.lastFormat)}`,
            `lastError           = ${hrName(latestCap.lastError)}`,
            `region              = ${latestCap.regionW}x${latestCap.regionH}`,
            `hasFrame            = ${latestCap.hasFrame}`,
          ].join("\n")
        : "(尚未取到)",
      "",
      "## 原始帧探针（本页独立取样）",
      [
        `http      = ${latestProbe.httpStatus}`,
        `bytes     = ${latestProbe.bytes}`,
        `pixels    = ${latestProbe.pixels}`,
        `magicOk   = ${latestProbe.magicOk}`,
        `header    = ${latestProbe.headerW}x${latestProbe.headerH} seq ${latestProbe.headerSeq}`,
        `nonzero   = ${latestProbe.nonzero} / ${latestProbe.sampled}`,
        `blank     = ${latestProbe.blank}`,
        `detail    = ${latestProbe.detail || "(空)"}`,
        `probedAt  = ${lastProbedAt}`,
      ].join("\n"),
      "",
      "## 结论",
      latestCap && latestGlass
        ? interpret(
            latestCap,
            latestGlass,
            latestProbe,
            latestGlass.updatedAt === 0 ? Number.POSITIVE_INFINITY : Date.now() - latestGlass.updatedAt,
          ).text
        : "(数据不足)",
    ];
    return lines.join("\n");
  }

  async function copyReport(): Promise<void> {
    const text = buildReport();
    try {
      await navigator.clipboard.writeText(text);
      v.probeStamp.textContent = "诊断文本已复制";
    } catch {
      // 剪贴板被拒（无焦点窗口）：退化成选中，至少让用户能手动 Ctrl+C。
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.cssText = "position:fixed;left:-9999px;top:0";
      document.body.append(ta);
      ta.select();
      v.probeStamp.textContent = "剪贴板不可用，已选中文本";
    }
  }

  // ---- 启动 ----
  // 版本号从运行时读，不写死 —— 写死的那个只在被遗忘时才有存在感。
  void getVersion()
    .then((ver) => {
      appVersion = ver;
      v.ver.textContent = `v${ver}`;
    })
    .catch(() => {
      appVersion = "未知";
      v.ver.textContent = "版本未知";
    });

  void tick();
  void probeTick();
  window.setInterval(() => void tick(), POLL_MS);
  window.setInterval(() => void probeTick(), FRAME_MS);
}
