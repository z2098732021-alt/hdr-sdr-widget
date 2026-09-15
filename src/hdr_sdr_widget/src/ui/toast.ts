//! toast.ts —— 提示的门面（面板侧唯一入口）。
//!
//! # v0.3 变更
//!
//! 旧实现在**胶囊内部**渲染提示：`mountToast(.toast-host)`，而 `.capsule`
//! 宽 40 逻辑像素、`overflow: hidden`，提示条字号 9px。结果是整条链路
//! "看起来实现了"，实际一个字都读不出来 —— 所以 `widget.ts` 干脆把 Rust
//! 推来的热键冲突提示 `void` 掉了，缺陷被静默掩盖。
//!
//! 现在改为转发到独立 Toast 窗口（`ui/toastOverlay.ts` 负责渲染，Rust
//! `show_toast` 负责创建 / 定位 / 收起）。本文件只保留一个薄门面，
//! 让调用方不必关心提示最终显示在哪里。

import { toCommandError, showToast, type ToastLevel } from "../bridge";

/** 展示一条提示。`message` 为已格式化的用户可读中文。 */
export function toast(message: string, level: ToastLevel = "info", durationMs = 4200): void {
  void showToast(message, level, durationMs).catch(() => {
    /* 提示失败不能反过来影响主流程 */
  });
}

/** 以 `CommandError` 展示错误提示。 */
export function toastError(err: unknown): void {
  const e = toCommandError(err);
  // 错误多留一会儿：用户往往需要读完再决定要不要开诊断面板。
  toast(e.message, "error", 6000);
}
