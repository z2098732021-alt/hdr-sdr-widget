import { defineConfig } from "vite";
import { fileURLToPath } from "node:url";

// Tauri 2 标准前端构建配置：
// - clearScreen 关闭，避免 Tauri CLI 的输出被 Vite 清掉；
// - 固定端口 1420（与 src-tauri/tauri.conf.json 的 devUrl 对齐），strictPort 确保
//   端口被占用时直接报错而不是静默换端口（换端口会导致 Tauri 找不到前端）；
// - envPrefix 覆盖 Tauri 注入的 TAURI_ENV_* 环境变量；
// - 目标平台为 Windows 11 自带 WebView2，最低 Chromium 105 即满足 backdrop-filter。
//
// v0.3：多页面。除悬浮物窗口（index.html）外，诊断窗口是**独立的 webview**，
// 需要自己的 HTML 入口（diag.html）。两者同源，契约都走 bridge.ts。
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  // 前端源码根：index.html / diag.html 均位于 src/ 下。
  root: "src",
  // 防止 Vite 清屏打断 Tauri CLI 的进度输出。
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 监听 src-tauri 内的变更以触发 HMR（tauri.conf.json 改动除外）。
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    // 产物输出到项目根 dist/（tauri.conf.json 的 frontendDist 指向 ../dist）。
    outDir: "../dist",
    emptyOutDir: true,
    // Tauri 依赖 WebView2，目标定在 chrome105 即可覆盖 Windows 11 全量用户。
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      input: {
        // 悬浮物主窗口。
        main: fileURLToPath(new URL("./src/index.html", import.meta.url)),
        // 诊断面板窗口（托盘「诊断面板…」打开）。
        diag: fileURLToPath(new URL("./src/diag.html", import.meta.url)),
        // Toast 窗口（`show_toast` 命令创建）。
        toast: fileURLToPath(new URL("./src/toast.html", import.meta.url)),
      },
    },
  },
});
