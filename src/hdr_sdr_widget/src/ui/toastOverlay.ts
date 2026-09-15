//! toastOverlay.ts —— Toast 窗口的内容渲染。
//!
//! 与 `ui/toast.ts`（面板侧门面）成对：门面负责"把消息发出去"，本模块负责
//! "把消息显示出来"。窗口本身由 Rust 创建、定位、定时收起；本模块只做渲染
//! 与点击收起。
//!
//! 不做队列：同一条 Toast 反复弹出时直接替换内容。因为窗口位置贴着胶囊，
//! 堆叠会盖住悬浮物本身。
//!
//! # 首帧竞态
//!
//! Toast 窗口是**按需创建**的。第一次弹出时 Rust 侧 `show()` 与 `emit_to`
//! 紧接着发生，而这里 `listen()` 还没注册 —— 那次事件必然丢失，用户会看到
//! 一个空白窗口。所以挂载时先拉一次 `getToastPayload`，并用 `id` 去重，
//! 保证同一条只渲染一次。

import {
  getToastPayload,
  hideToastWindow,
  onToastItem,
  type ToastPayload,
} from "../bridge";

/** 单条 Toast 的停留上限（ms），与服务端定时收起的兜底值一致。 */
const MAX_HOLD_MS = 15000;

export function mountToastOverlay(root: HTMLElement): void {
  let card: HTMLElement | null = null;
  let hideTimer = 0;
  /** 已渲染过的载荷序号：事件与拉取可能送来同一条。 */
  let lastId = -1;

  function teardown(): void {
    if (hideTimer !== 0) {
      window.clearTimeout(hideTimer);
      hideTimer = 0;
    }
    const el = card;
    if (!el) return;
    card = null;
    el.classList.add("leaving");
    window.setTimeout(() => el.remove(), 160);
  }

  function show(payload: ToastPayload): void {
    // 去重：同一条不重复渲染（重渲染会打断入场动画，看起来像闪一下）。
    if (payload.id === lastId) return;
    lastId = payload.id;
    teardown();

    const el = document.createElement("div");
    el.className = "toast-card";
    el.dataset.level = payload.level;

    const bar = document.createElement("div");
    bar.className = "toast-bar";

    const msg = document.createElement("div");
    msg.className = "toast-msg";
    // 用 textContent 而不是 innerHTML：这些文本来自后端错误信息，
    // 里面可能带用户可控的显示器名称 / 文件路径。
    msg.textContent = payload.message;

    const hint = document.createElement("div");
    hint.className = "toast-hint";
    hint.textContent = "点击收起";

    el.append(bar, msg, hint);
    el.addEventListener("click", () => {
      void hideToastWindow().catch(() => {});
      teardown();
    });

    root.replaceChildren(el);
    card = el;

    // 前端也上一道保险：万一 Rust 的定时线程没跑（比如窗口被关掉再重建），
    // 这里保证内容不会永远留在屏幕上。
    const hold = Math.min(Math.max(payload.durationMs, 1200), MAX_HOLD_MS);
    hideTimer = window.setTimeout(() => {
      void hideToastWindow().catch(() => {});
      teardown();
    }, hold);
  }

  void onToastItem(show);

  // 补拉：窗口刚创建时收到的那条事件已经错过了（见模块头注释）。
  void getToastPayload()
    .then((payload) => {
      if (payload) show(payload);
    })
    .catch(() => {
      /* 拉不到就等事件；两者都失败才是真问题 */
    });
}
