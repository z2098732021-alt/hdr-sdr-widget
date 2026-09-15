//! 领域模型：显示器、HDR 状态、百分比、写入结果。
//!
//! 与 ARCHITECTURE.md §5.1 类图一一对应。

use serde::Serialize;

/// 与 Windows 系统设置滑块口径一致的百分比，取值 [0, 100]。
///
/// **这是本应用内部唯一的亮度真值**（架构决策 D1）。不用 raw、不用 nits：
/// `raw = 1000 + 50 × percent` 天然是 50 的倍数，直接规避写入对齐隐患。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Default)]
pub struct Percent {
    /// 百分比数值，恒在 [0, 100]。
    pub value: u8,
}

impl Percent {
    /// 下界：0%（80 nits / raw 1000）。
    pub const MIN: u8 = 0;
    /// 上界：100%（480 nits / raw 6000）。
    pub const MAX: u8 = 100;

    /// 构造一个百分比，超出范围会被钳位而不是 panic。
    #[must_use]
    pub fn new(value: u8) -> Self {
        Self { value: value.clamp(Self::MIN, Self::MAX) }
    }

    /// 从任意整数钳位构造。用于 `raw → percent` 这类可能产生 u32 的换算。
    #[must_use]
    pub fn clamp(value: u32) -> Self {
        if value >= u32::from(Self::MAX) {
            Self { value: Self::MAX }
        } else {
            #[allow(clippy::cast_possible_truncation)]
            Self { value: value as u8 }
        }
    }

    /// 往上下调整，返回新值；越界时钳位而不回绕。
    #[must_use]
    pub fn offset(self, delta: i16) -> Self {
        let base = i32::from(self.value) + i32::from(delta);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let clamped = base.clamp(i32::from(Self::MIN), i32::from(Self::MAX)) as u8;
        Self { value: clamped }
    }

    /// 是否处于下界（用于 UI 的"减号置灰"）。
    #[must_use]
    pub fn at_min(self) -> bool {
        self.value <= Self::MIN
    }

    /// 是否处于上界（用于 UI 的"加号置灰"）。
    #[must_use]
    pub fn at_max(self) -> bool {
        self.value >= Self::MAX
    }
}

impl std::fmt::Display for Percent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}%", self.value)
    }
}

/// 显示器目标的稳定描述。
///
/// `key`（设备实例路径）跨会话稳定；`adapter_id` + `target_id` 是**易失句柄**，
/// 休眠唤醒 / 拔插 / 驱动重装后都会变，因此每次写操作前都要重新解析（架构决策 D2）。
///
/// **不派生 `Eq`**：`refresh_hz: f64` 不实现 `Eq`；比较用 `PartialEq` 即可。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DisplayTarget {
    /// 稳定键 = `monitorDevicePath`，例如
    /// `\\?\DISPLAY#XMI3009#5&1e5a718c&0&UID4354#{e6f07b5f-...}`。
    pub key: String,
    /// 显示器友好名，例如 `Mi Monitor`。
    pub name: String,
    /// 在 `enumerate_targets()` 结果中的序号，从 0 开始。
    pub index: u32,
    /// 是否为主显示器。
    pub is_primary: bool,
    /// 输出接口类型码（5 = HDMI，10 = DisplayPort 外置 …）。
    pub output_tech: u32,
    /// 适配器 LUID 打包成的 u64，仅供日志与比较。
    pub adapter_luid: u64,
    /// 目标 ID，易失。
    pub target_id: u32,
    /// 刷新率（Hz），诊断用。
    pub refresh_hz: f64,
}

impl DisplayTarget {
    /// 该目标的句柄是否已过期。
    ///
    /// 判定方式：重新枚举当前活动路径，若稳定键已不存在，或键存在但
    /// (适配器, 目标 ID) 与快照不同，即视为过期。
    #[must_use]
    pub fn is_stale(&self) -> bool {
        match crate::win32::display::rebind(&self.key) {
            Ok(fresh) => {
                fresh.adapter_luid != self.adapter_luid || fresh.target_id != self.target_id
            }
            Err(_) => true,
        }
    }
}

/// 高级色彩（HDR）状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct HdrState {
    /// 显示器是否支持 HDR。
    pub supported: bool,
    /// HDR 当前是否已开启。
    pub enabled: bool,
    /// 每个颜色通道的位深（HDR 开启时通常为 10 或 12）。
    pub bits_per_color: u8,
}

impl HdrState {
    /// HDR 是否真正可用（既支持又已开启）。只有此状态下才允许写入亮度。
    #[must_use]
    pub fn is_usable(self) -> bool {
        self.supported && self.enabled
    }
}

/// 一台显示器的完整运行时状态。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DisplayState {
    /// 显示器目标快照。
    pub target: DisplayTarget,
    /// HDR 状态。
    pub hdr: HdrState,
    /// SDR 内容亮度原始值，范围 [1000, 6000]。
    pub raw: u32,
    /// SDR 内容亮度百分比，内部唯一真值。
    pub percent: Percent,
    /// 绝对亮度（cd/m²），范围 [80, 480]。
    pub nits: f64,
    /// 本次读取是否成功。失败时上述数值均为上一次的缓存值或默认值。
    pub readable: bool,
}

/// 写入操作的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind")]
pub enum WriteResult {
    /// 写入成功，回读值与请求值完全一致。
    Applied {
        /// 系统实际生效的 raw 值。
        raw: u32,
    },
    /// 写入被调用，但系统把值吸附到了别的刻度。
    /// UI 应当**平滑磁吸**到此值，而不是回弹跳动（架构决策 D5）。
    Adjusted {
        /// 请求的 raw 值。
        requested: u32,
        /// 系统实际生效的 raw 值。
        actual: u32,
    },
    /// 写入彻底失败。
    Failed {
        /// 错误码（英文稳定标识）。
        code: String,
        /// 用户可读中文文案。
        message: String,
        /// Win32 原始错误码（若来自 API）。
        win32: Option<i32>,
    },
}

impl WriteResult {
    /// 构造一个失败结果。
    #[must_use]
    pub fn failed(err: &crate::error::AppError) -> Self {
        Self::Failed {
            code: err.code().to_string(),
            message: err.user_message(),
            win32: match err {
                crate::error::AppError::ApiFailed(code) => Some(*code),
                _ => None,
            },
        }
    }

    /// 最终生效的 raw 值；失败时返回 `None`。
    #[must_use]
    pub fn effective_raw(&self) -> Option<u32> {
        match self {
            Self::Applied { raw } | Self::Adjusted { actual: raw, .. } => Some(*raw),
            Self::Failed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_钳位_不回绕() {
        assert_eq!(Percent::new(0).value, 0);
        assert_eq!(Percent::new(100).value, 100);
        assert_eq!(Percent::new(255).value, 100);
        assert_eq!(Percent::clamp(0).value, 0);
        assert_eq!(Percent::clamp(37).value, 37);
        assert_eq!(Percent::clamp(100).value, 100);
        assert_eq!(Percent::clamp(5000).value, 100);
        assert_eq!(Percent::clamp(u32::MAX).value, 100);
    }

    #[test]
    fn percent_偏移_边界不回绕() {
        assert_eq!(Percent::new(5).offset(-10).value, 0);
        assert_eq!(Percent::new(95).offset(10).value, 100);
        assert_eq!(Percent::new(37).offset(5).value, 42);
        assert_eq!(Percent::new(37).offset(-5).value, 32);
    }

    #[test]
    fn percent_边界判定() {
        assert!(Percent::new(0).at_min());
        assert!(!Percent::new(1).at_min());
        assert!(Percent::new(100).at_max());
        assert!(!Percent::new(99).at_max());
    }

    #[test]
    fn hdr_可用性判定() {
        assert!(HdrState { supported: true, enabled: true, bits_per_color: 12 }.is_usable());
        assert!(!HdrState { supported: true, enabled: false, bits_per_color: 8 }.is_usable());
        assert!(!HdrState { supported: false, enabled: false, bits_per_color: 8 }.is_usable());
    }

    #[test]
    fn 写入结果_生效值提取() {
        assert_eq!(WriteResult::Applied { raw: 4100 }.effective_raw(), Some(4100));
        assert_eq!(
            WriteResult::Adjusted { requested: 4101, actual: 4100 }.effective_raw(),
            Some(4100)
        );
        assert_eq!(WriteResult::failed(&crate::error::AppError::HdrDisabled).effective_raw(), None);
    }

    #[test]
    fn 写入失败_携带_win32_错误码() {
        let r = WriteResult::failed(&crate::error::AppError::ApiFailed(87));
        match r {
            WriteResult::Failed { code, win32, .. } => {
                assert_eq!(code, "API_FAILED");
                assert_eq!(win32, Some(87));
            }
            other => panic!("应为 Failed，实际 {other:?}"),
        }
    }
}
