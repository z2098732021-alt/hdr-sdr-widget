//! Click-through software dimmers, owned by one message-pumping thread.
use std::{collections::HashMap, sync::mpsc, time::Duration};
use windows::{
    core::w,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::*,
    },
};
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    DefWindowProcW(hwnd, msg, w, l)
}

pub fn alpha(percent: u8) -> u8 {
    (229.5 * (1.0 - f64::from(percent.min(100)) / 100.0)).round() as u8
}
pub fn transmission(percent: u8) -> f32 {
    1.0 - f32::from(alpha(percent)) / 255.0
}
enum Command {
    Set(String, RECT, u8, mpsc::SyncSender<Result<bool, String>>),
    Remove(String),
    Clear,
    Inspect(mpsc::SyncSender<Vec<Snapshot>>),
}
#[derive(serde::Serialize)]
pub struct Snapshot {
    pub key: String,
    pub alpha: u8,
    pub excluded: bool,
    pub click_through: bool,
    pub no_activate: bool,
    pub visible: bool,
}
pub struct Manager {
    tx: mpsc::Sender<Command>,
}
impl Manager {
    pub fn start(panel: isize) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("software-dimmer".into())
            .spawn(move || unsafe {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    lpszClassName: w!("HdrSdrSoftwareDimmer"),
                    hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    ..Default::default()
                };
                RegisterClassW(&class);
                let mut windows: HashMap<String, (HWND, bool)> = HashMap::new();
                loop {
                    match rx.recv_timeout(Duration::from_millis(16)) {
                        Ok(Command::Set(key, bounds, percent, reply)) => {
                            let result = (|| -> Result<bool, String> {
                                if percent >= 100 {
                                    if let Some((hwnd, _)) = windows.remove(&key) {
                                        let _ = DestroyWindow(hwnd);
                                    }
                                    return Ok(true);
                                }
                                if !windows.contains_key(&key) {
                                    let hwnd = CreateWindowExW(
                                        WS_EX_LAYERED
                                            | WS_EX_TRANSPARENT
                                            | WS_EX_NOACTIVATE
                                            | WS_EX_TOOLWINDOW
                                            | WS_EX_TOPMOST,
                                        w!("HdrSdrSoftwareDimmer"),
                                        w!("软件压暗"),
                                        WS_POPUP,
                                        bounds.left,
                                        bounds.top,
                                        bounds.right - bounds.left,
                                        bounds.bottom - bounds.top,
                                        None,
                                        None,
                                        None,
                                        None,
                                    )
                                    .map_err(|e| e.to_string())?;
                                    let excluded =
                                        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)
                                            .is_ok();
                                    windows.insert(key.clone(), (hwnd, excluded));
                                }
                                let (hwnd, excluded) = windows[&key];
                                SetLayeredWindowAttributes(
                                    hwnd,
                                    COLORREF(0),
                                    alpha(percent),
                                    LWA_ALPHA,
                                )
                                .map_err(|e| e.to_string())?;
                                SetWindowPos(
                                    hwnd,
                                    HWND_TOPMOST,
                                    bounds.left,
                                    bounds.top,
                                    bounds.right - bounds.left,
                                    bounds.bottom - bounds.top,
                                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                                )
                                .map_err(|e| e.to_string())?;
                                // The control remains above the dimmer without becoming foreground.
                                if panel != 0 {
                                    let _ = SetWindowPos(
                                        HWND(panel as *mut _),
                                        HWND_TOPMOST,
                                        0,
                                        0,
                                        0,
                                        0,
                                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                    );
                                }
                                Ok(excluded)
                            })();
                            let _ = reply.send(result);
                        }
                        Ok(Command::Remove(key)) => {
                            if let Some((h, _)) = windows.remove(&key) {
                                let _ = DestroyWindow(h);
                            }
                        }
                        Ok(Command::Inspect(reply)) => {
                            let result = windows
                                .iter()
                                .map(|(key, (h, _))| {
                                    let mut alpha = 0;
                                    let mut affinity = 0;
                                    let _ = GetLayeredWindowAttributes(
                                        *h,
                                        None,
                                        Some(&mut alpha),
                                        None,
                                    );
                                    let _ = GetWindowDisplayAffinity(*h, &mut affinity);
                                    let style = GetWindowLongPtrW(*h, GWL_EXSTYLE) as u32;
                                    Snapshot {
                                        key: key.clone(),
                                        alpha,
                                        excluded: affinity == WDA_EXCLUDEFROMCAPTURE.0,
                                        click_through: style & WS_EX_TRANSPARENT.0 != 0,
                                        no_activate: style & WS_EX_NOACTIVATE.0 != 0,
                                        visible: IsWindowVisible(*h).as_bool(),
                                    }
                                })
                                .collect();
                            let _ = reply.send(result);
                        }
                        Ok(Command::Clear) => {
                            for (_, (h, _)) in windows.drain() {
                                let _ = DestroyWindow(h);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                for (_, (h, _)) in windows {
                    let _ = DestroyWindow(h);
                }
            })
            .expect("software dimmer thread");
        Self { tx }
    }
    pub fn set(&self, key: &str, bounds: RECT, percent: u8) -> Result<bool, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.tx
            .send(Command::Set(key.into(), bounds, percent, tx))
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(Duration::from_secs(1))
            .map_err(|_| "软件压暗窗口响应超时".to_owned())?
    }
    pub fn remove(&self, key: &str) {
        let _ = self.tx.send(Command::Remove(key.into()));
    }
    pub fn inspect(&self) -> Vec<Snapshot> {
        let (tx, rx) = mpsc::sync_channel(1);
        let _ = self.tx.send(Command::Inspect(tx));
        rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default()
    }
    pub fn clear(&self) {
        let _ = self.tx.send(Command::Clear);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dimmer_never_blacks_out_and_100_removes_it() {
        assert_eq!(alpha(0), 230);
        assert_eq!(alpha(50), 115);
        assert_eq!(alpha(100), 0);
        assert!(transmission(0) > 0.09);
        for p in 0..100 {
            assert!(alpha(p) >= alpha(p + 1));
        }
    }
}
