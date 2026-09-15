//! 注册表只读旁证：读取 `MonitorDataStore` 下记录的 `SDRWhiteLevel`。
//!
//! 对应 ARCHITECTURE.md 的 S2.10 与验证项 V4。
//!
//! # 为什么需要这条旁证
//!
//! `SET_SDR_WHITE_LEVEL` 是 undocumented API。判断"写入是否真的落到了系统持久层"
//! （而不是只改了内存态），最有力的证据就是注册表里对应的值也变了——因为
//! Windows 系统设置读的就是这个键。
//!
//! # 只读是本模块的铁律
//!
//! 本模块**绝不写入**任何注册表项。所有函数都是读操作。

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_DWORD, KEY_READ,
};

/// `MonitorDataStore` 的注册表根路径。
const MONITOR_DATA_STORE: &str =
    r"SYSTEM\CurrentControlSet\Control\GraphicsDrivers\MonitorDataStore";

/// 值名。
const VALUE_NAME: &str = "SDRWhiteLevel";

/// 从完整设备实例路径中提取注册表子键名。
///
/// 输入形如：
/// ```text
/// \\?\DISPLAY#XMI3009#5&1e5a718c&0&UID4354#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}
/// ```
/// 输出：
/// ```text
/// DISPLAY#XMI3009#5&1e5a718c&0&UID4354
/// ```
///
/// 规则：去掉开头的 `\\?\`，再截掉从 `#{` 开始的 GUID 后缀。
#[must_use]
pub fn instance_id_from_device_path(device_path: &str) -> Option<String> {
    let without_prefix = device_path.strip_prefix(r"\\?\").unwrap_or(device_path);
    // 截掉尾部的 `#{...}` 设备接口 GUID 段。
    let cleaned = match without_prefix.find("#{") {
        Some(pos) => &without_prefix[..pos],
        None => without_prefix,
    };
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// 读取某台显示器在注册表中记录的 `SDRWhiteLevel`。
///
/// `device_path` 传 [`crate::core::model::DisplayTarget::key`]。
///
/// 返回 `Ok(None)` 表示注册表里没有这项（某些显示器 / 驱动不会写这个键，
/// 属于正常情况，不应视为错误）。
pub fn read_sdr_white_level(device_path: &str) -> Result<Option<u32>, String> {
    let instance = match instance_id_from_device_path(device_path) {
        Some(v) => v,
        None => return Err(format!("无法从设备路径解析实例 ID：{device_path}")),
    };

    let subkey = format!("{MONITOR_DATA_STORE}\\{instance}");
    let subkey_wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let value_wide: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();

    let mut value: u32 = 0;
    let mut size = u32::try_from(size_of::<u32>()).unwrap_or(4);

    // SAFETY: subkey_wide / value_wide 均以 NUL 结尾，size 与缓冲区类型匹配
    // （RRF_RT_DWORD 保证只接受 4 字节 DWORD）。
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_wide.as_ptr()),
            PCWSTR(value_wide.as_ptr()),
            RRF_RT_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };

    if status.is_err() {
        // 键不存在是常见情况（并非所有显示器都记录此项），返回 Ok(None)。
        return Ok(None);
    }

    Ok(Some(value))
}

/// 读取全部活动显示器的注册表旁证值。
///
/// 返回 `(设备路径, 注册表值)` 列表；没有记录的显示器不会出现在结果中。
pub fn read_all() -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let targets = match crate::win32::display::enumerate_targets() {
        Ok(t) => t,
        Err(_) => return out,
    };
    for target in targets {
        if let Ok(Some(raw)) = read_sdr_white_level(&target.key) {
            out.push((target.key.clone(), raw));
        }
    }
    out
}

/// 打开一个只读的注册表键（供诊断面板验证权限）。
///
/// 保留此函数是为了在"复制诊断信息"里能明确区分"键不存在"与"没有权限读取"。
#[must_use]
pub fn can_read_store() -> bool {
    use windows::Win32::System::Registry::{RegOpenKeyExW, RegCloseKey, HKEY};

    let path_wide: Vec<u16> = MONITOR_DATA_STORE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut hkey = HKEY::default();

    // SAFETY: path_wide 以 NUL 结尾；成功后必须关闭句柄。
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path_wide.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
    };

    if status.is_err() {
        return false;
    }

    // SAFETY: hkey 由上面的成功调用返回，关闭一次即可。
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 实例_id_解析() {
        let path =
            r"\\?\DISPLAY#XMI3009#5&1e5a718c&0&UID4354#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(
            instance_id_from_device_path(path),
            Some("DISPLAY#XMI3009#5&1e5a718c&0&UID4354".to_string())
        );
    }

    #[test]
    fn 无前缀也能解析() {
        assert_eq!(
            instance_id_from_device_path("DISPLAY#AAA#1&2&3&UID9#{abc}"),
            Some("DISPLAY#AAA#1&2&3&UID9".to_string())
        );
    }

    #[test]
    fn 空路径返回_none() {
        assert_eq!(instance_id_from_device_path(""), None);
        assert_eq!(instance_id_from_device_path("#{x}"), None);
    }

    #[test]
    fn 非法路径报错而不是静默返回() {
        // 空字符串会被 instance_id 解析拦下并报错。
        assert!(read_sdr_white_level("#{x}").is_err());
    }

    #[test]
    fn 不存在的键返回_none() {
        assert_eq!(
            read_sdr_white_level(r"\\?\DISPLAY#NOTEXIST#0&0&0&UID0#{00000000-0000-0000-0000-000000000000}"),
            Ok(None)
        );
    }

    /// 真机测试：本机 Mi Monitor 应能读到注册表旁证值。
    #[test]
    fn 真机_注册表可读() {
        assert!(can_read_store(), "无法读取 MonitorDataStore，权限或路径有变");
    }
}
