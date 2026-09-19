//! DDC/CI VCP 0x10. Every operation rebinds its target and owns its handles.
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};
use windows::Win32::Devices::Display::*;

#[derive(Clone, Copy, Debug)]
pub struct Level {
    pub current: u32,
    pub maximum: u32,
}
impl Level {
    pub fn percent(self) -> u8 {
        ((u64::from(self.current) * 100 + u64::from(self.maximum) / 2) / u64::from(self.maximum))
            .min(100) as u8
    }
    pub fn value(self, percent: u8) -> u32 {
        ((u64::from(self.maximum) * u64::from(percent.min(100)) + 50) / 100) as u32
    }
}
struct Physical(Vec<PHYSICAL_MONITOR>);
impl Drop for Physical {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyPhysicalMonitors(&self.0);
        }
    }
}

fn operation(key: &str, percent: Option<u8>) -> Result<Level, String> {
    let (monitor, _) = super::display::monitor_for_key(key).map_err(|e| e.user_message())?;
    unsafe {
        let mut count = 0;
        GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count).map_err(|e| e.to_string())?;
        if count != 1 {
            return Err(format!("无法唯一绑定 DDC 物理显示器（{count} 个）"));
        }
        let mut handles = vec![PHYSICAL_MONITOR::default(); count as usize];
        GetPhysicalMonitorsFromHMONITOR(monitor, &mut handles).map_err(|e| e.to_string())?;
        let handles = Physical(handles);
        let handle = handles.0[0].hPhysicalMonitor;
        let read = || -> Result<Level, String> {
            let (mut current, mut maximum) = (0, 0);
            if GetVCPFeatureAndVCPFeatureReply(handle, 0x10, None, &mut current, Some(&mut maximum))
                == 0
            {
                return Err("DDC/CI 亮度读取失败；请检查显示器 DDC/CI 开关和连接方式".into());
            }
            if maximum == 0 || current > maximum {
                return Err("DDC/CI 返回了无效亮度范围".into());
            }
            Ok(Level { current, maximum })
        };
        let initial = read()?;
        if let Some(percent) = percent {
            let wanted = initial.value(percent);
            if initial.current == wanted {
                return Ok(initial);
            }
            if SetVCPFeature(handle, 0x10, wanted) == 0 {
                return Err("DDC/CI 亮度写入失败".into());
            }
            let mut actual = initial;
            for delay in [100, 150, 250] {
                std::thread::sleep(Duration::from_millis(delay));
                actual = read()?;
                if actual.current == wanted {
                    break;
                }
            }
            // A successful API return with an unchanged value is not proof of a write.
            if actual.current == initial.current {
                return Err("DDC/CI 写入后亮度未变化".into());
            }
            Ok(actual)
        } else {
            Ok(initial)
        }
    }
}

/// Windows offers no cancellation for a synchronous monitor driver call. Keep at
/// most one in-flight call per display; time out the waiter, never kill its thread.
#[derive(Default, Clone)]
pub struct Client {
    busy: Arc<AtomicBool>,
}
impl Client {
    pub fn call(&self, key: &str, percent: Option<u8>) -> Result<Level, String> {
        let key = key.to_owned();
        self.execute(move || operation(&key, percent), Duration::from_secs(2))
    }
    fn execute(
        &self,
        work: impl FnOnce() -> Result<Level, String> + Send + 'static,
        timeout: Duration,
    ) -> Result<Level, String> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err("DDC/CI 驱动仍在等待响应".into());
        }
        let busy = self.busy.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        if let Err(e) = std::thread::Builder::new()
            .name("ddc-io".into())
            .spawn(move || {
                let result = work();
                busy.store(false, Ordering::Release);
                let _ = tx.send(result);
            })
        {
            self.busy.store(false, Ordering::Release);
            return Err(e.to_string());
        }
        rx.recv_timeout(timeout)
            .map_err(|_| "DDC/CI 响应超时".to_owned())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_range_roundtrip() {
        for max in [1, 20, 100, 255, 65535, u32::MAX] {
            let l = Level {
                current: max,
                maximum: max,
            };
            assert_eq!(l.percent(), 100);
            assert_eq!(l.value(0), 0);
            assert_eq!(l.value(100), max);
            for p in 0..=100 {
                assert!(l.value(p) <= max);
            }
        }
        assert_eq!(
            Level {
                current: 128,
                maximum: 255
            }
            .percent(),
            50
        );
    }
    #[test]
    fn busy_call_does_not_start_another_driver_operation() {
        let client = Client::default();
        client.busy.store(true, Ordering::Release);
        assert!(client
            .call("not-a-display", None)
            .unwrap_err()
            .contains("仍在等待"));
    }
    #[test]
    fn timeout_discards_late_reply_and_recovers_without_overlapping_calls() {
        let client = Client::default();
        let (release, wait) = mpsc::channel();
        let result = client.execute(
            move || {
                wait.recv().unwrap();
                Ok(Level {
                    current: 20,
                    maximum: 100,
                })
            },
            Duration::from_millis(10),
        );
        assert!(result.unwrap_err().contains("超时"));
        assert!(client
            .execute(|| panic!("must not run"), Duration::from_secs(1))
            .is_err());
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while client.busy.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let result = client
            .execute(
                || {
                    Ok(Level {
                        current: 70,
                        maximum: 100,
                    })
                },
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(result.percent(), 70);
    }
}
