//! 内联单色扁平 SVG 图标集。
//!
//! 统一规格：`viewBox="0 0 24 24"`，`stroke="currentColor"`、
//! `fill="none"`、`stroke-width="1.6"`、`round` 端点 / 连接。
//! **禁止 emoji**（验收标准 S4.2 可 grep 校验）。

/// 图标原始 SVG 标记（不含 `<svg>` 根）。
const PATHS: Record<string, string> = {
  sun: `
    <circle cx="12" cy="12" r="4.2"/>
    <path d="M12 2.6v2.2M12 19.2v2.2M2.6 12h2.2M19.2 12h2.2
             M5.3 5.3l1.55 1.55M17.15 17.15l1.55 1.55
             M18.7 5.3l-1.55 1.55M6.85 17.15l-1.55 1.55"/>`,
  moon: `
    <path d="M20.4 14.6A8.6 8.6 0 0 1 9.4 3.6a8.6 8.6 0 1 0 11 11z"/>`,
  film: `
    <rect x="3" y="4.5" width="18" height="15" rx="2.5"/>
    <path d="M7.5 4.5v15M16.5 4.5v15M3 9.3h4.5M3 14.7h4.5
             M16.5 9.3H21M16.5 14.7H21"/>`,
  monitor: `
    <rect x="2.6" y="4.4" width="18.8" height="12.6" rx="2.2"/>
    <path d="M9 20.4h6M12 17v3.4"/>`,
  settings: `
    <circle cx="12" cy="12" r="3"/>
    <path d="M12 2.8v2.2M12 19v2.2M2.8 12H5M19 12h2.2
             M5.6 5.6l1.6 1.6M16.8 16.8l1.6 1.6M18.4 5.6l-1.6 1.6M7.2 16.8l-1.6 1.6"/>`,
  close: `
    <path d="M5.5 5.5l13 13M18.5 5.5l-13 13"/>`,
  plus: `
    <path d="M12 5v14M5 12h14"/>`,
  minus: `
    <path d="M5 12h14"/>`,
  warning: `
    <path d="M12 3.4 22 20.4H2z"/>
    <path d="M12 9.6v5M12 16.9v.1"/>`,
  info: `
    <circle cx="12" cy="12" r="9"/>
    <path d="M12 10.8v6M12 7.1v.1"/>`,
  link: `
    <path d="M10 14a4.2 4.2 0 0 0 5.9 0l3.6-3.6a4.2 4.2 0 0 0-5.9-5.9l-1.7 1.7
             M14 10a4.2 4.2 0 0 0-5.9 0L4.5 13.6a4.2 4.2 0 0 0 5.9 5.9l1.7-1.7"/>`,
};

/// 渲染一个图标 SVG 字符串。
///
/// @param name   图标名（`PATHS` 的键）。
/// @param cls    附加的 CSS 类（默认 `icon`）。
export function icon(name: string, cls = "icon"): string {
  const body = PATHS[name];
  if (!body) {
    // 未知图标：渲染一个占位圆点，避免整块 UI 崩掉。
    return `<svg class="${cls}" viewBox="0 0 24 24" fill="none"
      stroke="currentColor" stroke-width="1.6" stroke-linecap="round"
      stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="3"/></svg>`;
  }
  return `<svg class="${cls}" viewBox="0 0 24 24" fill="none"
    stroke="currentColor" stroke-width="1.6" stroke-linecap="round"
    stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}

/// 列出全部可用图标名（诊断 / 测试用）。
export function iconNames(): string[] {
  return Object.keys(PATHS);
}
