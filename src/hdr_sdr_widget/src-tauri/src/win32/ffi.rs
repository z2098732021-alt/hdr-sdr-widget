//! Win32 DisplayConfig API 的 FFI 声明与 `#[repr(C)]` 结构体定义。
//!
//! # 为什么全部手写而不依赖 `windows` crate
//!
//! `DISPLAYCONFIG_DEVICE_INFO_SET_SDR_WHITE_LEVEL`（0xFFFFFFEE）是 **undocumented
//! 私有 API**，不在任何官方 SDK 头文件或 `windows` crate 的公开枚举中，必须自行
//! 定义常量与结构体布局。
//!
//! # 内存布局是本模块的生命线
//!
//! 结构体错 1 字节就会让 `DisplayConfigSetDeviceInfo` 返回
//! `ERROR_INVALID_PARAMETER(87)`，或在更坏的情况下踩坏相邻内存。因此本模块
//! 用**编译期断言**把五个关键结构体的字节数锁死；一旦布局漂移，直接编译失败，
//! 绝不放过到运行时。
//!
//! 尺寸来源：架构师高见远在本机（Windows 11 build 26200）实测复核，
//! 且已由 `tools/probe_read.py`（Python ctypes）独立复现，两者一致。

#![allow(non_snake_case)]

use std::mem::size_of;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 只查询当前处于活动状态的显示路径。
pub const QDC_ONLY_ACTIVE_PATHS: u32 = 0x0000_0002;

/// 查询显示器友好名与设备实例路径。
pub const DEVICE_INFO_GET_TARGET_NAME: u32 = 2;

/// 查询高级色彩（HDR）支持与启用状态。
pub const DEVICE_INFO_GET_ADVANCED_COLOR_INFO: u32 = 9;

/// 查询 SDR 内容亮度 raw 值。
pub const DEVICE_INFO_GET_SDR_WHITE_LEVEL: u32 = 11;

/// **写入** SDR 内容亮度。私有 / undocumented，非官方 SDK 公开常量。
pub const DEVICE_INFO_SET_SDR_WHITE_LEVEL: u32 = 0xFFFF_FFEE;

/// `DisplayConfigGetDeviceInfo` / `DisplayConfigSetDeviceInfo` 的成功返回码。
pub const ERROR_SUCCESS: i32 = 0;

/// 参数非法（结构体 size 字段写错时最常见）。
pub const ERROR_INVALID_PARAMETER: i32 = 87;

/// 该设备不支持此操作。
pub const ERROR_NOT_SUPPORTED: i32 = 50;

/// 拒绝访问（权限不足）。
pub const ERROR_ACCESS_DENIED: i32 = 5;

/// 设备未就绪 / 一般性硬件失败。
pub const ERROR_GEN_FAILURE: i32 = 31;

/// 缓冲区不足（枚举时数组太小）。
pub const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

/// `ACTIVE_COLOR_INFO` 的 bit0：显示器支持高级色彩（HDR）。
pub const ADVANCED_COLOR_SUPPORTED: u32 = 0x1;

/// `ACTIVE_COLOR_INFO` 的 bit1：高级色彩（HDR）当前已启用。
pub const ADVANCED_COLOR_ENABLED: u32 = 0x2;

/// `ACTIVE_COLOR_INFO` 的 bit2：宽色域强制开启。
pub const ADVANCED_COLOR_WIDE_ENFORCED: u32 = 0x4;

/// 输出接口类型：HDMI。本机 Mi Monitor 实测即为此值。
pub const OUTPUT_TECHNOLOGY_HDMI: u32 = 5;

// ---------------------------------------------------------------------------
// 结构体
// ---------------------------------------------------------------------------

/// 本地唯一标识符（适配器 ID）。对应 Win32 `LUID`。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Luid {
    pub low: u32,
    pub high: i32,
}

impl Luid {
    /// 把 LUID 打包成一个 u64，便于做 HashMap 键与日志输出。
    #[must_use]
    pub fn as_u64(self) -> u64 {
        ((self.high as u32) as u64) << 32 | u64::from(self.low)
    }
}

/// 所有 `DisplayConfig*DeviceInfo` 结构体的公共头，20 字节。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceInfoHeader {
    /// 操作类型（`DEVICE_INFO_GET_*` / `DEVICE_INFO_SET_*`）。
    pub kind: u32,
    /// **整个结构体的字节数**。写错会直接返回 `ERROR_INVALID_PARAMETER`。
    pub size: u32,
    /// 目标显示器所属适配器。
    pub adapter_id: Luid,
    /// 目标 ID（与 `DISPLAYCONFIG_PATH_TARGET_INFO::id` 同源）。
    pub id: u32,
}

impl DeviceInfoHeader {
    /// 构造一个指向指定 (适配器, 目标) 的请求头。
    #[must_use]
    pub fn new(kind: u32, size: u32, adapter_id: Luid, id: u32) -> Self {
        Self { kind, size, adapter_id, id }
    }
}

/// 路径源端信息，20 字节。本项目不解析其内部字段，按原始字节保留。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PathSource {
    pub adapter_id: Luid,
    pub id: u32,
    pub mode_info_idx: u32,
    pub status_flags: u32,
}

/// 刷新率（`DISPLAYCONFIG_RATIONAL` 的 `repr(C)` 等价，分子 / 分母）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Rational {
    pub numerator: u32,
    pub denominator: u32,
}

/// 路径目标端（即一台显示器）信息，48 字节。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PathTarget {
    pub adapter_id: Luid,
    pub id: u32,
    pub mode_info_idx: u32,
    pub output_tech: u32,
    pub rotation: u32,
    pub scaling: u32,
    pub refresh_rate: Rational,
    pub scan_line_ordering: u32,
    pub target_available: i32,
    pub status_flags: u32,
}

impl PathTarget {
    /// 刷新率（Hz）。分母为 0 时返回 0.0，避免除零。
    #[must_use]
    pub fn refresh_hz(self) -> f64 {
        let num = self.refresh_rate.numerator;
        let den = self.refresh_rate.denominator;
        if den == 0 {
            0.0
        } else {
            f64::from(num) / f64::from(den)
        }
    }
}

/// 一条显示路径（源 → 目标），72 字节。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PathInfo {
    pub source: PathSource,
    pub target: PathTarget,
    pub flags: u32,
}

/// 模式信息（源模式 / 目标模式共用容器），64 字节。
/// 本项目只需要数组容量，不解析 union 内部。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ModeInfo {
    pub info_type: u32,
    pub id: u32,
    pub adapter_id: Luid,
    pub union_data: [u8; 48],
}

impl Default for ModeInfo {
    fn default() -> Self {
        Self { info_type: 0, id: 0, adapter_id: Luid::default(), union_data: [0u8; 48] }
    }
}

/// 读取 SDR 内容亮度的请求/响应体，24 字节。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SdrWhiteLevel {
    pub header: DeviceInfoHeader,
    /// 原始值，有效范围 [1000, 6000]。
    pub sdr_white_level: u32,
}

impl SdrWhiteLevel {
    /// 构造一个针对指定目标的读取请求。
    #[must_use]
    pub fn read_request(adapter_id: Luid, id: u32) -> Self {
        Self {
            header: DeviceInfoHeader::new(
                DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size_of::<Self>() as u32,
                adapter_id,
                id,
            ),
            sdr_white_level: 0,
        }
    }
}

/// 读取高级色彩（HDR）状态的请求/响应体，32 字节。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AdvancedColorInfo {
    pub header: DeviceInfoHeader,
    /// bit0 = supported，bit1 = enabled，bit2 = wideColorEnforced。
    pub value: u32,
    pub color_encoding: u32,
    pub bits_per_color_channel: u32,
}

impl AdvancedColorInfo {
    /// 构造一个针对指定目标的读取请求。
    #[must_use]
    pub fn read_request(adapter_id: Luid, id: u32) -> Self {
        Self {
            header: DeviceInfoHeader::new(
                DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size_of::<Self>() as u32,
                adapter_id,
                id,
            ),
            value: 0,
            color_encoding: 0,
            bits_per_color_channel: 0,
        }
    }

    /// 显示器是否支持高级色彩（HDR）。
    #[must_use]
    pub fn supported(self) -> bool {
        self.value & ADVANCED_COLOR_SUPPORTED != 0
    }

    /// HDR 当前是否已启用。
    #[must_use]
    pub fn enabled(self) -> bool {
        self.value & ADVANCED_COLOR_ENABLED != 0
    }

    /// 宽色域是否被强制开启。
    #[must_use]
    pub fn wide_color_enforced(self) -> bool {
        self.value & ADVANCED_COLOR_WIDE_ENFORCED != 0
    }
}

/// 读取显示器友好名与设备实例路径，420 字节。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TargetDeviceName {
    pub header: DeviceInfoHeader,
    pub flags: u32,
    pub output_tech: u32,
    pub manufacturer_id: u16,
    pub product_id: u16,
    pub connector_instance: u32,
    /// 以 NUL 结尾的 UTF-16 友好名，最多 63 个字符 + 结尾 NUL。
    pub friendly_name: [u16; 64],
    /// 以 NUL 结尾的 UTF-16 设备实例路径，最多 127 个字符 + 结尾 NUL。
    pub device_path: [u16; 128],
}

impl TargetDeviceName {
    /// 构造一个针对指定目标的读取请求。
    #[must_use]
    pub fn read_request(adapter_id: Luid, id: u32) -> Self {
        Self {
            header: DeviceInfoHeader::new(
                DEVICE_INFO_GET_TARGET_NAME,
                size_of::<Self>() as u32,
                adapter_id,
                id,
            ),
            flags: 0,
            output_tech: 0,
            manufacturer_id: 0,
            product_id: 0,
            connector_instance: 0,
            friendly_name: [0u16; 64],
            device_path: [0u16; 128],
        }
    }

    /// 把 UTF-16 缓冲区转成 `String`，遇 NUL 截断；非法码位用 U+FFFD 替换。
    #[must_use]
    pub fn decode(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// 显示器友好名，例如 `Mi Monitor`。
    #[must_use]
    pub fn friendly_name(self) -> String {
        Self::decode(&self.friendly_name)
    }

    /// 设备实例路径，跨会话稳定，用作显示器的持久化唯一键。
    #[must_use]
    pub fn device_path(self) -> String {
        Self::decode(&self.device_path)
    }
}

impl Default for TargetDeviceName {
    fn default() -> Self {
        Self {
            header: DeviceInfoHeader::default(),
            flags: 0,
            output_tech: 0,
            manufacturer_id: 0,
            product_id: 0,
            connector_instance: 0,
            friendly_name: [0u16; 64],
            device_path: [0u16; 128],
        }
    }
}

/// **写入** SDR 内容亮度的请求体，28 字节（私有 / undocumented）。
///
/// `final_value` 必须置 1；置 0 时系统不应用该值（社区实测结论）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SetSdrWhiteLevel {
    pub header: DeviceInfoHeader,
    /// 目标 raw 值，有效范围 [1000, 6000]。
    pub sdr_white_level: u32,
    /// 置 1 才会真正生效。
    pub final_value: u8,
}

impl SetSdrWhiteLevel {
    /// 构造一个针对指定目标的写入请求。
    #[must_use]
    pub fn write_request(adapter_id: Luid, id: u32, raw: u32) -> Self {
        Self {
            header: DeviceInfoHeader::new(
                DEVICE_INFO_SET_SDR_WHITE_LEVEL,
                size_of::<Self>() as u32,
                adapter_id,
                id,
            ),
            sdr_white_level: raw,
            final_value: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// 编译期护栏：布局错 1 字节就编译失败
// ---------------------------------------------------------------------------

const _: () = assert!(size_of::<Luid>() == 8);
const _: () = assert!(size_of::<DeviceInfoHeader>() == 20);
const _: () = assert!(size_of::<PathSource>() == 20);
const _: () = assert!(size_of::<PathTarget>() == 48);
const _: () = assert!(size_of::<PathInfo>() == 72);
const _: () = assert!(size_of::<ModeInfo>() == 64);
const _: () = assert!(size_of::<SdrWhiteLevel>() == 24);
const _: () = assert!(size_of::<AdvancedColorInfo>() == 32);
const _: () = assert!(size_of::<TargetDeviceName>() == 420);
const _: () = assert!(size_of::<SetSdrWhiteLevel>() == 28);

// ---------------------------------------------------------------------------
// extern 声明
// ---------------------------------------------------------------------------

#[link(name = "user32")]
extern "system" {
    /// 获取枚举当前显示配置所需的数组长度。
    pub fn GetDisplayConfigBufferSizes(
        flags: u32,
        num_path_array_elements: *mut u32,
        num_mode_info_array_elements: *mut u32,
    ) -> i32;

    /// 查询当前显示配置，填充路径与模式数组。
    pub fn QueryDisplayConfig(
        flags: u32,
        num_path_array_elements: *mut u32,
        path_info_array: *mut PathInfo,
        num_mode_info_array_elements: *mut u32,
        mode_info_array: *mut ModeInfo,
        current_topology_id: *mut u64,
    ) -> i32;

    /// 读取指定目标的设备信息。
    pub fn DisplayConfigGetDeviceInfo(request_packet: *mut DeviceInfoHeader) -> i32;

    /// 写入指定目标的设备信息。
    pub fn DisplayConfigSetDeviceInfo(request_packet: *const DeviceInfoHeader) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 结构体尺寸与实测一致() {
        assert_eq!(size_of::<PathInfo>(), 72);
        assert_eq!(size_of::<ModeInfo>(), 64);
        assert_eq!(size_of::<SdrWhiteLevel>(), 24);
        assert_eq!(size_of::<AdvancedColorInfo>(), 32);
        assert_eq!(size_of::<TargetDeviceName>(), 420);
        assert_eq!(size_of::<SetSdrWhiteLevel>(), 28);
    }

    #[test]
    fn 字段偏移量正确() {
        use std::mem::offset_of;
        // TargetDeviceName 的布局最容易因 u16 与 u32 交替而出错，逐个字段锁死。
        assert_eq!(offset_of!(TargetDeviceName, header), 0);
        assert_eq!(offset_of!(TargetDeviceName, flags), 20);
        assert_eq!(offset_of!(TargetDeviceName, output_tech), 24);
        assert_eq!(offset_of!(TargetDeviceName, manufacturer_id), 28);
        assert_eq!(offset_of!(TargetDeviceName, product_id), 30);
        assert_eq!(offset_of!(TargetDeviceName, connector_instance), 32);
        assert_eq!(offset_of!(TargetDeviceName, friendly_name), 36);
        assert_eq!(offset_of!(TargetDeviceName, device_path), 164);
    }

    #[test]
    fn 写入请求头正确() {
        let luid = Luid { low: 0x0001_27E9, high: 0 };
        let req = SetSdrWhiteLevel::write_request(luid, 4354, 2850);
        assert_eq!(req.header.kind, DEVICE_INFO_SET_SDR_WHITE_LEVEL);
        assert_eq!(req.header.size, 28);
        assert_eq!(req.header.id, 4354);
        assert_eq!(req.sdr_white_level, 2850);
        assert_eq!(req.final_value, 1);
    }

    #[test]
    fn luid_打包成_u64() {
        let luid = Luid { low: 0x0001_27E9, high: 0 };
        assert_eq!(luid.as_u64(), 0x0001_27E9);
        assert_eq!(format!("{:016X}", luid.as_u64()), "00000000000127E9");
    }

    #[test]
    fn utf16_解码遇_nul_截断() {
        let mut buf = [0u16; 8];
        for (i, c) in "Mi".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(TargetDeviceName::decode(&buf), "Mi");
    }

    #[test]
    fn 刷新率换算不除零() {
        let mut t = PathTarget::default();
        t.refresh_rate = Rational { numerator: 0, denominator: 0 };
        assert_eq!(t.refresh_hz(), 0.0);
        t.refresh_rate = Rational { numerator: 59954, denominator: 1000 };
        assert!((t.refresh_hz() - 59.954).abs() < 1e-6);
    }
}
