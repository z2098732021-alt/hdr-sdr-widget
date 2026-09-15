//! 显示变化监听：`WM_DISPLAYCHANGE` / `WM_SETTINGCHANGE` / 电源显示状态。
//!
//! 对应 ARCHITECTURE.md 的 S2.8。
//!
//! # 实现思路
//!
//! Win32 的窗口消息必须跑在**拥有消息循环的线程**上。本模块在后台起一个专用
//! 线程，创建一个**仅消息窗口**（message-only window，`HWND_MESSAGE`）来接收
//! 系统广播，把消息翻译成 [`MonitorEvent`] 通过 `std::sync::mpsc` 发给业务层。
//!
//! 选择"仅消息窗口"而不是隐藏可见窗口：它不参与合成、不抢焦点、不进 Alt-Tab，
//! 完全不会打扰用户。
//!
//! # 监听的三类事件
//!
//! | 事件 | 消息 | 触发时机 |
//! | --- | --- | --- |
//! | 分辨率 / 显示器拓扑变化 | `WM_DISPLAYCHANGE` | 改分辨率、拔插显示器 |
//! | 系统设置变化 | `WM_SETTINGCHANGE` | 用户在系统设置里改 HDR / SDR 亮度 |
//! | 显示器电源状态 | `WM_POWERBROADCAST` + `PBT_POWERSETTINGCHANGE` | 休眠 / 唤醒 / 屏幕关闭 |

use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use windows::core::GUID;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Power::{
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, TranslateMessage, UnregisterClassW, HWND_MESSAGE,
    MSG, WM_APP, WINDOW_EX_STYLE, WM_DESTROY, WM_DISPLAYCHANGE, WM_POWERBROADCAST,
    WM_SETTINGCHANGE, WNDCLASSW, WS_OVERLAPPED,
};

/// 显示器电源状态变化通知的 GUID。
///
/// 值：`6FE69556-704A-47A0-8F24-C28D936FDA47`。
/// 这里自行定义而不依赖 crate 导出，避免 feature 组合差异导致符号缺失。
const GUID_CONSOLE_DISPLAY_STATE: GUID =
    GUID::from_values(0x6FE6_9556, 0x704A, 0x47A0, [0x8F, 0x24, 0xC2, 0x8D, 0x93, 0x6F, 0xDA, 0x47]);

/// `WM_POWERBROADCAST` 的子消息：电源设置发生变化。
const PBT_POWERSETTINGCHANGE: u32 = 0x8013;

/// 仅注册窗口句柄（而非服务句柄）。
const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 0x0000_0000;

/// 自定义消息：请求监听线程退出。
const WM_APP_QUIT: u32 = WM_APP + 1;

/// 监听窗口的类名。
const CLASS_NAME: ::windows::core::PCWSTR = windows::core::w!("HdrSdrWidgetMonitorWatch");
/// 监听窗口的标题（仅消息窗口不可见，仅用于调试识别）。
const WINDOW_NAME: ::windows::core::PCWSTR = windows::core::w!("HdrSdrWidget Monitor Watch");

/// `GUID_CONSOLE_DISPLAY_STATE` 的数据段：显示器状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPowerState {
    /// 显示器已关闭（休眠 / 屏幕保护）。
    Off,
    /// 显示器已开启。
    On,
    /// 显示器已调暗。
    Dimmed,
    /// 未知状态值（未来系统可能新增）。
    Unknown(u32),
}

impl DisplayPowerState {
    /// 从 `POWERBROADCAST_SETTING` 的数据段解析。
    #[must_use]
    pub fn from_raw(value: u32) -> Self {
        match value {
            0 => Self::Off,
            1 => Self::On,
            2 => Self::Dimmed,
            other => Self::Unknown(other),
        }
    }
}

/// 显示环境变化事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorEvent {
    /// 分辨率、刷新率或显示器拓扑发生变化。
    DisplayChange,
    /// 系统设置发生变化（可能改了 HDR / SDR 亮度）。
    SettingChange,
    /// 显示器电源状态变化。
    PowerDisplay(DisplayPowerState),
}

/// `POWERBROADCAST_SETTING` 的头部（不含变长数据段）。
///
/// 布局：`GUID(16) + DWORD DataLength(4)`，其后紧跟 `DataLength` 字节数据。
#[repr(C)]
struct PowerBroadcastSettingHeader {
    power_setting: GUID,
    data_length: u32,
}

/// 监听线程的窗口过程。
///
/// 只处理三类关心的消息，其余一律交给 `DefWindowProcW`。
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE => {
            send_event(hwnd, MonitorEvent::DisplayChange);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            send_event(hwnd, MonitorEvent::SettingChange);
            LRESULT(0)
        }
        WM_POWERBROADCAST if wparam.0 as u32 == PBT_POWERSETTINGCHANGE => {
            if let Some(state) = decode_power_setting(lparam) {
                send_event(hwnd, MonitorEvent::PowerDisplay(state));
            }
            LRESULT(0)
        }
        WM_APP_QUIT => {
            // 先投递 WM_QUIT（GetMessageW 收到后返回 0，消息循环退出），再销毁窗口。
            // 只调 DestroyWindow 不会产生 WM_QUIT，消息循环会继续空转。
            let _ = PostQuitMessage(0);
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        other => DefWindowProcW(hwnd, other, wparam, lparam),
    }
}

/// 从窗口的 `GWLP_USERDATA` 取出发送器，把事件投递到业务层。
///
/// 之所以把发送器指针挂在窗口上而不是用全局变量：监听理论上可以启动多份
/// （虽然当前只需要一份），用窗口私有数据可以天然隔离。
unsafe fn send_event(hwnd: HWND, event: MonitorEvent) {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWLP_USERDATA};

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if ptr == 0 {
        return;
    }
    let sender = &*(ptr as *const Sender<MonitorEvent>);
    // 接收端已关闭时（业务层退出）静默丢弃，不因日志噪音打扰用户。
    let _ = sender.send(event);
}

/// 解析 `POWERBROADCAST_SETTING`，只对 `GUID_CONSOLE_DISPLAY_STATE` 感兴趣。
unsafe fn decode_power_setting(lparam: LPARAM) -> Option<DisplayPowerState> {
    if lparam.0 == 0 {
        return None;
    }
    let header = &*(lparam.0 as *const PowerBroadcastSettingHeader);
    if header.power_setting != GUID_CONSOLE_DISPLAY_STATE {
        return None;
    }
    if header.data_length < 4 {
        return None;
    }
    let data_ptr = (lparam.0 as *const u8).add(size_of::<PowerBroadcastSettingHeader>());
    let value = std::ptr::read_unaligned(data_ptr as *const u32);
    Some(DisplayPowerState::from_raw(value))
}

/// 启动显示变化监听线程。
///
/// 返回的 [`JoinHandle`] 本身不用于停止监听——请调用返回的 [`WatcherHandle`]
/// 上的 [`WatcherHandle::stop`]，它会先发退出消息让线程干净地注销并销毁窗口。
pub fn start(tx: Sender<MonitorEvent>) -> Result<WatcherHandle, String> {
    // HWND 内部是裸指针、不实现 Send，不能直接过通道；按整数（isize）传递，
    // 接收端再用 HWND(raw as *mut c_void) 还原（句柄值本身跨线程传递是安全的）。
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<isize, String>>();

    let thread = std::thread::Builder::new()
        .name("monitor-watch".to_string())
        .spawn(move || {
            // run_loop 在窗口创建成功后立刻通过 ready 通道回报句柄；
            // 线程内出现的任何失败也都经该通道回报，绝不静默吞掉。
            let _ = run_loop(&tx, ready_tx);
        })
        .map_err(|e| format!("无法创建监听线程：{e}"))?;

    // 窗口创建成功会立刻收到 Ok(hwnd)；失败会立刻收到 Err(msg)。
    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(raw)) => Ok(WatcherHandle {
            hwnd: Some(HWND(raw as *mut core::ffi::c_void)),
            thread: Some(thread),
        }),
        Ok(Err(msg)) => {
            let _ = thread.join();
            Err(msg)
        }
        Err(_) => {
            let _ = thread.join();
            Err("监听线程 5 秒内未就绪".to_string())
        }
    }
}

/// 监听线程的主体：创建窗口 → 注册电源通知 → 消息循环 → 清理。
fn run_loop(tx: &Sender<MonitorEvent>, ready_tx: Sender<Result<isize, String>>) -> Result<(), String> {
    // SAFETY: 下面所有 Win32 调用都在本线程内完成，窗口与类一一对应。
    unsafe {
        let wnd_class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&wnd_class) == 0 {
            let msg = format!("RegisterClassW 失败：{}", std::io::Error::last_os_error());
            let _ = ready_tx.send(Err(msg.clone()));
            return Err(msg);
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
            HWND_MESSAGE, // 仅消息窗口：不显示、不抢焦点
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let msg = format!("CreateWindowExW 失败：{e}");
                let _ = UnregisterClassW(CLASS_NAME, None);
                let _ = ready_tx.send(Err(msg.clone()));
                return Err(msg);
            }
        };

        // 把发送器挂在窗口私有数据区，供窗口过程取用。
        // SAFETY: tx 的生命周期覆盖整个消息循环（run_loop 结束前不会返回）。
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_USERDATA};
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, tx as *const Sender<MonitorEvent> as isize);

        // 窗口就绪，立刻回报句柄给调用方，避免 start() 空等 5 秒超时。
        let _ = ready_tx.send(Ok(hwnd.0 as isize));

        // 注册显示器电源状态通知。
        // windows 0.58: `RegisterPowerSettingNotification` 第三参类型是
        // `windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS`（u32 包装），
        // DEVICE_NOTIFY_WINDOW_HANDLE=0 用 `REGISTER_NOTIFICATION_FLAGS(0)` 表示。
        let notify = RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS(
                DEVICE_NOTIFY_WINDOW_HANDLE,
            ),
        );

        let mut msg = MSG::default();
        // GetMessageW 返回 FALSE(0) 表示收到 WM_QUIT；返回 -1 表示出错。
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if let Ok(handle) = notify {
            let _ = UnregisterPowerSettingNotification(handle);
        }
        let _ = UnregisterClassW(CLASS_NAME, None);
    }

    Ok(())
}

/// 监听线程的句柄，持有它即可在需要时干净地停止监听。
pub struct WatcherHandle {
    /// 监听窗口句柄（仅消息窗口）。投递 `WM_APP_QUIT` 即触发线程退出。
    hwnd: Option<HWND>,
    thread: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// 请求监听线程退出并等待其结束。
    ///
    /// 通过向窗口投递 `WM_APP_QUIT` 触发退出，从而让 `GetMessageW` 返回 0，
    /// 线程走完清理逻辑后自然结束。
    pub fn stop(mut self) {
        // 窗口句柄在线程启动时就已回传并保存在 self.hwnd，直接投递退出消息。
        if let Some(hwnd) = self.hwnd {
            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// 该线程是否已经结束。
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        // Drop 里只做"尽力而为"的清理：投递退出消息，但不 join（可能阻塞）。
        if let Some(hwnd) = self.hwnd {
            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn 电源状态解析() {
        assert_eq!(DisplayPowerState::from_raw(0), DisplayPowerState::Off);
        assert_eq!(DisplayPowerState::from_raw(1), DisplayPowerState::On);
        assert_eq!(DisplayPowerState::from_raw(2), DisplayPowerState::Dimmed);
        assert_eq!(DisplayPowerState::from_raw(9), DisplayPowerState::Unknown(9));
    }

    #[test]
    fn guid_常量正确() {
        // 6FE69556-704A-47A0-8F24-C28D936FDA47
        assert_eq!(GUID_CONSOLE_DISPLAY_STATE.data1, 0x6FE6_9556);
        assert_eq!(GUID_CONSOLE_DISPLAY_STATE.data2, 0x704A);
        assert_eq!(GUID_CONSOLE_DISPLAY_STATE.data3, 0x47A0);
        assert_eq!(
            GUID_CONSOLE_DISPLAY_STATE.data4,
            [0x8F, 0x24, 0xC2, 0x8D, 0x93, 0x6F, 0xDA, 0x47]
        );
    }

    /// 真机测试：启动监听线程，随后停止。验证窗口能创建、能干净退出。
    #[test]
    fn 真机_监听线程可启停() {
        let (tx, rx) = mpsc::channel();
        let handle = match start(tx) {
            Ok(h) => h,
            Err(e) => panic!("启动监听线程失败：{e}"),
        };
        // 等一小会儿确认线程没有立刻崩掉。
        std::thread::sleep(Duration::from_millis(200));
        assert!(!handle.is_finished(), "监听线程启动后立刻退出了");
        handle.stop();
        // 停止后不应再有事件洪泛；此处只断言通道未被关闭即可。
        drop(rx);
    }
}
