//! 全局快捷键：`RegisterHotKey` + 消息窗口线程。
//!
//! PRD P0-2：任意前台应用下生效；注册前探测冲突，失败时通过 Toast 提示而非 panic。
//! 架构决策 D8：注册失败不 panic，降级为「仅托盘可用」。
//!
//! 为避免依赖 `windows` crate 的 `Win32_UI_Input_KeyboardAndMouse` feature，
//! `RegisterHotKey` / `UnregisterHotKey` 与 `MOD_*` / `VK_*` 常量均在此手写 extern。

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tauri::{Emitter, Manager};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, PostQuitMessage,
    RegisterClassW, UnregisterClassW, HWND_MESSAGE, MSG, WM_APP, WINDOW_EX_STYLE, WM_DESTROY,
    WM_HOTKEY, WNDCLASSW, WS_OVERLAPPED,
};

use crate::commands::{self, AppState};
use crate::window;

// --- 热键修饰键常量 ---
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
/// 阻止按住时连续触发（降低拖动键盘连发）。
pub const MOD_NOREPEAT: u32 = 0x4000;

/// `ERROR_HOTKEY_ALREADY_REGISTERED`。
/// `windows::Win32::Foundation::GetLastError()` 返回的 `Win32Error.0` 是 u32。
pub const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;

#[link(name = "user32")]
extern "system" {
    // `HWND` 是 `#[repr(transparent)]` 指针包装；NULL 用 `HWND::default()`/空指针表达。
    fn RegisterHotKey(hwnd: HWND, id: i32, fs_modifiers: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hwnd: HWND, id: i32) -> i32;
}

/// 消息窗口类名。`w!` 宏直接产出 `PCWSTR`（不是 `&PCWSTR`）。
const CLASS_NAME: windows::core::PCWSTR = windows::core::w!("HdrSdrWidgetHotkeyWatch");
const WINDOW_NAME: windows::core::PCWSTR = windows::core::w!("HdrSdrWidget Hotkey");

/// 自定义消息：请求线程退出。
const WM_APP_QUIT: u32 = WM_APP + 1;

/// 热键消息携带的注册 ID。
const HOTKEY_ID: i32 = 1;

/// 把修饰键 + 虚拟键组装成可读字符串（诊断/冲突提示用）。
#[must_use]
pub fn describe(mods: u32, vk: u32) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if mods & MOD_CONTROL != 0 {
        parts.push("Ctrl");
    }
    if mods & MOD_ALT != 0 {
        parts.push("Alt");
    }
    if mods & MOD_SHIFT != 0 {
        parts.push("Shift");
    }
    if mods & MOD_WIN != 0 {
        parts.push("Win");
    }
    if let Some(name) = vk_name(vk) {
        parts.push(name);
    } else {
        // `format!` 是临时值，泄漏成 'static 后借用（数量有限，安全）。
        let leaked: &'static str = &*Box::leak(format!("键码 {vk}").into_boxed_str());
        parts.push(leaked);
    }
    parts.join("+")
}

/// 虚拟键 → 单字符/名字（仅覆盖字母、数字与常用功能键）。
fn vk_name(vk: u32) -> Option<&'static str> {
    match vk {
        0x30..=0x39 => Some(match vk {
            0x30 => "0",
            0x31 => "1",
            0x32 => "2",
            0x33 => "3",
            0x34 => "4",
            0x35 => "5",
            0x36 => "6",
            0x37 => "7",
            0x38 => "8",
            _ => "9",
        }),
        0x41..=0x5A => {
            // A-Z → 显示大写字母。
            let ch = (b'A' + (vk as u8 - 0x41)) as char;
            // 需要返回 'static str，这里转成泄漏的字节串（数量有限，安全）。
            let s: String = ch.to_string();
            Some(&*Box::leak(s.into_boxed_str()))
        }
        0x70..=0x87 => {
            let n = vk - 0x6F;
            let s: String = format!("F{n}");
            Some(&*Box::leak(s.into_boxed_str()))
        }
        _ => None,
    }
}

/// 把「Ctrl+Alt+B」字符串解析成 (modifiers, vk)。
/// 失败返回带中文说明的错误。
pub fn parse_hotkey(input: &str) -> Result<(u32, u32), String> {
    let mut mods: u32 = 0;
    let mut key: Option<u32> = None;

    for token in input.split('+') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "windows" | "super" => mods |= MOD_WIN,
            _ => {
                if key.is_some() {
                    return Err(format!("热键「{input}」包含多个主键（{token}）"));
                }
                key = Some(parse_key(token)?);
            }
        }
    }

    match key {
        Some(vk) if mods != 0 => Ok((mods | MOD_NOREPEAT, vk)),
        Some(_) => Err("热键至少需要一个修饰键（Ctrl/Alt/Shift/Win）".to_string()),
        None => Err(format!("热键「{input}」缺少主键")),
    }
}

/// 解析单个主键（字母 / 数字 / 功能键）。
fn parse_key(token: &str) -> Result<u32, String> {
    let upper = token.to_ascii_uppercase();
    let mut chars = upper.chars();
    if let Some(c) = chars.next() {
        if chars.next().is_none() {
            if c.is_ascii_alphanumeric() {
                return Ok(c as u32);
            }
            return Err(format!("不支持的主键：{token}"));
        }
    }
    match upper.as_str() {
        "F1" => Ok(0x70),
        "F2" => Ok(0x71),
        "F3" => Ok(0x72),
        "F4" => Ok(0x73),
        "F5" => Ok(0x74),
        "F6" => Ok(0x75),
        "F7" => Ok(0x76),
        "F8" => Ok(0x77),
        "F9" => Ok(0x78),
        "F10" => Ok(0x79),
        "F11" => Ok(0x7A),
        "F12" => Ok(0x7B),
        _ => Err(format!("不支持的主键：{token}")),
    }
}

/// 热键监听线程句柄。
///
/// `hwnd` 由监听线程在创建成功后**经 ready 通道回传**。
/// 为什么不用 `EnumWindows` 去找：热键窗口是 `HWND_MESSAGE`（仅消息窗口），
/// 而 `EnumWindows` 只枚举顶层可见窗口，**永远枚举不到仅消息窗口**。
/// 旧实现据此查找 → 恒返回 `None` → 退出消息投不出去 → `thread.join()`
/// 永久阻塞 → 改/关热键时热键互斥锁死锁。
/// （`core/monitor_watch.rs` 修过完全相同的坑，本文件当时漏了。）
pub struct HotkeyHandle {
    thread: Option<JoinHandle<()>>,
    /// 监听线程的消息窗口句柄；0 = 尚未就绪。
    hwnd: Arc<AtomicIsize>,
}

impl HotkeyHandle {
    /// 请求监听线程退出并等待结束。
    ///
    /// **注意**：本方法会 `join`，调用方必须确保没有持有 `CURRENT` 锁，
    /// 否则会阻塞所有热键相关 IPC。
    pub fn stop(&mut self) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw != 0 {
            // SAFETY: raw 由监听线程写入，窗口在 join 前不会被销毁。
            unsafe {
                let _ = PostMessageW(HWND(raw as *mut _), WM_APP_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 全局唯一的当前热键句柄。
static CURRENT: Mutex<Option<HotkeyHandle>> = Mutex::new(None);

// 热键回调，挂在本线程（即热键监听线程）的局部存储，供窗口过程取用。
thread_local! {
    static PRESS: RefCell<Option<Box<dyn Fn() + Send>>> = const { RefCell::new(None) };
}

/// 注册全局热键（幂等：重复调用先注销旧的）。
///
/// `on_press` 在热键线程中执行；失败时通过 `toast:show` 事件提示冲突。
pub fn register(app: &tauri::AppHandle, hotkey: &str, on_press: impl Fn() + Send + 'static) {
    // 先停止旧的注册。
    //
    // 关键：必须**先把句柄 take 出来、再解锁、最后 stop()**。
    // 旧写法 `if let Some(mut old) = CURRENT.lock().unwrap().take() { old.stop(); }`
    // 中，`if let` 条件里的临时 `MutexGuard` 会存活到整个 `if let` 语句结束
    // （含花括号体），于是 `stop()` 的 `join()` 在**持锁状态下**执行。
    // 叠加"线程永不退出"的缺陷 → 热键互斥锁永久死锁，所有热键相关 IPC 挂死。
    let old = CURRENT.lock().unwrap().take();
    if let Some(mut old) = old {
        old.stop();
    }

    let (mods, vk) = match parse_hotkey(hotkey) {
        Ok(v) => v,
        Err(msg) => {
            let _ = app.emit(
                "toast:show",
                ("HOTKEY_PARSE", format!("热键「{hotkey}」无效：{msg}")),
            );
            return;
        }
    };

    match start(mods, vk, on_press) {
        Ok(handle) => {
            *CURRENT.lock().unwrap() = Some(handle);
        }
        Err(msg) => {
            let _ = app.emit("toast:show", ("HOTKEY_CONFLICT", msg));
        }
    }
}

/// 按当前配置重注册热键（设置变更后调用）。
///
/// v2：`settings.hotkey` 为空串 = 禁用（默认关，P1-3/U4）。改空时停掉旧注册。
pub fn reload(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let hotkey = commands::with_settings(&state, |s| s.hotkey.clone());
    if hotkey.trim().is_empty() {
        // 同 `register`：先 take、解锁，再 stop，避免持锁 join。
        let old = CURRENT.lock().unwrap().take();
        if let Some(mut old) = old {
            old.stop();
        }
        return;
    }
    let app = app.clone();
    // 闭包 move 一份独立 clone，`register` 持有 &app 借用时互不冲突。
    let app_for_cb = app.clone();
    register(&app, &hotkey, move || {
        let state = app_for_cb.state::<AppState>();
        let _ = window::toggle(&app_for_cb, &state);
    });
}

/// 启动热键监听线程。
///
/// ready 通道在**注册成功后、进入消息循环前**发送窗口句柄原始值（`isize`，
/// 规避 `HWND` 非 `Send`），因此主线程无需等到消息循环结束。
/// 旧实现在 `run_loop` **返回之后**才 send —— 而 `run_loop` 要等消息循环退出
/// 才返回 —— 于是成功路径必然等满 5 秒超时，启动/保存设置会卡 5 秒。
fn start(mods: u32, vk: u32, on_press: impl Fn() + Send + 'static) -> Result<HotkeyHandle, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<isize, String>>();

    let thread = std::thread::Builder::new()
        .name("hotkey-watch".to_string())
        .spawn(move || {
            // 回调必须在热键线程内设置，窗口过程才能从同一线程的 thread_local 取到。
            PRESS.with(|slot| *slot.borrow_mut() = Some(Box::new(on_press)));
            run_loop(mods, vk, ready_tx);
        })
        .map_err(|e| format!("无法创建热键线程：{e}"))?;

    // 正常路径下窗口创建 + 注册热键是微秒级；5 秒只作为兜底上限。
    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(hwnd_raw)) => Ok(HotkeyHandle {
            thread: Some(thread),
            hwnd: Arc::new(AtomicIsize::new(hwnd_raw)),
        }),
        Ok(Err(msg)) => {
            // 注册失败（多为热键冲突）→ 线程已结束，join 回收。
            let _ = thread.join();
            Err(msg)
        }
        Err(_) => Err("热键线程启动超时（5 秒内未就绪）".to_string()),
    }
}

/// 监听线程主体：创建消息窗口 → 注册热键 → 回传句柄 → 消息循环。
fn run_loop(mods: u32, vk: u32, ready: std::sync::mpsc::Sender<Result<isize, String>>) {
    unsafe {
        let wnd_class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&wnd_class) == 0 {
            let _ = ready.send(Err(format!(
                "RegisterClassW 失败：{}",
                std::io::Error::last_os_error()
            )));
            return;
        }

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            WINDOW_NAME,
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = UnregisterClassW(CLASS_NAME, None);
                let _ = ready.send(Err(format!("CreateWindowExW 失败：{e}")));
                return;
            }
        };

        // 注册全局热键；失败（冲突/权限）则回传中文提示。
        let rc = RegisterHotKey(hwnd, HOTKEY_ID, mods, vk);
        if rc == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            let _ = UnregisterClassW(CLASS_NAME, None);
            let msg = if err.0 == ERROR_HOTKEY_ALREADY_REGISTERED {
                format!(
                    "全局热键 {} 已被其他程序占用，无法注册。你可以在设置中更换热键。",
                    describe(mods, vk)
                )
            } else {
                format!("注册全局热键失败（错误码 {}）", err.0)
            };
            let _ = ready.send(Err(msg));
            return;
        }

        // 就绪：先回传句柄，主线程即可继续；随后进入消息循环。
        if ready.send(Ok(hwnd.0 as isize)).is_err() {
            // 主线程已放弃等待（超时/退出）：立即收尾，不留悬挂热键。
            let _ = UnregisterHotKey(hwnd, HOTKEY_ID);
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            let _ = UnregisterClassW(CLASS_NAME, None);
            return;
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnregisterHotKey(hwnd, HOTKEY_ID);
        let _ = UnregisterClassW(CLASS_NAME, None);
    }
}

/// 窗口过程：处理 WM_HOTKEY 与退出消息。
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
            PRESS.with(|slot| {
                if let Some(cb) = slot.borrow().as_ref() {
                    cb();
                }
            });
            LRESULT(0)
        }
        WM_APP_QUIT => {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            // 必须补 PostQuitMessage：`GetMessageW` 只在取到 `WM_QUIT` 时返回 0，
            // 仅销毁窗口不会产生 `WM_QUIT` → 消息循环永不退出 → `join()` 永久阻塞。
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        other => DefWindowProcW(hwnd, other, wparam, lparam),
    }
}

/// 按标题定位热键消息窗口（**已废弃**）。
///
/// 保留此说明而非代码，是为了防止后人再次踩同一个坑：
/// 热键窗口是 `HWND_MESSAGE` 仅消息窗口，`EnumWindows` **只枚举顶层可见窗口**，
/// 永远枚举不到它，函数恒返回 `None`。正确做法是创建成功后经通道回传句柄
/// （见 [`start`] / [`run_loop`]）。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析标准组合键() {
        let (mods, vk) = parse_hotkey("Ctrl+Alt+B").unwrap();
        assert_eq!(mods & MOD_CONTROL, MOD_CONTROL);
        assert_eq!(mods & MOD_ALT, MOD_ALT);
        assert_eq!(vk, 0x42);
    }

    #[test]
    fn 解析大写与小写等价() {
        let (m1, k1) = parse_hotkey("ctrl+alt+b").unwrap();
        let (m2, k2) = parse_hotkey("CTRL+ALT+B").unwrap();
        assert_eq!((m1, k1), (m2, k2));
    }

    #[test]
    fn 无修饰键被拒绝() {
        assert!(parse_hotkey("B").is_err());
    }

    #[test]
    fn 多主键被拒绝() {
        assert!(parse_hotkey("Ctrl+B+C").is_err());
    }

    #[test]
    fn 数字与功能键() {
        let (_, vk) = parse_hotkey("Ctrl+Alt+1").unwrap();
        assert_eq!(vk, 0x31);
        let (_, vk) = parse_hotkey("Ctrl+Alt+F12").unwrap();
        assert_eq!(vk, 0x7B);
    }

    #[test]
    fn 描述可读() {
        let (mods, vk) = parse_hotkey("Ctrl+Alt+B").unwrap();
        assert_eq!(describe(mods, vk), "Ctrl+Alt+B");
    }
}
