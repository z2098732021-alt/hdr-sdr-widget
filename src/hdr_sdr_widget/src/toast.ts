//! Toast 窗口入口。

import "./styles/toastWin.css";

import { mountToastOverlay } from "./ui/toastOverlay";

const root = document.getElementById("toast-root");
if (!root) {
  throw new Error("缺少 #toast-root 根容器");
}

mountToastOverlay(root);
