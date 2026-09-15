//! `bridge.ts` —— L3 UI 层唯一的 IPC 出口。
//!
//! 与 Rust 侧 `commands.rs` 的命令名 / DTO / 事件名逐一对应。
//! 迁移到 Electron / WPF 壳时，只替换本文件的实现，UI 其余代码零改动。
//! **本文件禁止引入任何 Win32 / Node API。**

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// 类型契约（与 Rust DTO 一致，字段为 camelCase）
// ---------------------------------------------------------------------------

/** 一台显示器在 UI 中展示的最小信息（`commands::MonitorInfo`）。 */
export interface MonitorInfo {
  key: string;
  name: string;
  index: number;
  isPrimary: boolean;
  hdrSupported: boolean;
  hdrEnabled: boolean;
  bitsPerColor: number;
  refreshHz: number;
}

/** 当前选中显示器的 SDR 亮度读数（`commands::SdrReading`）。 */
export interface SdrReading {
  key: string;
  name: string;
  percent: number;
  nits: number;
  raw: number;
  hdrSupported: boolean;
  hdrEnabled: boolean;
  readable: boolean;
}

/** 写入结果（`core::model::WriteResult`，serde tag="kind"）。 */
export type WriteResult =
  | { kind: "applied"; raw: number }
  | { kind: "adjusted"; requested: number; actual: number }
  | { kind: "failed"; code: string; message: string; win32?: number };

/** 错误载荷（`commands::CommandError`）。 */
export interface CommandError {
  code: string;
  message: string;
  detail?: unknown;
}

/** 预设档位（`store::settings::Preset`）。 */
export interface Preset {
  id: string;
  name: string;
  percent: number;
  iconId: string;
}

/** 配置（`store::settings::AppSettings`，camelCase）。 */
export interface AppSettings {
  version: number;
  unit: "percent" | "nits" | "multiple";
  step: number;
  stepShift: number;
  hotkey: string;
  followMouseMonitor: boolean;
  hideOnBlur: boolean;
  autostart: boolean;
  lastWindowPos: { x: number; y: number; monitorKey: string };
  lastMonitorKey: string;
  presets: Preset[];
  perMonitorMemory: Record<string, number>;
  firstRun: boolean;
  /** v2：贴边方向（null = 自由悬浮）。 */
  dockSide: "left" | "right" | null;
  /** v2：贴边时的垂直位置（物理像素）。 */
  widgetY: number;
}

/** v2：贴边方向（`widget.rs` / `edge.rs` 契约）。 */
export type Edge = "left" | "right";

/** v2：状态机相位（`edge::Phase`）。 */
export type Phase =
  | "hidden"
  | "revealing"
  | "visible"
  | "hovered"
  | "pressed"
  | "dragging"
  | "hiding"
  | "suppressed";

/** v2：悬浮物贴边状态（`edge::WidgetState`，camelCase JSON）。 */
export interface WidgetState {
  docked: Edge | null;
  expanded: boolean;
  phase: Phase;
}

/** v2：拖拽中的边缘接近提示（`edge::DragHint`）。 */
export interface DragHint {
  nearEdge: Edge | null;
}

/**
 * 背景捕获链路统计（`win32::capture::CaptureStatsSnapshot`，camelCase）。
 *
 * 这是 v0.3 诊断面的核心：玻璃背板"黑块"到底是哪一步断的，看这张表就知道。
 * 判读顺序见 `ui/diag.ts::interpret()`。
 */
export interface CaptureStatsSnapshot {
  /** 成功创建 DDA 会话次数。> 0 说明 DuplicateOutput 没被别的进程占满。 */
  sessionCreated: number;
  /** 会话创建失败次数（常见 `0x887A0022` NOT_CURRENTLY_AVAILABLE）。 */
  sessionFailed: number;
  /** 拿到"内容已定义"帧的次数（已按 LastPresentTime != 0 过滤）。 */
  frameAcquired: number;
  /** 拿到帧但内容全零 / 未定义而丢弃的次数。 */
  frameEmpty: number;
  /** AcquireNextFrame 超时次数（静止桌面属正常，不应丢帧）。 */
  frameTimeout: number;
  /** DXGI_ERROR_ACCESS_LOST 次数（全屏切换 / 分辨率变更时正常）。 */
  accessLost: number;
  /** CopySubresourceRegion 成功次数。 */
  copyOk: number;
  /** 拷贝失败次数（多半是 staging 格式与交付纹理不匹配）。 */
  copyError: number;
  /** 交付纹理格式不被支持而跳过的次数。 */
  unsupportedFormat: number;
  /** 交付纹理的 DXGI_FORMAT 数值（87 = B8G8R8A8_UNORM 才是对的）。 */
  lastFormat: number;
  /** 最近一次失败的错误码（HRESULT，0 表示无）。 */
  lastError: number;
  /** 最近一帧的宽（采样区域，物理像素）。 */
  regionW: number;
  /** 最近一帧的高（采样区域，物理像素）。 */
  regionH: number;
  /** 当前是否持有有效帧——玻璃能不能上屏就看它。 */
  hasFrame: boolean;
}

/** 诊断信息（`commands::Diagnostics`）。 */
export interface Diagnostics {
  route: string;
  targets: number;
  lastError: string | null;
  fallbackAvailable: boolean;
  /** v0.3：背景捕获链路统计。 */
  capture: CaptureStatsSnapshot;
}

/** 帧摄取的直观状态（与 `liquidGlass.ts::GlassFrameStatus` 同值域）。 */
export type GlassFrameStatus =
  | "idle"
  | "ok"
  | "blank"
  | "no-frame"
  | "bad-payload"
  | "error";

/**
 * 渲染端自报的帧状态（`commands::GlassStatusSnapshot`）。
 *
 * 面板窗口是独立 webview，诊断窗口读不到它的内存对象，所以渲染端把状态经
 * `reportGlassStatus` 上报给 Rust，诊断窗口再用 `getGlassStatus` 取回。
 */
export interface GlassStatusSnapshot {
  status: GlassFrameStatus;
  seq: number;
  frameW: number;
  frameH: number;
  scale: number;
  detail: string;
  rendered: number;
  blank: number;
  /** 单帧渲染耗时滚动平均（ms，含模糊 + 裁剪 + 逐像素折射）。0 = 尚未渲染过。 */
  renderMs: number;
  /**
   * 前端已发起的取帧请求次数（含失败的）。
   *
   * 诊断价值：它能区分两种**症状相同、病因相反**的故障 ——
   *   - `polls === 0` → 帧循环压根没启动（相位判定问题，前端认为不可见）
   *   - `polls` 很大而 `rendered === 0` → 循环在跑，是取帧链路的问题
   * 没有这个计数时，两者在诊断面上都表现为"没有画面"，只能靠猜。
   */
  polls: number;
  /**
   * 前端**自己看到的**相位（即胶囊 DOM 上的 `data-phase`）。
   *
   * 与 Rust 侧状态机相位对照：不一致就说明 `widget:state` 事件丢失，
   * 前端卡在旧相位上 —— 这正是帧循环永久停摆的典型成因。
   */
  phase: string;
  /** 服务端打戳的上报时刻（Unix 毫秒，0 = 从未上报）。 */
  updatedAt: number;
}

// ---------------------------------------------------------------------------
// 命令封装
// ---------------------------------------------------------------------------

/** 枚举全部活动显示器。 */
export function listMonitors(): Promise<MonitorInfo[]> {
  return invoke<MonitorInfo[]>("list_monitors");
}

/** 读取指定显示器（省略则取当前选中）的 SDR 内容亮度。 */
export function readSdrLevel(key?: string): Promise<SdrReading> {
  return invoke<SdrReading>("read_sdr_level", { key: key ?? null });
}

/** 写入指定显示器的亮度百分比。 */
export function applyPercent(key: string, percent: number): Promise<WriteResult> {
  return invoke<WriteResult>("apply_percent", { key, percent });
}

/** 一键应用预设（day / movie / night）。 */
export function applyPreset(preset: string): Promise<WriteResult> {
  return invoke<WriteResult>("apply_preset", { preset });
}

/** 读取配置。 */
export function getSettings(): Promise<AppSettings> {
  return invoke<AppSettings>("get_settings");
}

/** 保存配置（落盘并同步自启状态）。 */
export function saveSettings(next: AppSettings): Promise<void> {
  return invoke<void>("set_settings", { next });
}

/** 切换面板显隐。 */
export function toggleWindow(): Promise<void> {
  return invoke<void>("toggle_window");
}

/** v2：拖拽移动悬浮物（前端 pointermove 上报位移）。 */
export function moveWindow(dx: number, dy: number): Promise<void> {
  return invoke<void>("move_window", { dx, dy });
}

/** v2：松手结束拖拽（贴边 / 自由落位 / 越界纠正 + 持久化）。 */
export function endWindowDrag(): Promise<void> {
  return invoke<void>("end_window_drag");
}

/** v2：读悬浮物贴边状态。 */
export function getWidgetState(): Promise<WidgetState> {
  return invoke<WidgetState>("get_widget_state");
}

/** v2：上报指针交互相位（hovered/pressed/dragging/visible）。 */
export function setPointerPhase(
  phase: "hovered" | "pressed" | "dragging" | "visible",
): Promise<void> {
  return invoke<void>("set_pointer_phase", { phase });
}

/** 退出进程。 */
export function quitApp(): Promise<void> {
  return invoke<void>("quit_app");
}

/** 打开系统设置 HDR 页。 */
export function openHdrSettings(): Promise<void> {
  return invoke<void>("open_hdr_settings");
}

/** 复制诊断信息。 */
export function getDiagnostics(): Promise<Diagnostics> {
  return invoke<Diagnostics>("get_diagnostics");
}

/**
 * v0.3：只取背景捕获链路统计。
 *
 * 单独成一个轻量命令是有原因的——`get_diagnostics` 会去抢控制器锁（可能触发
 * 显示器枚举，几十到几百毫秒）。诊断 HUD 要 500ms 轮询，只能走这条捷径。
 */
export function getCaptureStats(): Promise<CaptureStatsSnapshot> {
  return invoke<CaptureStatsSnapshot>("get_capture_stats");
}

/**
 * 帧载荷的字节布局（小端），与 `commands::pull_frame` 一一对应。
 *
 * `[seq: u64][w: u32][h: u32][rgba...]`；`byteLength === 0` 表示后端无新帧。
 *
 * 为什么二进制而不是 base64 JSON：动态桌面下每帧都要编解码并多传 33% 体积，
 * 是「延迟高、一动就卡」的主因之一。
 */
export const FRAME_HEADER_BYTES = 16;

/**
 * 最近一次 `pull_frame` 的**原始返回形态**（诊断用）。
 *
 * 为什么需要它：Tauri 的 IPC 在不同传输路径下，raw body 到前端可能是
 * `ArrayBuffer`、`Uint8Array`，也可能是 JSON 化的**数字数组**（98KB 帧会膨胀成
 * 几十万字符，那会重新引入"延迟高、一动就卡"）。只看 `renderMs` 判断不出来 ——
 * 那个数只含渲染，不含传输。所以把形态摆到诊断面板上，一眼可辨。
 */
export let lastFrameTransfer = "(未取过帧)";

/**
 * 最近一次 `pull_frame` 的**端到端耗时**（ms，含 IPC 传输 + 解码）。
 *
 * 为什么必须单独量它：`renderMs`（liquidGlass 上报）**只含渲染**，
 * 不含传输 —— 上一轮就是只看 `renderMs`（0.9ms）误以为一切良好，
 * 实际传输层已经在走 JSON 数字数组那条慢路径。
 *
 * ⚠️ 只报"最近一次"是不够的：帧停下后最后一次往往是**空响应**（0.2ms），
 * 会把真实开销盖掉（本轮就因此量了三轮都量不到）。所以另配一个
 * 只统计**非空帧**的滚动平均 —— 见 [`frameTransferAvgMs`]。
 */
export let lastFrameTransferMs = 0;

/**
 * **真实帧**（非空响应）的传输耗时滚动平均（ms）。空响应不参与统计 ——
 * 否则"没有帧"时的 0.2ms 会混进来，把真实开销稀释成假象。
 */
export let frameTransferAvgMs = 0;

/** 已统计进 [`frameTransferAvgMs`] 的真实帧数（0 = 还没量到）。 */
export let frameTransferSamples = 0;

/**
 * 拉取一帧（仅当比 `sinceSeq` 新时才返回数据，否则空 `ArrayBuffer`）。
 *
 * ⚠️ **不要改回 `glass://` 自定义协议**。真机实测该通道在约 10～19 次响应后
 * 会让 WebView2 渲染进程的 JS 整体停止执行（进程存活但定时器全静默），
 * 导致渲染端永远拿不到帧。与响应体积无关，只与响应次数有关。
 * 详见 `FIX-PLAN.md` §4.0.2.3 与 `commands::pull_frame`。
 */
export async function pullFrame(sinceSeq: number): Promise<ArrayBuffer> {
  // ⚠️ 必须做**类型归一化**：Tauri 的 IPC 在**不同传输路径**下，`tauri::ipc::Response`
  // 的 raw body 到前端会变成不同的 JS 类型：
  //   - 自定义协议 / fetch 路径 → `ArrayBuffer`
  //   - postMessage（brownfield）路径 → `Uint8Array` 视图，甚至 JSON 化的**数字数组**
  //     （`Vec<u8>` 被 serde 成 `[1,2,3,…]`）。
  // 旧实现直接把结果丢给 `new DataView(x)`，在 postMessage 路径下**每帧必抛**
  //   "First argument to DataView constructor must be an ArrayBuffer"
  // ——被 poll 的 catch 吞掉后表现为 status=error、`rendered` 永远为 0：
  // 画布从未更新，用户看到的就是"光学效果全没了"。
  // ⚠️ 这个 bug 浏览器 harness **测不出来**（它用自己的桩返回 ArrayBuffer），
  // 是交付物本体的 `HSDR_STATUS_DUMP` 自检把这条错误串抓出来的 —— 记住这个教训。
  const t0 = performance.now();
  const v = await invoke<unknown>("pull_frame", { sinceSeq });
  // 记录形态供诊断面板用（见 lastFrameTransfer 的说明）。
  lastFrameTransfer = v instanceof ArrayBuffer
    ? `ArrayBuffer(${v.byteLength}B)`
    : ArrayBuffer.isView(v)
      ? `${(v as ArrayBufferView).constructor.name}(${(v as ArrayBufferView).byteLength}B)`
      : Array.isArray(v)
        ? `Array(${(v as unknown[]).length}) ← JSON 数字数组，偏慢`
        : typeof v === "string"
          ? `base64 string(${(v as string).length})`
          : v == null
            ? "null/空"
            : Object.prototype.toString.call(v);
  let out: ArrayBuffer;
  if (v instanceof ArrayBuffer) {
    out = v;
  } else if (ArrayBuffer.isView(v)) {
    // 显式拷成独立 ArrayBuffer（视图可能只是更大缓冲区的片段；也避开 SAB 的类型问题）。
    const view = v as ArrayBufferView;
    const copy = new Uint8Array(view.byteLength);
    copy.set(new Uint8Array(view.buffer as ArrayBuffer, view.byteOffset, view.byteLength));
    out = copy.buffer;
  } else if (Array.isArray(v)) {
    // JSON 化后的数字数组（postMessage 路径的旧形态）。
    out = Uint8Array.from(v as number[]).buffer;
  } else if (typeof v === "string") {
    // base64 字符串 —— 当前生产路径（见 commands.rs::pull_frame 的说明）。
    if (v.length === 0) {
      out = new ArrayBuffer(0);
    } else {
      const bin = window.atob(v);
      const bytes = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i += 1) bytes[i] = bin.charCodeAt(i);
      out = bytes.buffer;
    }
  } else if (v == null) {
    out = new ArrayBuffer(0); // 无新帧
  } else {
    // 未知形态：**报出真实类型**而不是静默当空帧 —— 否则又会变成"没有任何信号的失效"。
    throw new Error(`pull_frame 返回了未预期的类型：${Object.prototype.toString.call(v)}`);
  }
  lastFrameTransferMs = performance.now() - t0;
  // 只有**非空帧**才计入滚动平均（空响应会把真实开销稀释掉）。
  if (out.byteLength > 0) {
    frameTransferSamples += 1;
    frameTransferAvgMs =
      frameTransferSamples === 1
        ? lastFrameTransferMs
        : frameTransferAvgMs * 0.8 + lastFrameTransferMs * 0.2;
  }
  return out;
}

/**
 * v0.3：面板窗口上报玻璃渲染链路状态。
 *
 * 调用方（`liquidGlass.ts`）已按 ~4Hz 节流，这里不再重复节流。失败静默——
 * 诊断上报本身不该影响主流程。
 */
export function reportGlassStatus(status: GlassStatusSnapshot): Promise<void> {
  return invoke<void>("report_glass_status", { status });
}

/** v0.3：读取面板窗口上报的渲染端状态（诊断窗口用）。 */
export function getGlassStatus(): Promise<GlassStatusSnapshot> {
  return invoke<GlassStatusSnapshot>("get_glass_status");
}

/** v0.3：打开（或聚焦）诊断窗口。 */
export function openDiagWindow(): Promise<void> {
  return invoke<void>("open_diag_window");
}

/** v0.3：关闭诊断窗口。 */
export function closeDiagWindow(): Promise<void> {
  return invoke<void>("close_diag_window");
}

// ---------------------------------------------------------------------------
// v0.3：Toast（独立窗口）
// ---------------------------------------------------------------------------

/** 提示级别，与 `ui/toastOverlay.ts` 的配色一一对应。 */
export type ToastLevel = "info" | "warn" | "error";

/**
 * 弹出提示。
 *
 * 走**独立窗口**而不是在胶囊内渲染：悬浮物窗口只有 68×228 逻辑像素、
 * 胶囊宽 40px 且 `overflow: hidden`，在那里放提示条一行读不到六个汉字。
 */
export function showToast(
  message: string,
  level: ToastLevel = "info",
  durationMs = 4200,
): Promise<void> {
  return invoke<void>("show_toast", { message, level, durationMs });
}

/** 立刻收起 Toast 窗口。 */
export function hideToastWindow(): Promise<void> {
  return invoke<void>("hide_toast_window");
}

/** 一条 Toast 的载荷（`commands::ToastPayload`）。 */
export interface ToastPayload {
  /** 自增序号，用于去重（事件与拉取会同时到达同一条）。 */
  id: number;
  message: string;
  level: ToastLevel;
  durationMs: number;
}

/**
 * Toast 窗口订阅载荷。
 *
 * ⚠️ 首次弹出时窗口是**刚创建**的，`show()` 与 `emit_to` 几乎同时发生，
 * 而本页 JS 还没执行到 `listen()` —— 这一次事件必然丢失。所以必须配合
 * [`getToastPayload`] 在挂载时补拉一次，并按 `id` 去重。
 */
export function onToastItem(cb: (payload: ToastPayload) => void): Promise<() => void> {
  return listen<ToastPayload>("toast:item", (event) => cb(event.payload));
}

/** 拉取最近一条 Toast 载荷（补首次创建时的事件丢失）。 */
export function getToastPayload(): Promise<ToastPayload | null> {
  return invoke<ToastPayload | null>("get_toast_payload");
}

// ---------------------------------------------------------------------------
// 事件订阅
// ---------------------------------------------------------------------------

/** 面板显示事件（Rust `window.rs` 在 show 后推送，载荷为空）。 */
export function onPanelShown(cb: () => void): Promise<() => void> {
  return listen("panel:shown", () => cb());
}

/** v2：悬浮物贴边状态变化（`edge.rs` / `window.rs` 推送）。 */
export function onWidgetState(cb: (s: WidgetState) => void): Promise<() => void> {
  return listen<WidgetState>("widget:state", (event) => cb(event.payload));
}

/** v2：拖拽接近屏边提示（`window.rs` 推送，驱动吸附引导线）。 */
export function onDragHint(cb: (h: DragHint) => void): Promise<() => void> {
  return listen<DragHint>("widget:drag_hint", (event) => cb(event.payload));
}

/** v2：悬浮物展开完成（`edge.rs` / `window.rs` 推送，强制刷新读数）。 */
export function onWidgetShown(cb: () => void): Promise<() => void> {
  return listen("widget:shown", () => cb());
}

/** 显示器状态变化（Rust `commands::emit_monitors` 推送）。 */
export function onMonitorsChanged(cb: (monitors: MonitorInfo[]) => void): Promise<() => void> {
  return listen<MonitorInfo[]>("monitors:changed", (event) => cb(event.payload));
}

/** 全局 Toast（Rust `hotkey.rs` 推送，`[code, message]` 元组）。 */
export function onGlobalToast(cb: (code: string, message: string) => void): Promise<() => void> {
  return listen<[string, string]>("toast:show", (event) => cb(event.payload[0], event.payload[1]));
}

// ---------------------------------------------------------------------------
// 错误统一出口
// ---------------------------------------------------------------------------

/** 把 `invoke` 抛出的错误规范化成 `CommandError`。 */
export function toCommandError(e: unknown): CommandError {
  if (e && typeof e === "object") {
    const o = e as Record<string, unknown>;
    const code = typeof o.code === "string" ? o.code : "ERROR";
    const message = typeof o.message === "string" ? o.message : String(e);
    return { code, message, detail: o.detail };
  }
  return { code: "ERROR", message: String(e) };
}

/** 执行命令并把失败转成统一错误；null 表示成功。 */
export async function run<T>(op: () => Promise<T>): Promise<{ ok: true; value: T } | { ok: false; error: CommandError }> {
  try {
    const value = await op();
    return { ok: true, value };
  } catch (e) {
    return { ok: false, error: toCommandError(e) };
  }
}
