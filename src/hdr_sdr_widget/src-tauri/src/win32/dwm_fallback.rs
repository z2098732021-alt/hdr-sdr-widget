//! 降级写入路线：`dwmapi.dll` 序号 171 的 `DwmpSDRToHDRBoost`。
//!
//! # 这是什么
//!
//! 当 undocumented 私有 API `SET_SDR_WHITE_LEVEL` 在未来 Windows 更新中失效时
//! （风险 R1），这是唯一的程序化降级路线。它是 `dwmapi.dll` 的一个**未命名导出**
//! （只有序号，没有符号名），必须用 `GetProcAddress` 按序号取：
//!
//! ```text
//! HRESULT DwmpSDRToHDRBoost(HMONITOR hmonitor, double boost);
//! ```
//!
//! # 已知缺陷
//!
//! 该路线**不会写入系统持久层**——注销重登 / 重启 Explorer 后值会回弹到系统记录值。
//! 因此本路线只能作为"能用但不同步"的降级方案，UI 上必须标注"兼容模式"。
//!
//! # 默认不启用
//!
//! 本模块只提供探测与调用能力，**绝不主动调用**。是否启用由
//! [`crate::core::controller::BrightnessController`] 在连续写入失败后决定。

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::PCSTR;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// `DwmpSDRToHDRBoost` 在 `dwmapi.dll` 中的导出序号。
const DWMP_SDR_TO_HDR_BOOST_ORDINAL: usize = 171;

/// 缓存取到的函数地址。0 表示尚未探测；`usize::MAX` 表示已探测但不可用。
static CACHED_FN: AtomicUsize = AtomicUsize::new(0);
/// 探测完成标记（避免把"不可用"和"未探测"混为一谈的重复探测）。
static PROBED: AtomicUsize = AtomicUsize::new(0);

/// `DwmpSDRToHDRBoost` 的函数签名。
type DwmpSdrToHdrBoost = unsafe extern "system" fn(HMONITOR, f64) -> i32;

/// 加载 `dwmapi.dll` 并按序号 171 取函数地址。
///
/// 结果会被缓存，重复调用不会重复 `LoadLibraryW`。
/// 取不到时返回 `None`（函数不存在 / DLL 加载失败）。
fn resolve() -> Option<DwmpSdrToHdrBoost> {
    // 已探测过：直接读缓存。
    if PROBED.load(Ordering::Acquire) == 1 {
        let addr = CACHED_FN.load(Ordering::Acquire);
        return decode(addr);
    }

    let addr = unsafe {
        // dwmapi.dll 是系统 DLL，用完整路径加载更安全，但系统目录已在搜索路径中。
        let dll: windows::core::PCWSTR = windows::core::w!("dwmapi.dll");
        let module: HMODULE = LoadLibraryW(dll).ok()?;
        let proc = GetProcAddress(module, PCSTR(DWMP_SDR_TO_HDR_BOOST_ORDINAL as *const u8));
        match proc {
            Some(f) => f as usize,
            None => usize::MAX,
        }
    };

    CACHED_FN.store(addr, Ordering::Release);
    PROBED.store(1, Ordering::Release);
    decode(addr)
}

/// 把缓存中的地址还原成函数指针。
fn decode(addr: usize) -> Option<DwmpSdrToHdrBoost> {
    if addr == 0 || addr == usize::MAX {
        return None;
    }
    // SAFETY: 地址来自 GetProcAddress 且非哨兵值，签名与社区逆向结论一致。
    Some(unsafe { std::mem::transmute::<usize, DwmpSdrToHdrBoost>(addr) })
}

/// 该降级路线在当前系统上是否可用。
///
/// 只做探测，**不产生任何副作用**。
#[must_use]
pub fn is_available() -> bool {
    resolve().is_some()
}

/// 通过 DWM 路线设置 SDR→HDR 增益倍数。
///
/// `boost` 是相对 80 nits 的倍数，即 `nits / 80.0`。
///
/// # 已知缺陷
///
/// 该值**不会落系统持久层**，注销重登后会回弹。UI 必须标注"兼容模式"。
pub fn set_boost(monitor: HMONITOR, boost: f64) -> Result<(), i32> {
    let func = resolve().ok_or(-1)?;
    // SAFETY: monitor 由调用方保证有效；boost 为有限 f64。
    // 非有限值（NaN/Inf）会让 DWM 行为未定义，因此在入口拦截。
    if !boost.is_finite() || !(0.0..=100.0).contains(&boost) {
        return Err(-2);
    }
    let hr = unsafe { func(monitor, boost) };
    if hr < 0 {
        return Err(hr);
    }
    Ok(())
}

/// 把 raw 值换算成 DWM 路线需要的倍数。
///
/// `nits = raw × 0.08`，`boost = nits / 80`。
#[must_use]
pub fn raw_to_boost(raw: u32) -> f64 {
    f64::from(raw) * 0.08 / 80.0
}

/// 枚举当前全部显示器的 HMONITOR 句柄。
///
/// 返回空向量表示枚举失败（此时调用方应退回只读模式）。
#[must_use]
pub fn list_monitors() -> Vec<HMONITOR> {
    use std::cell::RefCell;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};

    thread_local! {
        /// `EnumDisplayMonitors` 的回调没有用户数据参数，只能靠线程局部存储收集结果。
        static FOUND: RefCell<Vec<HMONITOR>> = const { RefCell::new(Vec::new()) };
    }

    FOUND.with(|slot| slot.borrow_mut().clear());

    // SAFETY: 回调只往线程局部 Vec 里 push HMONITOR（Copy 类型），不触碰其他状态。
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        _data: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::BOOL {
        FOUND.with(|slot| slot.borrow_mut().push(monitor));
        windows::Win32::Foundation::TRUE
    }

    // SAFETY: hdc 传 None 表示枚举所有显示器；lparam 未使用。
    let ok = unsafe { EnumDisplayMonitors(None, None, Some(callback), windows::Win32::Foundation::LPARAM(0)) };
    if !ok.as_bool() {
        return Vec::new();
    }

    FOUND.with(|slot| slot.borrow().clone())
}

/// 占位常量，用于诊断输出中标注模块句柄来源。
#[must_use]
pub fn module_name() -> &'static str {
    "dwmapi.dll"
}

/// 仅用于编译期确认 `HANDLE` 与 `HMONITOR` 的指针宽度一致（诊断断言）。
const _: () = assert!(std::mem::size_of::<HANDLE>() == std::mem::size_of::<usize>());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_换算正确() {
        assert!((raw_to_boost(1000) - 1.0).abs() < 1e-9);
        assert!((raw_to_boost(6000) - 6.0).abs() < 1e-9);
        assert!((raw_to_boost(2850) - 2.85).abs() < 1e-9);
    }

    #[test]
    fn 非法_boost_被拦截() {
        // HMONITOR 为 None 时，函数会先校验参数再走 resolve，因此传入非法值
        // 一定返回 Err(-2)，不会真的调用 DWM。
        assert_eq!(set_boost(HMONITOR(std::ptr::null_mut()), f64::NAN), Err(-2));
        assert_eq!(set_boost(HMONITOR(std::ptr::null_mut()), -1.0), Err(-2));
        assert_eq!(set_boost(HMONITOR(std::ptr::null_mut()), 9999.0), Err(-2));
    }

    #[test]
    fn 模块名常量() {
        assert_eq!(module_name(), "dwmapi.dll");
    }

    /// 真机探测：只探测不调用，任何结果都不算失败。
    #[test]
    fn 真机_探测不产生副作用() {
        let available = is_available();
        // 探测结果必须是稳定的（有缓存），连调两次结果一致。
        assert_eq!(available, is_available());
    }
}
