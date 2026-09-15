//! L1 原生层：Win32 DisplayConfig API 的 safe 封装。
//!
//! **边界规则**：整个 crate 里所有 `unsafe`、所有 `extern "system"`、所有原始
//! 指针只允许出现在本目录。对外只暴露 safe 函数，返回值统一为
//! `Result<_, AppError>`。
//!
//! 风险 R2（写入取值粒度未知）与 R3（适配器句柄易失）都收敛在这里处理：
//! - 每次写操作前调用 [`display::rebind`] 重新解析稳定键 → 易失句柄；
//! - 写入后由 [`crate::core::controller`] 做回读校验与吸附纠正。

pub mod capture;
pub mod display;
pub mod dwm_fallback;
pub mod ffi;
pub mod fullscreen;
pub mod geometry;
pub mod hit_test;
