//! Shared brightness service. Renderers never call display drivers or own policy.
use hdr_sdr_widget_lib::{
    core::{
        convert::{percent_to_raw, raw_to_nits, raw_to_percent},
        model::Percent,
    },
    win32::{ddc, display},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    #[default]
    Auto,
    Hdr,
    Ddc,
    Software,
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Backend {
    Hdr,
    Ddc,
    Software,
    #[default]
    Unavailable,
}
impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hdr => "HDR SDR 内容亮度",
            Self::Ddc => "DDC/CI 硬件亮度",
            Self::Software => "软件压暗",
            Self::Unavailable => "检测中 / 不可用",
        }
    }
}
pub fn choose(mode: Mode, hdr: bool, ddc: bool) -> Backend {
    match mode {
        Mode::Software => Backend::Software,
        Mode::Ddc if ddc => Backend::Ddc,
        Mode::Hdr if hdr => Backend::Hdr,
        _ if hdr => Backend::Hdr,
        _ if ddc => Backend::Ddc,
        _ => Backend::Software,
    }
}
#[derive(Clone, Default, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    pub key: String,
    pub name: String,
    pub id: u64,
    pub percent: u8,
    pub mode: Mode,
    pub backend: Backend,
    pub can_control: bool,
    pub raw: Option<u32>,
    pub nits: Option<f64>,
    #[serde(rename = "hdrEnabled")]
    pub hdr: bool,
    pub hdr_supported: bool,
    pub sdr_white_raw: u32,
    pub ddc_available: bool,
    pub ddc_error: Option<String>,
    pub fallback_reason: Option<String>,
    pub error: Option<String>,
    pub confirmed: bool,
    pub write_ms: f64,
    pub software_transmission: f32,
    pub capture_safe: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WriteResult {
    Applied {
        percent: u8,
        backend: Backend,
    },
    Adjusted {
        requested: u8,
        actual: u8,
        backend: Backend,
    },
    Failed {
        code: String,
        message: String,
    },
}
impl WriteResult {
    fn failed(code: &str, message: &str) -> Self {
        Self::Failed {
            code: code.into(),
            message: message.into(),
        }
    }
}
#[derive(Clone)]
struct Request {
    id: u64,
    generation: u64,
    percent: u8,
    backend: Backend,
}
struct Slot {
    reading: Reading,
    mode: Mode,
    generation: u64,
    latest: u64,
    pending: Option<Request>,
    outcome: Option<(u64, WriteResult)>,
}
struct Device {
    state: Mutex<Slot>,
    wake: Condvar,
    active: AtomicBool,
    refresh: AtomicBool,
}
impl Device {
    fn publish(&self, mut reading: Reading, generation: u64, id: u64, requested: Option<u8>) {
        let mut s = self.state.lock().unwrap();
        if s.generation != generation || s.latest != id || !self.active.load(Ordering::Acquire) {
            return;
        }
        reading.id = id;
        if let Some(wanted) = requested {
            let result = if let Some(e) = &reading.error {
                WriteResult::failed("brightness_write", e)
            } else if wanted == reading.percent {
                WriteResult::Applied {
                    percent: reading.percent,
                    backend: reading.backend,
                }
            } else {
                WriteResult::Adjusted {
                    requested: wanted,
                    actual: reading.percent,
                    backend: reading.backend,
                }
            };
            s.outcome = Some((id, result));
        }
        s.reading = reading;
        self.wake.notify_all();
    }
    fn current(&self, r: &Request) -> bool {
        let s = self.state.lock().unwrap();
        s.generation == r.generation && s.latest == r.id && self.active.load(Ordering::Acquire)
    }
}
pub struct Worker {
    devices: Mutex<HashMap<String, Arc<Device>>>,
    modes: Mutex<HashMap<String, Mode>>,
    selected: Mutex<String>,
    follow_mouse: AtomicBool,
    sequence: AtomicU64,
    stop: AtomicBool,
    dimmer: crate::dimmer::Manager,
}
impl Worker {
    pub fn start(
        key: String,
        follow_mouse: bool,
        modes: HashMap<String, Mode>,
        panel: isize,
    ) -> Arc<Self> {
        let worker = Arc::new(Self {
            devices: Mutex::new(HashMap::new()),
            modes: Mutex::new(modes),
            selected: Mutex::new(key),
            follow_mouse: AtomicBool::new(follow_mouse),
            sequence: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            dimmer: crate::dimmer::Manager::start(panel),
        });
        let this = worker.clone();
        std::thread::Builder::new()
            .name("brightness-discovery".into())
            .spawn(move || this.discover())
            .expect("brightness discovery");
        worker
    }
    fn device(&self, key: &str) -> Option<Arc<Device>> {
        self.devices.lock().unwrap().get(key).cloned()
    }
    pub fn select(&self, key: &str) {
        *self.selected.lock().unwrap() = key.into();
    }
    pub fn follow_mouse(&self, enabled: bool) {
        self.follow_mouse.store(enabled, Ordering::Release);
    }
    pub fn reading(&self) -> Reading {
        self.reading_for(&self.selected.lock().unwrap().clone())
    }
    pub fn reading_for(&self, key: &str) -> Reading {
        self.device(key)
            .map(|d| d.state.lock().unwrap().reading.clone())
            .unwrap_or(Reading {
                key: key.into(),
                ..Default::default()
            })
    }
    pub fn readings(&self) -> Vec<Reading> {
        let mut result: Vec<_> = self
            .devices
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.active.load(Ordering::Acquire))
            .map(|d| d.state.lock().unwrap().reading.clone())
            .collect();
        result.sort_by(|a, b| a.key.cmp(&b.key));
        result
    }
    pub fn set_mode(&self, key: &str, mode: Mode) {
        self.modes.lock().unwrap().insert(key.into(), mode);
        if let Some(d) = self.device(key) {
            let mut s = d.state.lock().unwrap();
            s.mode = mode;
            s.generation += 1;
            s.latest = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
            s.pending = None;
            s.outcome = None;
            s.reading.can_control = false;
            d.wake.notify_all();
        }
    }
    pub fn invalidate(&self) {
        let devices: Vec<_> = self.devices.lock().unwrap().values().cloned().collect();
        for d in devices {
            let mut s = d.state.lock().unwrap();
            s.generation += 1;
            s.pending = None;
            s.outcome = None;
            s.latest = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
            s.reading.can_control = false;
            d.wake.notify_all();
        }
    }
    pub fn refresh(&self) {
        for d in self.devices.lock().unwrap().values() {
            d.refresh.store(true, Ordering::Release);
            d.wake.notify_all();
        }
    }
    pub fn dimmer_snapshot(&self) -> Vec<crate::dimmer::Snapshot> {
        self.dimmer.inspect()
    }
    pub fn restore_software(&self) {
        for r in self.readings() {
            if r.backend == Backend::Software {
                self.submit(r.key, 100, true);
            }
        }
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.dimmer.clear();
        for d in self.devices.lock().unwrap().values() {
            d.wake.notify_all();
        }
    }
    pub fn submit(&self, key: String, percent: u8, _final_value: bool) -> u64 {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(d) = self.device(&key) {
            let mut s = d.state.lock().unwrap();
            s.latest = id;
            s.outcome = None;
            s.pending = Some(Request {
                id,
                generation: s.generation,
                percent: percent.min(100),
                backend: s.reading.backend,
            });
            d.wake.notify_all();
        }
        id
    }
    pub fn submit_confirmed(&self, key: String, percent: u8) -> WriteResult {
        let Some(d) = self.device(&key) else {
            return WriteResult::failed("target_missing", "显示器尚未就绪");
        };
        self.select(&key);
        let id = self.submit(key, percent, true);
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut s = d.state.lock().unwrap();
        loop {
            if s.latest != id {
                return WriteResult::failed("superseded", "已由新的亮度请求或控制方式接管");
            }
            if let Some((done, result)) = &s.outcome {
                if *done == id {
                    return result.clone();
                }
            }
            if Instant::now() >= deadline || self.stop.load(Ordering::Acquire) {
                return WriteResult::failed("confirmation_timeout", "亮度确认超时");
            }
            s = d.wake.wait_timeout(s, Duration::from_millis(25)).unwrap().0;
        }
    }
    fn discover(self: Arc<Self>) {
        let mut due = Instant::now();
        while !self.stop.load(Ordering::Acquire) {
            if Instant::now() >= due {
                due = Instant::now() + Duration::from_secs(2);
                if let Ok(targets) = display::enumerate_targets() {
                    let mut devices = self.devices.lock().unwrap();
                    for (key, d) in devices.iter() {
                        let active = targets.iter().any(|t| &t.key == key);
                        if d.active.swap(active, Ordering::AcqRel) != active {
                            let mut s = d.state.lock().unwrap();
                            s.generation += 1;
                            s.pending = None;
                            s.reading.can_control = false;
                            d.wake.notify_all();
                        }
                    }
                    for target in &targets {
                        if devices.contains_key(&target.key) {
                            continue;
                        }
                        let mode = self
                            .modes
                            .lock()
                            .unwrap()
                            .get(&target.key)
                            .copied()
                            .unwrap_or_default();
                        let d = Arc::new(Device {
                            state: Mutex::new(Slot {
                                reading: Reading {
                                    key: target.key.clone(),
                                    name: target.name.clone(),
                                    mode,
                                    software_transmission: 1.0,
                                    capture_safe: true,
                                    ..Default::default()
                                },
                                mode,
                                generation: 0,
                                latest: 0,
                                pending: None,
                                outcome: None,
                            }),
                            wake: Condvar::new(),
                            active: AtomicBool::new(true),
                            refresh: AtomicBool::new(false),
                        });
                        devices.insert(target.key.clone(), d.clone());
                        let this = self.clone();
                        let key = target.key.clone();
                        std::thread::Builder::new()
                            .name("display-brightness".into())
                            .spawn(move || this.run_device(key, d))
                            .expect("display brightness");
                    }
                    drop(devices);
                    let mut selected = self.selected.lock().unwrap();
                    if !targets.iter().any(|t| t.key == *selected) {
                        *selected = targets
                            .iter()
                            .find(|t| t.is_primary)
                            .or(targets.first())
                            .map(|t| t.key.clone())
                            .unwrap_or_default();
                    }
                }
            }
            if self.follow_mouse.load(Ordering::Acquire) {
                if let Some((x, y)) = hdr_sdr_widget_lib::win32::geometry::cursor_pos() {
                    if let Ok(key) = display::key_at_point(x, y) {
                        self.select(&key);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn run_device(&self, key: String, d: Arc<Device>) {
        let ddc = ddc::Client::default();
        let mut generation = u64::MAX;
        let mut r = d.state.lock().unwrap().reading.clone();
        let mut next_refresh = Instant::now();
        let mut next_probe = Instant::now();
        let mut last_write = Instant::now() - Duration::from_secs(1);
        let mut failures = 0;
        let mut ddc_level: Option<ddc::Level> = None;
        let mut blocked_hdr = false;
        let mut force_probe = true;
        while !self.stop.load(Ordering::Acquire) {
            if !d.active.load(Ordering::Acquire) {
                self.dimmer.remove(&key);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let (mode, gen, id) = {
                let s = d.state.lock().unwrap();
                (s.mode, s.generation, s.latest)
            };
            if generation != gen {
                generation = gen;
                r.mode = mode;
                failures = 0;
                blocked_hdr = false;
                force_probe = true;
                next_probe = Instant::now();
                next_refresh = Instant::now();
            }
            if d.refresh.swap(false, Ordering::AcqRel) {
                next_refresh = Instant::now();
            }
            if Instant::now() >= next_refresh {
                next_refresh = Instant::now() + Duration::from_secs(2);
                let old_hdr = r.hdr;
                // SDR targets can lack advanced-color queries, independently of DDC.
                let state = display::rebind(&key).map(|t| {
                    let hdr = display::read_advanced_color(&t).unwrap_or_default();
                    (t, hdr)
                });
                match state {
                    Ok((target, hdr)) => {
                        r.name = target.name.clone();
                        r.hdr = hdr.is_usable();
                        r.hdr_supported = hdr.supported;
                        if old_hdr != r.hdr {
                            blocked_hdr = false;
                            next_probe = Instant::now();
                        }
                        let white = display::read_sdr_white(&target);
                        if let Ok(raw) = white {
                            r.sdr_white_raw = raw;
                        }
                        if (force_probe
                            || mode == Mode::Ddc
                            || (!r.hdr && mode != Mode::Software)
                            || blocked_hdr)
                            && Instant::now() >= next_probe
                        {
                            force_probe = false;
                            match ddc.call(&key, None) {
                                Ok(level) => {
                                    ddc_level = Some(level);
                                    r.ddc_available = true;
                                    r.ddc_error = None;
                                }
                                Err(e) => {
                                    ddc_level = None;
                                    r.ddc_available = false;
                                    r.ddc_error = Some(e);
                                }
                            }
                            next_probe = Instant::now()
                                + Duration::from_secs(if r.ddc_available { 2 } else { 30 });
                        }
                        let backend = choose(
                            mode,
                            r.hdr && white.is_ok() && !blocked_hdr,
                            r.ddc_available,
                        );
                        if backend != r.backend {
                            self.dimmer.remove(&key);
                            r.software_transmission = 1.0;
                            r.capture_safe = true;
                            r.backend = backend;
                            r.error = None;
                            if backend == Backend::Software {
                                r.percent = 100;
                            }
                        }
                        r.mode = mode;
                        r.can_control = true;
                        r.confirmed = true;
                        r.fallback_reason = match (mode, backend) {
                            (Mode::Software, _)
                            | (Mode::Hdr, Backend::Hdr)
                            | (Mode::Ddc, Backend::Ddc)
                            | (Mode::Auto, Backend::Hdr | Backend::Ddc) => None,
                            _ => Some(if backend == Backend::Software {
                                r.ddc_error
                                    .clone()
                                    .unwrap_or("硬件亮度控制不可用，使用软件压暗".into())
                            } else {
                                format!("指定方式不可用，已使用{}", backend.label())
                            }),
                        };
                        match backend {
                            Backend::Hdr => {
                                r.raw = Some(r.sdr_white_raw);
                                r.percent = raw_to_percent(r.sdr_white_raw).value;
                                r.nits = Some(raw_to_nits(r.sdr_white_raw));
                            }
                            Backend::Ddc => {
                                r.raw = ddc_level.map(|l| l.current);
                                r.percent = ddc_level.map(|l| l.percent()).unwrap_or(r.percent);
                                r.nits = None;
                            }
                            _ => {
                                r.raw = None;
                                r.nits = None;
                                if r.percent < 100 {
                                    if let Ok((_, bounds)) = display::monitor_for_key(&key) {
                                        if let Ok(safe) = self.dimmer.set(&key, bounds, r.percent) {
                                            r.capture_safe = safe;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        r.can_control = false;
                        r.error = Some(e.user_message());
                        self.dimmer.remove(&key);
                    }
                }
                if !self.stop.load(Ordering::Acquire) {
                    d.publish(r.clone(), generation, id, None);
                }
            }
            let request = {
                let mut s = d.state.lock().unwrap();
                let interval = if r.backend == Backend::Ddc { 100 } else { 30 };
                if last_write.elapsed() >= Duration::from_millis(interval) {
                    s.pending.take()
                } else {
                    None
                }
            };
            if let Some(request) = request {
                if !d.current(&request) {
                    continue;
                }
                let started = Instant::now();
                // Backend changes never reinterpret an in-flight percentage.
                let result: Result<(), String> = (|| {
                    if !r.can_control || r.backend != request.backend {
                        return Err("控制方式已变化，请重新调节亮度".into());
                    }
                    match r.backend {
                        Backend::Hdr => {
                            let target = display::rebind(&key).map_err(|e| e.user_message())?;
                            if !display::read_advanced_color(&target)
                                .map_err(|e| e.user_message())?
                                .is_usable()
                            {
                                return Err("HDR 已关闭".into());
                            }
                            let raw = percent_to_raw(Percent::new(request.percent));
                            display::write_sdr_white(&target, raw).map_err(|e| e.user_message())?;
                            let mut actual =
                                display::read_sdr_white(&target).map_err(|e| e.user_message())?;
                            for delay in [50, 100, 200] {
                                if actual == raw || !d.current(&request) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(delay));
                                actual = display::read_sdr_white(&target)
                                    .map_err(|e| e.user_message())?;
                            }
                            r.percent = raw_to_percent(actual).value;
                            r.raw = Some(actual);
                            r.sdr_white_raw = actual;
                            r.nits = Some(raw_to_nits(actual));
                        }
                        Backend::Ddc => {
                            let actual = ddc.call(&key, Some(request.percent))?;
                            ddc_level = Some(actual);
                            r.percent = actual.percent();
                            r.raw = Some(actual.current);
                            next_probe = Instant::now() + Duration::from_secs(2);
                        }
                        Backend::Software => {
                            let (_, bounds) =
                                display::monitor_for_key(&key).map_err(|e| e.user_message())?;
                            r.capture_safe = self.dimmer.set(&key, bounds, request.percent)?;
                            r.percent = request.percent;
                            r.software_transmission = crate::dimmer::transmission(r.percent);
                        }
                        Backend::Unavailable => return Err("亮度控制不可用".into()),
                    }
                    Ok(())
                })();
                last_write = Instant::now();
                r.write_ms = started.elapsed().as_secs_f64() * 1000.0;
                r.confirmed = true;
                match result {
                    Ok(()) => {
                        r.error = None;
                        failures = 0;
                    }
                    Err(e) => {
                        failures += 1;
                        r.error = Some(e.clone());
                        if failures >= 3 || e.contains("超时") || e.contains("HDR 已关闭") {
                            if r.backend == Backend::Ddc {
                                r.ddc_available = false;
                                ddc_level = None;
                                r.ddc_error = Some(e);
                                next_probe = Instant::now() + Duration::from_secs(30);
                            }
                            if r.backend == Backend::Hdr {
                                blocked_hdr = true;
                            }
                            next_refresh = Instant::now();
                            failures = 0;
                        }
                    }
                }
                d.publish(
                    r.clone(),
                    request.generation,
                    request.id,
                    Some(request.percent),
                );
            }
            let s = d.state.lock().unwrap();
            let _ = d.wake.wait_timeout(s, Duration::from_millis(10));
        }
        self.dimmer.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_matrix() {
        assert_eq!(choose(Mode::Auto, true, true), Backend::Hdr);
        assert_eq!(choose(Mode::Auto, false, true), Backend::Ddc);
        assert_eq!(choose(Mode::Auto, false, false), Backend::Software);
        assert_eq!(choose(Mode::Ddc, true, true), Backend::Ddc);
        assert_eq!(choose(Mode::Ddc, true, false), Backend::Hdr);
        assert_eq!(choose(Mode::Hdr, false, true), Backend::Ddc);
        assert_eq!(choose(Mode::Software, true, true), Backend::Software);
    }
    fn device() -> Device {
        Device {
            state: Mutex::new(Slot {
                reading: Reading::default(),
                mode: Mode::Auto,
                generation: 2,
                latest: 10,
                pending: None,
                outcome: None,
            }),
            wake: Condvar::new(),
            active: AtomicBool::new(true),
            refresh: AtomicBool::new(false),
        }
    }
    #[test]
    fn stale_generation_and_id_cannot_overwrite_reading() {
        let d = device();
        for (gen, id) in [(1, 10), (2, 9)] {
            d.publish(
                Reading {
                    percent: 90,
                    ..Default::default()
                },
                gen,
                id,
                Some(90),
            );
        }
        assert_eq!(d.state.lock().unwrap().reading.percent, 0);
        d.publish(
            Reading {
                percent: 72,
                backend: Backend::Ddc,
                ..Default::default()
            },
            2,
            10,
            Some(73),
        );
        assert!(matches!(
            d.state.lock().unwrap().outcome,
            Some((10, WriteResult::Adjusted { actual: 72, .. }))
        ));
    }
    #[test]
    fn idle_read_cannot_replace_completed_result() {
        let d = device();
        d.publish(
            Reading {
                error: Some("write failed".into()),
                ..Default::default()
            },
            2,
            10,
            Some(70),
        );
        d.publish(Reading::default(), 2, 10, None);
        assert!(matches!(
            d.state.lock().unwrap().outcome,
            Some((10, WriteResult::Failed { .. }))
        ));
    }
}
