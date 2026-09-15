//! Tauri 构建脚本。
//!
//! 职责：调用 `tauri_build::build()` 生成 Windows 资源文件（图标、版本信息、
//! 应用清单），并处理 Tauri 的代码生成。
//!
//! 注意：本脚本对本 crate 的**所有**目标（含 `probe` 验证二进制）都会执行。

fn main() {
    tauri_build::build()
}
