//! widget.ts —— Liquid Glass 悬浮物控制器（v0.2.0 二版）
//!
//! 职责：
//! - 装配 DOM（背景捕获帧 + 玻璃胶囊光学层 + Fill + 反馈）；
//! - 手势路由（方向主导）：**横向拖动 = 移动窗口**，**纵向拖动 = 调值**，
//!   点击（无位移）= 跳值——不再需要精准点边框；
//! - 状态镜像：Rust 状态机为唯一真源，`data-*` 仅作渲染；
//! - 亮度读写：Fill 的 0–1 抽象值映射到 SDR 内容亮度（无图标 / 无数字 / 无文字）。

import { mountGlass, type GlassApi } from "./liquidGlass";
import { toast, toastError } from "./toast";
import {
  applyPercent,
  endWindowDrag,
  getSettings,
  getWidgetState,
  listMonitors,
  moveWindow,
  onDragHint,
  onMonitorsChanged,
  onWidgetShown,
  onWidgetState,
  openHdrSettings,
  readBrightness,
  setPointerPhase,
  type DragHint,
  type MonitorInfo,
  type WidgetState,
} from "../bridge";

/** 拖动判定阈值（CSS px）：超过才从「待定」进入拖拽。 */
const DRAG_THRESHOLD = 6;
/** 胶囊在窗口内的固定位置/尺寸（与 tokens.css 一致，用于稳定 value 映射）。 */
const CAPSULE_TOP = 14;
const CAPSULE_H = 200;
/** 值写入最小间隔（ms）。窗口内合并、窗口外补发一次，见 `writePercent`。 */
const WRITE_INTERVAL_MS = 30;

/**
 * 选定目标显示器。
 *
 * 优先用配置里记住的那台（`lastMonitorKey`，Rust 侧 `resolve_target` 用的是
 * 同一个键），再退到主屏 / 第一台。
 *
 * 旧实现无条件取主屏 —— 副屏用户每次启动都被拉回主屏，改的是自己没在看的
 * 那块屏的亮度。而 Rust 侧 `current_key` 反而是记住的，两边各说各话：
 * 滑条显示的是 B 屏读数，拖动的却写进了 A 屏。
 */
function pickTarget(monitors: MonitorInfo[], rememberedKey: string): MonitorInfo | null {
  if (rememberedKey) {
    const hit = monitors.find((m) => m.key === rememberedKey);
    if (hit) return hit;
  }
  return monitors.find((m) => m.isPrimary) ?? monitors[0] ?? null;
}

/**
 * 相位是否属于"可见/过渡"（应当高频取帧）。
 *
 * 抽成函数而不是内联条件，是因为它现在只影响**取帧频率**，不再影响
 * "循环是否活着" —— 后者是 v0.3.1 修掉的根因（见 liquidGlass.ts 的 `visible`）。
 */
function isVisiblePhase(phase: string): boolean {
  return (
    phase === "revealing" ||
    phase === "visible" ||
    phase === "hovered" ||
    phase === "pressed" ||
    phase === "dragging"
  );
}

export interface WidgetController {
  refresh(): Promise<void>;
  onGlobalToast(code: string, message: string): void;
}

export function mountWidget(app: HTMLElement): WidgetController {
  app.innerHTML = `
    <div class="capsule" data-phase="hidden" data-docked="false" data-expanded="false"
         data-hover="false" data-pressed="false" data-dragging="false" data-hdr="on">
      <div class="capsule-clip">
        <canvas class="backdrop"></canvas>
        <div class="fill-clip">
          <div class="fill"></div>
        </div>
        <div class="glass-tint"></div>
        <div class="glass-rim"></div>
        <div class="press-light"></div>
        <div class="hdr-dot"></div>
      </div>
    </div>`;
  //
  // v0.3.2 高透全透：删掉了 `.glass-sheen`（对角高光，硬编码 0.14 白、无令牌、且明显发白）
  // 与三层 `.glass-bloom`（HDR 泛光 —— 外圈大光晕是"假"的主因）。
  // ⚠️ 还原光学效果**不要**把这些 DOM 层加回来：用户要的四个光学元素（折射 / 色散 /
  // 迎光侧细高光 / 内侧厚度暗边）全部由 `tokens.css` 的令牌驱动，见 `liquidGlass.ts`。

  const capsule = app.querySelector<HTMLElement>(".capsule")!;
  const backdrop = app.querySelector<HTMLCanvasElement>(".backdrop")!;
  const fill = app.querySelector<HTMLElement>(".fill")!;
  const pressLight = app.querySelector<HTMLElement>(".press-light")!;
  const hdrDot = app.querySelector<HTMLElement>(".hdr-dot")!;

  // 提示不再挂在这里：胶囊宽 40px 塞不下可读文本，提示改走独立 Toast 窗口
  // （`ui/toast.ts` → Rust `show_toast`）。
  const glass: GlassApi = mountGlass({ capsule, backdrop, fill, pressLight, hdrDot });

  // 取帧链**在挂载时无条件启动，此后常驻**（相位只切频率，不停循环）。
  // 旧实现把启动权完全交给相位事件，而 `window::show()` 在 Tauri setup 里
  // （进程启动 ~0ms）就广播了相位，页面几百毫秒后才加载完 —— 那次事件永久丢失，
  // 循环于是从未启动。真机实测：采集端 DDA 取得 245 帧、拷贝 245 次全成功，
  // 前端 `polls = 0`、`rendered = 0`，胶囊没有任何背景内容、近乎不可见。
  glass.startFrameLoop();

  // ---- 运行时状态 ----
  let currentKey = "";
  let hdrEnabled = true;
  let lastWriteAt = 0;
  /** 节流窗口内最后一次目标值（窗口结束后补发，保证松手时的值一定落地）。 */
  let pendingPct: number | null = null;
  let writeTimer = 0;

  // ---- 手势状态 ----
  let pointerMode: "none" | "pending" | "value" | "window" = "none";
  let startScreenX = 0;
  let startScreenY = 0;
  let lastScreenX = 0;
  let lastScreenY = 0;
  let pendingDx = 0;
  let pendingDy = 0;
  let framePending = false;

  // ---- 值写入（leading + trailing 节流） ----
  /**
   * 把待发值真正写下去。
   *
   * 必须区分"节流窗口内被压掉的值"和"已经写下去的值"：旧实现直接
   * `if (now - lastWriteAt < 30) return;`，于是**最后一段拖动被整段丢弃** ——
   * 松手时滑条停在 62%，屏幕却停在上一次成功写入的 55%。用户看到的是
   * "松手后亮度对不上"，而且因为 30ms 太短，复现全看运气。
   */
  function flushWrite(): void {
    if (writeTimer !== 0) {
      window.clearTimeout(writeTimer);
      writeTimer = 0;
    }
    if (pendingPct === null) return;
    const pct = pendingPct;
    pendingPct = null;
    lastWriteAt = Date.now();
    if (!currentKey || !hdrEnabled) return;
    void applyPercent(currentKey, pct).then(r => { if (r.kind === "failed") toastError({ message: r.message }); }).catch((e) => toastError(e));
  }

  /** 写值（leading 立即，trailing 补发最后一次）。 */
  function writePercent(pct: number): void {
    const elapsed = Date.now() - lastWriteAt;
    pendingPct = pct;
    if (elapsed >= WRITE_INTERVAL_MS) {
      flushWrite();
      return;
    }
    if (writeTimer === 0) {
      writeTimer = window.setTimeout(flushWrite, WRITE_INTERVAL_MS - elapsed);
    }
  }

  // value 用固定常量（clientY 相对窗口，窗口在 value 拖动期间不移动，稳定）
  function pointerToPercent(e: PointerEvent): number {
    const y = e.clientY - CAPSULE_TOP;
    return Math.max(0, Math.min(100, Math.round((1 - y / CAPSULE_H) * 100)));
  }

  // ---- 统一手势：待定 → 横向=移动窗口 / 纵向=调值 / 无位移=点击跳值 ----
  function onPointerDown(e: PointerEvent): void {
    if (pointerMode !== "none") return;
    pointerMode = "pending";
    startScreenX = e.screenX;
    startScreenY = e.screenY;
    lastScreenX = e.screenX;
    lastScreenY = e.screenY;
    pendingDx = 0;
    pendingDy = 0;
    capsule.dataset.pressed = "true";
    glass.showPress(e.clientX - CAPSULE_TOP, e.clientY - CAPSULE_TOP);
    void setPointerPhase("pressed");
    capsule.setPointerCapture(e.pointerId);
    capsule.addEventListener("pointermove", onMove);
    capsule.addEventListener("pointerup", onUp, { once: true });
    capsule.addEventListener("pointercancel", onUp, { once: true });
  }

  function onMove(e: PointerEvent): void {
    if (pointerMode === "none") return;
    if (pointerMode === "pending") {
      const dx = e.screenX - startScreenX;
      const dy = e.screenY - startScreenY;
      if (Math.abs(dx) + Math.abs(dy) < DRAG_THRESHOLD) return;
      // 方向主导：横向明显 → 移动窗口；否则 → 调值。
      pointerMode = Math.abs(dx) > Math.abs(dy) ? "window" : "value";
      capsule.dataset.dragging = "true";
      void setPointerPhase("dragging");
    }
    if (pointerMode === "window") {
      // 用屏幕坐标（绝对）：窗口移动后 clientX 基准会变，screenX 不会。
      pendingDx += e.screenX - lastScreenX;
      pendingDy += e.screenY - lastScreenY;
      lastScreenX = e.screenX;
      lastScreenY = e.screenY;
      if (framePending) return;
      framePending = true;
      requestAnimationFrame(() => {
        framePending = false;
        const dx = Math.round(pendingDx);
        const dy = Math.round(pendingDy);
        pendingDx = 0;
        pendingDy = 0;
        if (dx !== 0 || dy !== 0) {
          void moveWindow(dx, dy).catch(() => {});
        }
      });
    } else {
      // value：实时跟手。
      const pct = pointerToPercent(e);
      glass.setValue(pct, false);
      writePercent(pct);
    }
  }

  function onUp(e: PointerEvent): void {
    const mode = pointerMode;
    pointerMode = "none";
    capsule.removeEventListener("pointermove", onMove);
    capsule.removeEventListener("pointerup", onUp);
    capsule.removeEventListener("pointercancel", onUp);
    try {
      capsule.releasePointerCapture(e.pointerId);
    } catch {
      /* pointercancel 后 capture 已释放 */
    }

    if (mode === "pending") {
      // 点击（无位移）：跳值 + 距离自适应动画。
      const pct = pointerToPercent(e);
      const prevPct = parseFloat(fill.style.height || "0");
      const dist = Math.abs(pct - prevPct);
      if (hdrEnabled) {
        glass.setValue(pct, true, dist);
        writePercent(pct);
        flushWrite();
      } else {
        void openHdrSettings().catch(() => {});
      }
    } else if (mode === "value") {
      // 拖动结束：用抬起位置作为最终值，并强制落地（可能还在节流窗口内）。
      const pct = pointerToPercent(e);
      glass.setValue(pct, false);
      writePercent(pct);
      flushWrite();
    } else if (mode === "window") {
      void endWindowDrag().catch(() => {});
    }

    capsule.dataset.pressed = "false";
    capsule.dataset.dragging = "false";
    glass.hidePress();
    void setPointerPhase("visible");
  }

  capsule.addEventListener("pointerdown", onPointerDown);
  capsule.addEventListener("pointerenter", () => {
    // 手势进行中不覆盖相位。指针被 setPointerCapture 捕获后，指针移出再移回
    // 胶囊会重新触发 pointerenter —— 若此时正在拖动调值，就会把 Rust 状态机
    // 从 dragging 打回 hovered，松手时 end_window_drag 走不到、贴边吸附失效。
    if (pointerMode !== "none") return;
    capsule.dataset.hover = "true";
    void setPointerPhase("hovered");
  });
  capsule.addEventListener("pointerleave", () => {
    if (pointerMode === "none") {
      capsule.dataset.hover = "false";
      void setPointerPhase("visible");
    }
  });

  // ---- 状态镜像（Rust 唯一真源） ----
  function applyWidgetState(s: WidgetState): void {
    capsule.dataset.phase = s.phase;
    capsule.dataset.docked = s.docked === null ? "false" : "true";
    capsule.dataset.expanded = s.expanded ? "true" : "false";
    // 取帧频率随相位切换，但**循环常驻**（隐藏 1Hz 保活）。旧实现在此处
    // `stopFrameLoop()` 彻底停采样，漏掉一次"回到可见态"的事件就永久停摆 ——
    // 真机上表现为"采集端 245 帧全成功、前端 rendered = 0、胶囊没有背景内容"。
    glass.setVisible(isVisiblePhase(s.phase));

    // 正在调亮度 → **冻结背衬**。
    //
    // 亮度写的是显示器的 SDR 白电平，会把屏幕上的 SDR 内容整体提亮；而被捕捉的
    // 正是这块屏幕 —— 于是调亮的同时，玻璃里的背景、叠在其上的边缘高光、液面
    // 遮罩会**一起变亮**，内容像被白色糊住（用户实测原话）。
    // 胶囊的垂直拖拽就是调亮度，所以 Pressed / Dragging 正好覆盖"调整中"。
    glass.setHold(s.phase === "pressed" || s.phase === "dragging");
  }

  function onDragHintCb(h: DragHint): void {
    void h;
  }

  // ---- 亮度读数 ----
  async function refresh(): Promise<void> {
    try {
      const [monitors, settings] = await Promise.all([
        listMonitors(),
        // 配置读不到不致命：退回"主屏优先"即可，不影响亮度控制本身。
        getSettings().catch(() => null),
      ]);
      const target = pickTarget(monitors, settings?.lastMonitorKey ?? "");
      if (!target) return;
      currentKey = target.key;
      const reading = await readBrightness(target.key);
      hdrEnabled = reading.canControl;
      glass.setTransmission(reading.softwareTransmission, reading.captureSafe);
      glass.setHdr(hdrEnabled);
      glass.setValue(reading.percent, true);
    } catch {
      /* 读不到不阻塞主流程 */
    }
  }

  // ---- 事件订阅 ----
  void onWidgetState(applyWidgetState);
  void onWidgetShown(() => void refresh());
  void onDragHint(onDragHintCb);
  // 显示拓扑 / 系统 HDR 设置变化（`monitor_watch` 桥接线程推送）：重读当前屏。
  void onMonitorsChanged(() => void refresh());

  void getWidgetState().then(applyWidgetState).catch(() => {});
  void refresh();
  const brightnessPoll = window.setInterval(() => { if (pointerMode === "none") void refresh(); }, 1000);
  window.addEventListener("pagehide", () => window.clearInterval(brightnessPoll), { once: true });

  // ---- 相位保活（兜底重拉）----
  //
  // 为什么必须有：相位的**唯一**驱动是 `widget:state` 事件，而事件可能
  // **在 webview 订阅之前就发出** —— `window::show()` 在 Tauri setup 里执行
  // （进程启动后 ~0ms），而页面要多花几百毫秒才加载完并注册 listen，那一次
  // 广播就永久丢了。只靠"订阅 + 启动时拉一次"的话，一旦丢掉的恰好是
  // "回到可见态"那一次，帧循环就会永久停在 `stopFrameLoop` 状态。
  //
  // 症状极具误导性：**采集端一切正常**（真机实测 DDA 取得 245 帧、拷贝 245 次
  // 全成功、格式 87 正确），但前端 `rendered = 0` —— 画布从未被写入，胶囊没有
  // 背景内容，在深色桌面上近乎不可见。会让人误以为是光学参数不对或窗口没渲染。
  //
  // 每秒兜底重拉一次相位；`applyWidgetState` 对同值调用幂等
  // （`startFrameLoop` 内部有 `if (running) return`），所以成本可忽略，
  // 且不破坏"隐藏态零采样"的设计。
  const PHASE_KEEPALIVE_MS = 1000;
  window.setInterval(() => {
    void getWidgetState().then(applyWidgetState).catch(() => {
      /* 拉取失败不改状态，等下一次 */
    });
  }, PHASE_KEEPALIVE_MS);

  return {
    refresh,
    onGlobalToast(code, message) {
      // Rust 侧的热键冲突 / 解析失败提示。旧实现把这两个参数直接 `void` 掉，
      // 于是"热键被别的软件占了"这件事在 UI 上完全不可见——用户只知道
      // 按了没反应，只能猜。
      //
      // 不加操作按钮：当前还没有"热键设置"界面，放一个指向系统 HDR 页的
      // 按钮只会误导。冲突的解决办法（改 config 里的 hotkey）写在消息里。
      const level = code.startsWith("HOTKEY") ? "warn" : "error";
      toast(message, level, 6000);
    },
  };
}
