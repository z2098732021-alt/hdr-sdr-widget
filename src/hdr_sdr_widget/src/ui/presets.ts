//! 预设弹层：长按呼出三档（白天 / 观影 / 夜间）竖排覆盖胶囊（P1-5）。
//!
//! 由于悬浮物窗口固定 44×220，弹层以胶囊内覆盖层呈现：长按数值区
//! 500ms 呼出，点选应用并收起，点胶囊其他区域收起。

import { icon } from "./icons";
import type { Preset } from "../bridge";

export interface PresetPopupApi {
  setPresets(list: Preset[]): void;
  show(): void;
  hide(): void;
  isOpen(): boolean;
  onApply(cb: (id: string) => void): void;
}

/** 长按呼出阈值（毫秒）。 */
export const PRESET_HOLD_MS = 500;

/**
 * 创建预设弹层，渲染进 `container`（应为 `.capsule`）。
 */
export function createPresetPopup(container: HTMLElement): PresetPopupApi {
  const pop = document.createElement("div");
  pop.className = "preset-pop";
  pop.setAttribute("role", "menu");
  container.appendChild(pop);

  let applyCb: (id: string) => void = () => {};
  let presets: Preset[] = [];
  let open = false;

  function render(): void {
    pop.replaceChildren();
    for (const p of presets) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.setAttribute("role", "menuitem");
      btn.title = `${p.name} ${p.percent}%`;
      btn.innerHTML = `${icon(p.iconId || "sun")}<span class="preset-name">${p.name}</span>`;
      btn.addEventListener("click", (e) => {
        e.stopPropagation();
        applyCb(p.id);
        hide();
      });
      pop.appendChild(btn);
    }
  }

  function show(): void {
    open = true;
    pop.classList.add("open");
  }

  function hide(): void {
    open = false;
    pop.classList.remove("open");
  }

  return {
    setPresets(list: Preset[]): void {
      presets = list;
      render();
    },
    show,
    hide,
    isOpen(): boolean {
      return open;
    },
    onApply(cb: (id: string) => void): void {
      applyCb = cb;
    },
  };
}
