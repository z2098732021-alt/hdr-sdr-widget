//! `hdr_sdr_widget_lib` —— L1 原生层 + L2 逻辑层。
//!
//! 本库**刻意不依赖 Tauri**，从而让 `probe` 验证二进制可以在数秒内编译完成，
//! 不必等待 Tauri 的完整构建（首次约 5–8 分钟）。Tauri 相关代码只存在于
//! `src/main.rs`（主程序二进制）中。
//!
//! 模块分层：
//! - [`win32`]：L1 原生层，全部 `unsafe` 与 `extern "system"` 收敛于此。
//! - [`core`]：L2 逻辑层，不触碰 Win32 细节，纯逻辑可单测。
//! - [`error`]：统一错误类型与中文用户文案。

pub mod core;
pub mod error;
pub mod win32;
