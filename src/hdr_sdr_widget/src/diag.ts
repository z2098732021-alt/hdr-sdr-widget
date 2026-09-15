//! 诊断窗口入口：装配诊断面板并启动轮询。
//!
//! 与 `main.ts`（悬浮物窗口）平行，两者互不影响，共用 `bridge.ts` 契约。

import "./styles/tokens.css";
import "./styles/diag.css";

import { mountDiag } from "./ui/diag";
import { mountNativeDiag } from "./ui/nativeDiag";

const root = document.getElementById("diag");
if (!root) {
  throw new Error("缺少 #diag 根容器");
}

void mountNativeDiag(root).then((native) => { if (!native) mountDiag(root); }).catch(() => mountDiag(root));
