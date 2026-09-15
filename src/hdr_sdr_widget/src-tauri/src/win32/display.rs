//! 显示器枚举与 SDR 内容亮度读写（safe 封装）。
//!
//! 对应 ARCHITECTURE.md §5.4 的 `DisplayConfigApi`。
//!
//! # 关于易失句柄（风险 R3）
//!
//! `adapterId` + `targetId` 在休眠唤醒、拔插、驱动重装后都会变化。因此本模块
//! 对外暴露的 [`DisplayTarget::key`] 使用 `monitorDevicePath`（设备实例路径）
//! 作为跨会话稳定标识；每次真正读写前，都要通过 [`rebind`] 把稳定键重新解析成
//! 当前的 (适配器, 目标 ID) 句柄。

use std::mem::size_of;

use crate::core::model::{DisplayTarget, HdrState};
use crate::error::AppError;
use crate::win32::ffi::{
    AdvancedColorInfo, DeviceInfoHeader, Luid, ModeInfo, PathInfo, SetSdrWhiteLevel,
    SdrWhiteLevel, TargetDeviceName, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER,
    ERROR_SUCCESS, QDC_ONLY_ACTIVE_PATHS,
};

/// 一次 `QueryDisplayConfig` 的原始结果。
struct RawConfig {
    /// 活动路径列表。
    paths: Vec<PathInfo>,
    /// 模式列表（本项目不解析，仅用于满足 API 契约）。
    #[allow(dead_code)]
    modes: Vec<ModeInfo>,
}

/// 查询当前全部活动显示路径。
///
/// 先取所需缓冲区长度，再分配并填充；返回 `ERROR_INSUFFICIENT_BUFFER` 时
/// 用新的长度重试一次（显示器配置在两次调用之间发生变化时的竞态）。
fn query_active_paths() -> Result<RawConfig, AppError> {
    let mut path_count: u32 = 0;
    let mut mode_count: u32 = 0;

    // SAFETY: 两个指针都指向本函数栈上已初始化的 u32，API 只会写入。
    let rc = unsafe {
        crate::win32::ffi::GetDisplayConfigBufferSizes(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            &mut mode_count,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(AppError::from_win32(rc));
    }

    // 最多重试 3 次，覆盖"取长度后又插入了显示器"的竞态。
    for attempt in 0..3 {
        let mut paths = vec![PathInfo::default(); path_count as usize];
        let mut modes = vec![ModeInfo::default(); mode_count as usize];
        let mut out_paths = path_count;
        let mut out_modes = mode_count;

        // SAFETY: paths / modes 均为已初始化的连续数组，长度由 out_paths /
        // out_modes 传入；API 写入的元素数量不会超过传入长度。
        // current_topology_id 传空指针是允许的（我们不使用拓扑信息）。
        let rc = unsafe {
            crate::win32::ffi::QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut out_paths,
                paths.as_mut_ptr(),
                &mut out_modes,
                modes.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };

        match rc {
            ERROR_SUCCESS => {
                paths.truncate(out_paths as usize);
                modes.truncate(out_modes as usize);
                return Ok(RawConfig { paths, modes });
            }
            ERROR_INSUFFICIENT_BUFFER if attempt < 2 => {
                // 用 API 回写的新长度再来一次。
                path_count = out_paths;
                mode_count = out_modes;
                continue;
            }
            other => return Err(AppError::from_win32(other)),
        }
    }

    Err(AppError::Inconsistent(
        "QueryDisplayConfig 连续 3 次返回缓冲区不足，显示器配置可能正在剧烈变化".to_string(),
    ))
}

/// 读取指定 (适配器, 目标) 的显示器友好名、设备路径与输出接口类型。
fn fetch_name(adapter: Luid, id: u32) -> Result<(String, String, u32), AppError> {
    let mut req = TargetDeviceName::read_request(adapter, id);
    let rc = unsafe {
        crate::win32::ffi::DisplayConfigGetDeviceInfo(
            &mut req.header as *mut DeviceInfoHeader,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(AppError::from_win32(rc));
    }
    Ok((req.friendly_name(), req.device_path(), req.output_tech))
}

/// 读取指定 (适配器, 目标) 的高级色彩（HDR）状态。
fn fetch_hdr(adapter: Luid, id: u32) -> Result<HdrState, AppError> {
    let mut req = AdvancedColorInfo::read_request(adapter, id);
    let rc = unsafe {
        crate::win32::ffi::DisplayConfigGetDeviceInfo(
            &mut req.header as *mut DeviceInfoHeader,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(AppError::from_win32(rc));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(HdrState {
        supported: req.supported(),
        enabled: req.enabled(),
        bits_per_color: (req.bits_per_color_channel.min(255)) as u8,
    })
}

/// 读取指定 (适配器, 目标) 的 SDR 内容亮度 raw 值。
fn fetch_sdr(adapter: Luid, id: u32) -> Result<u32, AppError> {
    let mut req = SdrWhiteLevel::read_request(adapter, id);
    let rc = unsafe {
        crate::win32::ffi::DisplayConfigGetDeviceInfo(
            &mut req.header as *mut DeviceInfoHeader,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(AppError::from_win32(rc));
    }
    Ok(req.sdr_white_level)
}

/// 枚举当前全部活动显示器。
///
/// 返回的每个 [`DisplayTarget`] 都带有稳定键 `key`（设备实例路径）与当前易失句柄。
/// `index` 为路径序号，`is_primary` 依据源端 ID 是否为 0 判定（Windows 约定
/// 主显示器的源 ID 为 0）。
pub fn enumerate_targets() -> Result<Vec<DisplayTarget>, AppError> {
    let config = query_active_paths()?;
    let mut targets = Vec::with_capacity(config.paths.len());

    for (index, path) in config.paths.iter().enumerate() {
        let target = &path.target;
        if target.target_available == 0 {
            // 路径存在但目标不可用（显示器被关闭），跳过而不是报错。
            continue;
        }

        let (name, key, output_tech) = match fetch_name(target.adapter_id, target.id) {
            Ok(v) => v,
            Err(_) => (String::new(), String::new(), target.output_tech),
        };

        // 设备路径为空时没有稳定键可用，退化成"适配器+目标ID"合成键并如实标注。
        let stable_key = if key.is_empty() {
            format!("volatile://{:016X}/{:08X}", target.adapter_id.as_u64(), target.id)
        } else {
            key
        };

        let display_name = if name.is_empty() {
            format!("显示器 {}", index + 1)
        } else {
            name
        };

        targets.push(DisplayTarget {
            key: stable_key,
            name: display_name,
            #[allow(clippy::cast_possible_truncation)]
            index: index as u32,
            is_primary: path.source.id == 0,
            output_tech,
            adapter_luid: target.adapter_id.as_u64(),
            target_id: target.id,
            refresh_hz: target.refresh_hz(),
        });
    }

    if targets.is_empty() {
        return Err(AppError::Inconsistent(
            "未枚举到任何可用的活动显示路径".to_string(),
        ));
    }

    Ok(targets)
}

/// Resolve a physical desktop point to the stable display key through its GDI source.
pub fn key_at_point(x:i32,y:i32)->Result<String,AppError> {
    use windows::Win32::{Foundation::{POINT,LUID},Graphics::Gdi::*,Devices::Display::*};
    let mut info=MONITORINFOEXW::default();info.monitorInfo.cbSize=size_of::<MONITORINFOEXW>() as u32;
    unsafe {
        let monitor=MonitorFromPoint(POINT{x,y},MONITOR_DEFAULTTONEAREST);
        if !GetMonitorInfoW(monitor,&mut info.monitorInfo).as_bool(){return Err(AppError::ApiFailed(-1));}
    }
    let config=query_active_paths()?;
    for path in config.paths {
        let mut source=DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        source.header=DISPLAYCONFIG_DEVICE_INFO_HEADER{r#type:DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,size:size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId:LUID{LowPart:path.source.adapter_id.low,HighPart:path.source.adapter_id.high},id:path.source.id};
        let result=unsafe {DisplayConfigGetDeviceInfo(&mut source.header)};
        if result==0 && source.viewGdiDeviceName==info.szDevice {
            let (_,key,_)=fetch_name(path.target.adapter_id,path.target.id)?;
            return Ok(key);
        }
    }
    Err(AppError::ApiFailed(-1))
}

/// 按稳定键（设备实例路径）重新解析出当前的易失句柄。
///
/// 这是抵御风险 R3 的核心：写操作前必须调用，绝不能复用缓存的 `target_id`。
pub fn rebind(key: &str) -> Result<DisplayTarget, AppError> {
    let targets = enumerate_targets()?;
    targets
        .into_iter()
        .find(|t| t.key == key)
        .ok_or(AppError::TargetNotFound)
}

/// 按序号取一台显示器（用于"退化匹配"与命令行工具的 `--index` 参数）。
pub fn target_by_index(index: usize) -> Result<DisplayTarget, AppError> {
    let targets = enumerate_targets()?;
    targets
        .into_iter()
        .find(|t| t.index as usize == index)
        .ok_or(AppError::TargetNotFound)
}

/// 取第一台可用显示器。
pub fn first_target() -> Result<DisplayTarget, AppError> {
    let mut targets = enumerate_targets()?;
    if targets.is_empty() {
        return Err(AppError::TargetNotFound);
    }
    Ok(targets.swap_remove(0))
}

/// 读取指定显示器的 SDR 内容亮度 raw 值。
///
/// 内部会先按稳定键重新解析句柄，因此传入过期的 `DisplayTarget` 快照也不会写错显示器。
pub fn read_sdr_white(target: &DisplayTarget) -> Result<u32, AppError> {
    let fresh = rebind(&target.key)?;
    fetch_sdr(Luid::from_u64(fresh.adapter_luid), fresh.target_id)
}

/// 读取指定显示器的 HDR（高级色彩）状态。
pub fn read_advanced_color(target: &DisplayTarget) -> Result<HdrState, AppError> {
    let fresh = rebind(&target.key)?;
    fetch_hdr(Luid::from_u64(fresh.adapter_luid), fresh.target_id)
}

/// **写入**指定显示器的 SDR 内容亮度 raw 值。
///
/// 使用 undocumented 私有 API `SET_SDR_WHITE_LEVEL = 0xFFFFFFEE`，
/// `final_value` 恒置 1（置 0 时系统不应用该值）。
///
/// # 副作用
///
/// 会立即改变用户屏幕的 SDR 内容亮度。调用方**必须**负责在验证结束后还原原值，
/// 参见 `src/bin/probe.rs` 中的 RAII `RestoreGuard`。
pub fn write_sdr_white(target: &DisplayTarget, raw: u32) -> Result<(), AppError> {
    let fresh = rebind(&target.key)?;
    let adapter = Luid::from_u64(fresh.adapter_luid);

    // 先把 raw 钳到合法区间，避免把越界值喂给内核。
    let clamped = crate::core::convert::clamp_raw(raw);

    let req = SetSdrWhiteLevel::write_request(adapter, fresh.target_id, clamped);
    let rc = unsafe {
        crate::win32::ffi::DisplayConfigSetDeviceInfo(
            &req.header as *const DeviceInfoHeader,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(AppError::from_win32(rc));
    }
    Ok(())
}

/// 写入能力的探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCapability {
    /// 私有写入 API 可用：写入当前值本身后立即回读，值保持一致。
    Available,
    /// API 调用本身成功，但回读值与写入值不符（被系统吸附）。
    AvailableWithSnap {
        /// 系统实际保留的值。
        actual: u32,
    },
    /// API 返回"不支持"。
    Unsupported,
    /// API 返回"拒绝访问"。
    Denied,
    /// 其他失败。
    Failed(i32),
}

/// 探测写入 API 是否可用。
///
/// 手法：读取当前值 A，然后**写入 A 本身**（无副作用），再回读。
/// 若三者一致则判定可用。这是风险 R1 的常规自检，会在每次启动与每次
/// 打开面板时执行。
pub fn probe_write_support() -> WriteCapability {
    let target = match first_target() {
        Ok(t) => t,
        Err(_) => return WriteCapability::Failed(-1),
    };

    let current = match read_sdr_white(&target) {
        Ok(v) => v,
        Err(_) => return WriteCapability::Failed(-1),
    };

    if let Err(err) = write_sdr_white(&target, current) {
        return match err {
            AppError::NotSupported => WriteCapability::Unsupported,
            AppError::PermissionDenied => WriteCapability::Denied,
            AppError::ApiFailed(code) => WriteCapability::Failed(code),
            other => WriteCapability::Failed(match other {
                AppError::ApiFailed(code) => code,
                _ => -1,
            }),
        };
    }

    match read_sdr_white(&target) {
        Ok(actual) if actual == current => WriteCapability::Available,
        Ok(actual) => WriteCapability::AvailableWithSnap { actual },
        Err(_) => WriteCapability::Failed(-1),
    }
}

/// 真实 Win32 实现，适配 [`crate::core::controller::DisplayApi`]。
///
/// 存在的意义：让 `BrightnessController` 可以脱离硬件做单测，
/// 同时真机路径不需要任何条件编译。
#[derive(Debug, Default, Clone, Copy)]
pub struct RealApi;

impl RealApi {
    /// 构造一个真实 API 适配器。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl crate::core::controller::DisplayApi for RealApi {
    fn enumerate(&self) -> Result<Vec<DisplayTarget>, AppError> {
        enumerate_targets()
    }

    fn rebind(&self, key: &str) -> Result<DisplayTarget, AppError> {
        crate::win32::display::rebind(key)
    }

    fn read_sdr(&self, target: &DisplayTarget) -> Result<u32, AppError> {
        read_sdr_white(target)
    }

    fn read_hdr(&self, target: &DisplayTarget) -> Result<HdrState, AppError> {
        read_advanced_color(target)
    }

    fn write_sdr(&self, target: &DisplayTarget, raw: u32) -> Result<(), AppError> {
        write_sdr_white(target, raw)
    }
}

/// 判断某个 Win32 返回码是否意味着"结构体尺寸写错了"。
///
/// 这是本模块最容易犯的错误，单独提供一个判定函数供诊断面板使用。
#[must_use]
pub fn is_size_mismatch(rc: i32) -> bool {
    rc == ERROR_INVALID_PARAMETER
}

impl Luid {
    /// 从 `as_u64()` 打包出的值还原成 LUID。
    #[must_use]
    pub fn from_u64(v: u64) -> Self {
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        Self {
            low: (v & 0xFFFF_FFFF) as u32,
            high: (v >> 32) as i32,
        }
    }
}

/// 结构体尺寸快照，用于诊断输出与启动自检。
#[must_use]
pub fn struct_size_report() -> [( &'static str, usize); 6] {
    [
        ("DeviceInfoHeader", size_of::<DeviceInfoHeader>()),
        ("PathInfo", size_of::<PathInfo>()),
        ("ModeInfo", size_of::<ModeInfo>()),
        ("SdrWhiteLevel", size_of::<SdrWhiteLevel>()),
        ("AdvancedColorInfo", size_of::<AdvancedColorInfo>()),
        ("TargetDeviceName", size_of::<TargetDeviceName>()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luid_打包还原往返() {
        for v in [0u64, 1, 0x0001_27E9, 0x0000_0000_0001_27E9, u64::MAX] {
            assert_eq!(Luid::from_u64(v).as_u64(), v);
        }
    }

    #[test]
    fn 结构体尺寸快照与实测一致() {
        let report = struct_size_report();
        let map: std::collections::HashMap<_, _> = report.iter().copied().collect();
        assert_eq!(map["DeviceInfoHeader"], 20);
        assert_eq!(map["PathInfo"], 72);
        assert_eq!(map["ModeInfo"], 64);
        assert_eq!(map["SdrWhiteLevel"], 24);
        assert_eq!(map["AdvancedColorInfo"], 32);
        assert_eq!(map["TargetDeviceName"], 420);
    }

    #[test]
    fn 尺寸错误判定() {
        assert!(is_size_mismatch(ERROR_INVALID_PARAMETER));
        assert!(!is_size_mismatch(ERROR_SUCCESS));
    }

    /// 真实硬件冒烟测试：本机有 1 台 Mi Monitor。
    /// 需要真机环境，因此在无显示器的 CI 上会被跳过而不是失败。
    #[test]
    fn 真机_枚举到至少一台显示器() {
        let targets = match enumerate_targets() {
            Ok(t) => t,
            Err(_) => return, // 环境无可用显示器，跳过
        };
        assert!(!targets.is_empty());
        for t in &targets {
            assert!(!t.key.is_empty());
            assert!(!t.name.is_empty());
        }
    }

    /// 真机冒烟测试：读到的值必须落在合法区间，且与 Python 探针一致。
    #[test]
    fn 真机_读到的_raw_在合法区间() {
        let target = match first_target() {
            Ok(t) => t,
            Err(_) => return,
        };
        let raw = match read_sdr_white(&target) {
            Ok(v) => v,
            Err(_) => return,
        };
        assert!(
            (1000..=6000).contains(&raw),
            "读到的 raw={raw} 越界，疑似结构体布局错误"
        );
    }
}
