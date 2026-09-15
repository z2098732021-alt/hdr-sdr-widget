//! 开机自启：HKCU Run 键增删（架构决策 D7）。
//!
//! 为什么不用任务计划程序：无需管理员权限、无触发器延迟、任务管理器可直接
//! 显示启用/禁用状态（PRD P1-4 验收要求）。
//!
//! 注册表函数用原始 extern 声明（同 `win32/ffi.rs` 风格），避免依赖
//! `windows` crate 的 flags 类型差异。

use std::os::windows::ffi::OsStrExt;

/// HKCU Run 键路径。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 本应用的自启值名。
const VALUE_NAME: &str = "HdrSdrWidget";
/// `ERROR_SUCCESS`。
const ERROR_SUCCESS: i32 = 0;
/// `ERROR_FILE_NOT_FOUND`。
const ERROR_FILE_NOT_FOUND: i32 = 2;

// HKEY 句柄常量（HKEY_CURRENT_USER 是伪句柄，直接传值即可）。
const HKEY_CURRENT_USER: usize = 0x8000_0001;
// 访问权限。
const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_SET_VALUE: u32 = 0x0002;
// 值类型：以 NUL 结尾的 UTF-16 字符串。
const REG_SZ: u32 = 1;

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        hkey: usize,
        lp_sub_key: *const u16,
        ul_options: u32,
        sam_desired: u32,
        phk_result: *mut usize,
    ) -> i32;
    fn RegSetValueExW(
        hkey: usize,
        lp_value_name: *const u16,
        reserved: u32,
        dw_type: u32,
        lp_data: *const u8,
        cb_data: u32,
    ) -> i32;
    fn RegQueryValueExW(
        hkey: usize,
        lp_value_name: *const u16,
        lp_reserved: *mut u32,
        lp_type: *mut u32,
        lp_data: *mut u8,
        lpcb_data: *mut u32,
    ) -> i32;
    fn RegDeleteValueW(hkey: usize, lp_value_name: *const u16) -> i32;
    fn RegCloseKey(hkey: usize) -> i32;
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16 缓冲区。
fn wide_null(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// 当前可执行文件路径。
fn exe_path() -> Result<String, String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| format!("无法定位可执行文件路径：{e}"))
}

/// Run 键里应当写入的**命令行**。
///
/// 必须整体加引号：注册表 `Run` 的值是被当作命令行解析的。本机安装路径含空格
/// （`D:\Agent\interesting app\...`），若不加引号，Windows 会把空格之前当成
/// 可执行文件、之后当成参数 → 自启**静默失效**（注册表看起来写得没错）。
fn expected_command() -> Result<String, String> {
    Ok(format!("\"{}\"", exe_path()?))
}

/// 读取 Run 键里当前写入的命令行原文。不存在/读不到返回 `None`。
fn read_value() -> Option<String> {
    let key_wide = wide_null(RUN_KEY);
    let value_wide = wide_null(VALUE_NAME);

    let mut hkey: usize = 0;
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key_wide.as_ptr(), 0, KEY_QUERY_VALUE, &mut hkey) };
    if rc != ERROR_SUCCESS {
        return None;
    }

    let mut buf = [0u16; 2048];
    let mut len: u32 = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
    let query_rc = unsafe {
        RegQueryValueExW(
            hkey,
            value_wide.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast::<u8>(),
            &mut len,
        )
    };
    unsafe {
        RegCloseKey(hkey);
    }
    if query_rc != ERROR_SUCCESS {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..chars]);
    Some(s.trim_end_matches('\0').to_string())
}

/// 开启自启：写入 HKCU Run 键。
pub fn enable() -> Result<(), String> {
    let command = expected_command()?;
    let key_wide = wide_null(RUN_KEY);
    let value_wide = wide_null(VALUE_NAME);
    let data_wide = wide_null(&command);

    let mut hkey: usize = 0;
    // SAFETY: key_wide 以 NUL 结尾；hkey 由成功调用初始化。
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key_wide.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) };
    if rc != ERROR_SUCCESS {
        return Err(format!("打开 Run 注册表键失败（错误码 {rc}）"));
    }

    // SAFETY: data 为 NUL 结尾的 UTF-16 字节串（含结尾 2 字节 NUL）。
    let data_bytes = data_wide.as_ptr().cast::<u8>();
    let data_len = u32::try_from(data_wide.len() * 2).unwrap_or(u32::MAX);
    let set_rc = unsafe {
        RegSetValueExW(hkey, value_wide.as_ptr(), 0, REG_SZ, data_bytes, data_len)
    };
    // SAFETY: hkey 已成功打开，只关闭一次。
    unsafe {
        RegCloseKey(hkey);
    }

    if set_rc != ERROR_SUCCESS {
        return Err(format!("写入自启注册表失败（错误码 {set_rc}）"));
    }
    Ok(())
}

/// 关闭自启：删除 HKCU Run 值。值不存在视为成功。
pub fn disable() -> Result<(), String> {
    let key_wide = wide_null(RUN_KEY);
    let value_wide = wide_null(VALUE_NAME);

    let mut hkey: usize = 0;
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key_wide.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) };
    if rc != ERROR_SUCCESS {
        return Err(format!("打开 Run 注册表键失败（错误码 {rc}）"));
    }

    let del_rc = unsafe { RegDeleteValueW(hkey, value_wide.as_ptr()) };
    unsafe {
        RegCloseKey(hkey);
    }

    match del_rc {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        other => Err(format!("删除自启注册表失败（错误码 {other}）")),
    }
}

/// 当前自启是否已启用（且指向**当前**这个 exe）。
///
/// 不只是"键存在"：还比对写入的命令行是否等于当前 exe。否则程序升级/移动后
/// 注册表里会留一条指向旧路径的死记录，用户看到"已开启"但实际开机不会启动。
pub fn is_enabled() -> bool {
    match (read_value(), expected_command()) {
        (Some(actual), Ok(expected)) => actual == expected,
        _ => false,
    }
}

/// Run 键里是否存在本应用的值（无论路径是否有效）。
///
/// `is_enabled()` 为假但本函数为真 = 存在一条**失效**的自启记录，需要自愈。
pub fn has_stale_entry() -> bool {
    match (read_value(), expected_command()) {
        (Some(actual), Ok(expected)) => actual != expected,
        (Some(_), Err(_)) => true,
        _ => false,
    }
}

/// 按目标状态同步（幂等）：设置变更后调用。
///
/// 开启时写 Run 键（**顺带覆盖失效的旧记录**），关闭时删除 Run 值。
pub fn sync(target: bool) -> Result<(), String> {
    if target {
        if is_enabled() {
            Ok(())
        } else {
            // 不存在或指向旧路径 → 直接重写，实现升级自愈。
            enable()
        }
    } else if is_enabled() || has_stale_entry() {
        disable()
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 值名与路径正确() {
        assert_eq!(VALUE_NAME, "HdrSdrWidget");
        assert!(RUN_KEY.ends_with("CurrentVersion\\Run"));
    }

    #[test]
    fn utf16_宽字符串含结尾_nul() {
        let w = wide_null("abc");
        assert_eq!(w, [0x61, 0x62, 0x63, 0]);
    }

    /// 真机测试：开启→确认→关闭→确认。幂等安全。
    #[test]
    fn 真机_自启开关往返() {
        let _ = disable();
        assert!(!is_enabled());
        enable().expect("开启自启应成功");
        assert!(is_enabled());
        disable().expect("关闭自启应成功");
        assert!(!is_enabled());
    }
}
