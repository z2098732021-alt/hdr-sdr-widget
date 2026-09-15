//! 垂直滑块：底 0% → 顶 100%，拇指 1:1 拖调值 + 三位数字滚动柱。
//!
//! 对齐 ARCHITECTURE.md T04-S4.3：
//! - 拖动 1:1 直写（无缓动），整数变化才上报（去重）；
//! - `Adjusted` 磁吸 / `Failed` 回弹由调用方（widget.ts）处理后 `setPercent`；
//! - 数值滚动 80ms/档 + `--spring-pop` overshoot（P0-9）。

/** 拖拽回调（由调用方做节流 / 写盘 / 磁吸处理）。 */
export interface ThumbSliderCallbacks {
  /** 拖拽中值变化（整数去重后上报）。 */
  onDrag(percent: number): void;
  /** 松手提交最终值。 */
  onCommit(percent: number): void;
}

export interface ThumbSliderApi {
  /** 设置视觉值（无缓动直接落位，用于初始化 / 磁吸 / 回弹）。 */
  setPercent(percent: number): void;
  /** 启用 / 禁用（HDR 关闭 / 只读时禁用拖拽）。 */
  setEnabled(on: boolean): void;
  /** 当前内部值。 */
  getPercent(): number;
}

const TRACK_MAX_H = 152; // 与 widget.css `.track` 高度对齐的参考值（实际以测量为准）

/**
 * 创建垂直滑块，渲染进 `container`。
 * `container` 需为 `.slider-view`（内含 value-row / track-area 的空骨架）。
 */
export function createThumbSlider(
  container: HTMLElement,
  callbacks: ThumbSliderCallbacks,
): ThumbSliderApi {
  const track = container.querySelector<HTMLElement>(".track");
  const fill = container.querySelector<HTMLElement>(".track-fill");
  const thumb = container.querySelector<HTMLElement>(".thumb");
  const valueRoll = container.querySelector<HTMLElement>(".value-roll");
  if (!track || !fill || !thumb || !valueRoll) {
    throw new Error("thumbSlider 初始化失败：缺少轨道/拇指/数值节点");
  }
  // 重新绑定为收窄后的非空引用，供嵌套闭包使用（TS 不会在闭包内保持收窄）。
  const trackEl = track;
  const fillEl = fill;
  const thumbEl = thumb;
  const valueRollEl = valueRoll;

  let percent = 0;
  let enabled = true;
  let lastReported = -1;
  let dragging = false;

  // 三位数字柱：每个 `.digit` 内竖排 0-9，按档位 translateY(-d em)。
  for (let i = 0; i < 3; i++) {
    const col = document.createElement("div");
    col.className = "digit";
    for (let d = 0; d <= 9; d++) {
      const span = document.createElement("span");
      span.textContent = String(d);
      col.appendChild(span);
    }
    valueRollEl.appendChild(col);
  }
  const cols = Array.from(valueRollEl.querySelectorAll<HTMLElement>(".digit"));

  function setDigit(col: HTMLElement, digit: number): void {
    col.style.transform = `translateY(-${digit}em)`;
  }

  /** 视觉落位：拇指 / 填充 / 数字柱。 */
  function render(): void {
    const trackH = trackEl.clientHeight || TRACK_MAX_H;
    const fillH = (percent / 100) * trackH;
    fillEl.style.height = `${fillH}px`;
    thumbEl.style.bottom = `${fillH}px`;
    thumbEl.setAttribute("aria-valuenow", String(Math.round(percent)));

    const p = Math.round(percent);
    setDigit(cols[0]!, Math.floor(p / 100) % 10);
    setDigit(cols[1]!, Math.floor(p / 10) % 10);
    setDigit(cols[2]!, p % 10);
  }

  /** pointer 位置 → 百分比（底 0% → 顶 100%）。 */
  function toPercent(clientY: number): number {
    const rect = trackEl.getBoundingClientRect();
    const y = clientY - rect.top;
    const pct = (1 - y / rect.height) * 100;
    return Math.min(100, Math.max(0, Math.round(pct)));
  }

  function onPointerDown(e: PointerEvent): void {
    if (!enabled) return;
    dragging = true;
    thumbEl.classList.add("dragging");
    thumbEl.setPointerCapture(e.pointerId);
    e.preventDefault();
    apply(e.clientY);
  }

  function onPointerMove(e: PointerEvent): void {
    if (!dragging) return;
    apply(e.clientY);
  }

  function onPointerUp(e: PointerEvent): void {
    if (!dragging) return;
    dragging = false;
    thumbEl.classList.remove("dragging");
    try {
      thumbEl.releasePointerCapture(e.pointerId);
    } catch {
      // pointercancel 后 capture 已被浏览器自动释放，此处可能抛
      // NotFoundError，不阻塞调值提交。
    }
    if (percent !== lastReported) {
      lastReported = percent;
      callbacks.onCommit(percent);
    }
  }

  function apply(clientY: number): void {
    const next = toPercent(clientY);
    if (next === percent) return;
    percent = next;
    render();
    if (percent !== lastReported) {
      lastReported = percent;
      callbacks.onDrag(percent);
    }
  }

  thumbEl.addEventListener("pointerdown", onPointerDown);
  thumbEl.addEventListener("pointermove", onPointerMove);
  thumbEl.addEventListener("pointerup", onPointerUp);
  thumbEl.addEventListener("pointercancel", onPointerUp);

  // 初始渲染。
  render();

  return {
    setPercent(p: number): void {
      percent = Math.min(100, Math.max(0, Math.round(p)));
      render();
    },
    setEnabled(on: boolean): void {
      enabled = on;
      thumbEl.style.cursor = on ? "grab" : "not-allowed";
    },
    getPercent(): number {
      return percent;
    },
  };
}
