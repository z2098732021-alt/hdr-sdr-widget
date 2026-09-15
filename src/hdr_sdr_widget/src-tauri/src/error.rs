//! 统一错误类型与中文用户文案。
//!
//! 设计原则：**绝不静默吞掉错误**。每一类失败都必须能翻译成一句用户看得懂的
//! 中文，并携带足够的诊断字段（错误码、显示器路径、当前路线）供"复制诊断信息"。

use std::fmt;

use serde::Serialize;

use crate::win32::ffi::{
    ERROR_ACCESS_DENIED, ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
};

/// 本应用对外暴露的全部失败模式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "code", content = "detail", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AppError {
    /// Win32 API 返回错误码。
    ApiFailed(i32),
    /// 该显示器 / 系统不支持此操作。
    NotSupported,
    /// 显示器支持 HDR，但当前未开启。
    HdrDisabled,
    /// 稳定键对应的显示器已不存在（被拔掉、休眠唤醒后重建）。
    TargetNotFound,
    /// 权限不足（非管理员且需要提权）。
    PermissionDenied,
    /// 传入参数越界（例如 percent > 100）。
    InvalidArgument(String),
    /// 内部一致性错误：枚举到的路径与请求的目标对不上。
    Inconsistent(String),
    /// IO / 序列化失败（配置持久化）。
    Io(String),
}

impl AppError {
    /// 把 Win32 返回码翻译成语义化错误。0 视为成功，调用方不应走到这里。
    #[must_use]
    pub fn from_win32(code: i32) -> Self {
        match code {
            0 => Self::Inconsistent("Win32 返回 0（成功）却被当作错误处理".to_string()),
            ERROR_ACCESS_DENIED => Self::PermissionDenied,
            ERROR_NOT_SUPPORTED => Self::NotSupported,
            ERROR_INVALID_PARAMETER => Self::ApiFailed(ERROR_INVALID_PARAMETER),
            ERROR_GEN_FAILURE => Self::ApiFailed(ERROR_GEN_FAILURE),
            other => Self::ApiFailed(other),
        }
    }

    /// 稳定的英文错误码，供前端 `switch` 与日志检索。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ApiFailed(_) => "API_FAILED",
            Self::NotSupported => "NOT_SUPPORTED",
            Self::HdrDisabled => "HDR_DISABLED",
            Self::TargetNotFound => "TARGET_NOT_FOUND",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::InvalidArgument(_) => "INVALID_ARGUMENT",
            Self::Inconsistent(_) => "INCONSISTENT",
            Self::Io(_) => "IO",
        }
    }

    /// 用户可读的中文文案。会直接显示在 Toast 上，必须具体到"下一步能做什么"。
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::ApiFailed(ERROR_INVALID_PARAMETER) => {
                "系统拒绝了这个亮度值：可能是该显示器不支持此档位，或显卡驱动不兼容。\
                 请尝试相邻档位；若反复出现，点击\"复制诊断信息\"反馈。"
                    .to_string()
            }
            Self::ApiFailed(ERROR_GEN_FAILURE) => {
                "显卡驱动返回了硬件错误。请更新显卡驱动后重试；\
                 若刚从休眠唤醒，稍等几秒再试。"
                    .to_string()
            }
            Self::ApiFailed(code) => {
                format!("系统调用失败（错误码 {code}）。可尝试重启本软件；若持续失败，请复制诊断信息。")
            }
            Self::NotSupported => {
                "当前显示器或系统版本不支持以程序方式调整 SDR 内容亮度。\
                 你可以点击下方按钮前往系统设置手动调整。"
                    .to_string()
            }
            Self::HdrDisabled => {
                "HDR 尚未开启，SDR 内容亮度调节不生效。请先在系统设置中开启 HDR。"
                    .to_string()
            }
            Self::TargetNotFound => {
                "找不到之前选中的那台显示器。它可能已被拔出、切换或重新枚举。\
                 请重新选择显示器。"
                    .to_string()
            }
            Self::PermissionDenied => {
                "权限不足，无法写入显示设置。请以普通用户身份重启本软件；\
                 若仍失败，可尝试以管理员身份运行。"
                    .to_string()
            }
            Self::InvalidArgument(detail) => format!("参数无效：{detail}"),
            Self::Inconsistent(detail) => format!("内部状态不一致：{detail}"),
            Self::Io(detail) => format!("读写配置失败：{detail}"),
        }
    }

    /// 是否属于"重试有可能自愈"的瞬时错误。
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::ApiFailed(ERROR_GEN_FAILURE) | Self::TargetNotFound
        )
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiFailed(code) => write!(f, "Win32 API 失败（错误码 {code}）"),
            Self::NotSupported => write!(f, "不支持此操作"),
            Self::HdrDisabled => write!(f, "HDR 未开启"),
            Self::TargetNotFound => write!(f, "目标显示器不存在"),
            Self::PermissionDenied => write!(f, "权限不足"),
            Self::InvalidArgument(detail) => write!(f, "参数无效：{detail}"),
            Self::Inconsistent(detail) => write!(f, "内部状态不一致：{detail}"),
            Self::Io(detail) => write!(f, "IO 失败：{detail}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误码分类正确() {
        assert_eq!(AppError::from_win32(5), AppError::PermissionDenied);
        assert_eq!(AppError::from_win32(50), AppError::NotSupported);
        assert_eq!(
            AppError::from_win32(87),
            AppError::ApiFailed(ERROR_INVALID_PARAMETER)
        );
        assert_eq!(AppError::from_win32(31), AppError::ApiFailed(ERROR_GEN_FAILURE));
        assert_eq!(AppError::from_win32(1234), AppError::ApiFailed(1234));
    }

    #[test]
    fn 每类错误都有非空中文文案() {
        let samples = [
            AppError::ApiFailed(87),
            AppError::ApiFailed(31),
            AppError::ApiFailed(9999),
            AppError::NotSupported,
            AppError::HdrDisabled,
            AppError::TargetNotFound,
            AppError::PermissionDenied,
            AppError::InvalidArgument("测试".to_string()),
            AppError::Inconsistent("测试".to_string()),
            AppError::Io("测试".to_string()),
        ];
        for err in samples {
            let msg = err.user_message();
            assert!(!msg.trim().is_empty(), "{err:?} 的文案为空");
            assert!(!err.code().is_empty());
        }
    }

    #[test]
    fn 瞬时错误可识别() {
        assert!(AppError::ApiFailed(ERROR_GEN_FAILURE).is_transient());
        assert!(AppError::TargetNotFound.is_transient());
        assert!(!AppError::PermissionDenied.is_transient());
        assert!(!AppError::HdrDisabled.is_transient());
    }

    #[test]
    fn 序列化为稳定的_code_标签() {
        let json = serde_json::to_string(&AppError::HdrDisabled).unwrap();
        assert_eq!(json, r#"{"code":"HDR_DISABLED"}"#);
        let json = serde_json::to_string(&AppError::ApiFailed(87)).unwrap();
        assert_eq!(json, r#"{"code":"API_FAILED","detail":87}"#);
    }
}
