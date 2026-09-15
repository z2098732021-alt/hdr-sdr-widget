//! UI 入口：装配悬浮物（widget.ts）+ 订阅 Rust 全局事件。
//!
//! v2 悬浮物形态下主窗口即胶囊；`widget.ts` 内部自行订阅
//! `widget:state` / `widget:drag_hint` / `widget:shown`，
//! 本文件只订阅与 UI 形态无关的全局事件（toast:show）。

import "./styles/tokens.css";
import "./styles/widget.css";

import { mountWidget } from "./ui/widget";
import { onGlobalToast } from "./bridge";

const app = document.getElementById("app");
if (!app) {
  throw new Error("缺少 #app 根容器");
}

const widget = mountWidget(app);

// Rust 侧推送的全局 Toast（热键冲突 / 解析失败等）。
void onGlobalToast((code, message) => {
  widget.onGlobalToast(code, message);
});
