//! 配置存储层：`AppSettings` 定义与 JSON 持久化。
//!
//! 放在独立 `store` 模块（而非塞进 lib 的 `core`），因为它只服务主程序
//! （Tauri 侧）的配置读写，lib 刻意不依赖它。

pub mod settings;
