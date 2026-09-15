//! L2 逻辑层：单位换算、读写编排、显示变化监听、注册表只读旁证。
//!
//! 本层**不直接触碰任何 unsafe Win32 细节**，只依赖 [`crate::win32`] 暴露的
//! safe 函数，因此全部逻辑都可以脱离硬件做单元测试。

pub mod controller;
pub mod convert;
pub mod model;
pub mod monitor_watch;
pub mod registry;
