//! Latest-value mailbox: no UI lock is held during driver calls or readback waits.
use hdr_sdr_widget_lib::{
    core::{
        controller::DisplayApi,
        convert::{percent_to_raw, raw_to_percent},
        model::{DisplayTarget, Percent},
    },
    win32::display::RealApi,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Request {
    pub key: String,
    pub id: u64,
    pub percent: u8,
    pub final_value: bool,
}
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    pub key: String,
    pub id: u64,
    pub percent: u8,
    pub raw: u32,
    pub hdr: bool,
    pub error: Option<String>,
    pub confirmed: bool,
    pub write_ms: f64,
}
#[derive(Default)]
struct Mailbox {
    pending: HashMap<String, Request>,
    latest: HashMap<String, u64>,
    readings: HashMap<String, Reading>,
    outcomes: HashMap<String, Reading>,
}
pub struct Worker {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    stop: AtomicBool,
    sequence: AtomicU64,
    selected: Mutex<String>,
    follow_mouse: bool,
}
struct Check {
    request: Request,
    target: DisplayTarget,
    due: Instant,
    attempt: usize,
    write_ms: f64,
}
impl Worker {
    pub fn start(key: String, follow_mouse: bool) -> Arc<Self> {
        let this = Arc::new(Self {
            mailbox: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            selected: Mutex::new(key),
            follow_mouse,
        });
        let worker = this.clone();
        std::thread::Builder::new()
            .name("brightness-worker".into())
            .spawn(move || worker.run())
            .expect("brightness worker");
        this
    }
    pub fn select(&self, key: &str) {
        *self.selected.lock().unwrap() = key.to_owned();
        self.wake.notify_one();
    }
    pub fn submit(&self, key: String, percent: u8, final_value: bool) -> u64 {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut m = self.mailbox.lock().unwrap();
        m.latest.insert(key.clone(), id);
        m.pending.insert(
            key.clone(),
            Request {
                key,
                id,
                percent: percent.min(100),
                final_value,
            },
        );
        drop(m);
        self.wake.notify_one();
        id
    }
    pub fn reading(&self) -> Reading {
        let key = self.selected.lock().unwrap().clone();
        self.mailbox
            .lock()
            .unwrap()
            .readings
            .get(&key)
            .cloned()
            .unwrap_or_default()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake.notify_all();
    }
    pub fn submit_confirmed(
        &self,
        key: String,
        percent: u8,
    ) -> hdr_sdr_widget_lib::core::model::WriteResult {
        use hdr_sdr_widget_lib::core::model::WriteResult;
        self.select(&key);
        let id = self.submit(key.clone(), percent, true);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut m = self.mailbox.lock().unwrap();
        loop {
            if m.latest.get(&key) != Some(&id) {
                return WriteResult::Failed {
                    code: "superseded".into(),
                    message: "已由更新的亮度请求接管".into(),
                    win32: None,
                };
            }
            if let Some(r) = m.outcomes.get(&key).filter(|r| r.id == id && r.confirmed) {
                return if let Some(error) = &r.error {
                    WriteResult::Failed {
                        code: "native_write".into(),
                        message: error.clone(),
                        win32: None,
                    }
                } else if r.raw == percent_to_raw(Percent::new(percent)) {
                    WriteResult::Applied { raw: r.raw }
                } else {
                    WriteResult::Adjusted {
                        requested: percent_to_raw(Percent::new(percent)),
                        actual: r.raw,
                    }
                };
            }
            if Instant::now() >= deadline || self.stop.load(Ordering::Acquire) {
                return WriteResult::Failed {
                    code: "confirmation_timeout".into(),
                    message: "亮度确认超时".into(),
                    win32: None,
                };
            }
            m = self
                .wake
                .wait_timeout(m, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }
    fn current(&self, r: &Request) -> bool {
        self.mailbox.lock().unwrap().latest.get(&r.key) == Some(&r.id)
    }
    fn publish(&self, mut reading: Reading) {
        let mut m = self.mailbox.lock().unwrap();
        if reading.id == *m.latest.get(&reading.key).unwrap_or(&0) {
            if reading.id > 0
                && reading.confirmed
                && m.outcomes
                    .get(&reading.key)
                    .is_none_or(|r| r.id != reading.id)
            {
                m.outcomes.insert(reading.key.clone(), reading.clone());
            }
            if let Some(outcome) = m.outcomes.get(&reading.key).filter(|r| r.id == reading.id) {
                if outcome.error.is_some() {
                    reading.error = outcome.error.clone();
                }
            }
            m.readings.insert(reading.key.clone(), reading);
        }
        drop(m);
        self.wake.notify_all();
    }
    fn run(&self) {
        let api = RealApi::new();
        self.run_with(&api);
    }
    fn run_with(&self, api: &impl DisplayApi) {
        let mut writes: HashMap<String, Instant> = HashMap::new();
        let mut checks: HashMap<String, Check> = HashMap::new();
        let mut refresh_at = Instant::now();
        while !self.stop.load(Ordering::Acquire) {
            let now = Instant::now();
            let requests = {
                let mut m = self.mailbox.lock().unwrap();
                let keys: Vec<_> = m
                    .pending
                    .keys()
                    .filter(|key| {
                        writes
                            .get(*key)
                            .is_none_or(|t| t.elapsed() >= Duration::from_millis(30))
                    })
                    .cloned()
                    .collect();
                keys.into_iter()
                    .filter_map(|k| m.pending.remove(&k))
                    .collect::<Vec<_>>()
            };
            for r in requests {
                checks.remove(&r.key);
                if !self.current(&r) {
                    continue;
                }
                let started = Instant::now();
                let result = (|| {
                    let target = api.rebind(&r.key)?;
                    if !api.read_hdr(&target)?.is_usable() {
                        return Err(hdr_sdr_widget_lib::error::AppError::HdrDisabled);
                    }
                    api.write_sdr(&target, percent_to_raw(Percent::new(r.percent)))?;
                    Ok(target)
                })();
                writes.insert(r.key.clone(), Instant::now());
                match result {
                    Ok(target) => {
                        checks.insert(
                            r.key.clone(),
                            Check {
                                request: r,
                                target,
                                due: Instant::now(),
                                attempt: 0,
                                write_ms: started.elapsed().as_secs_f64() * 1000.0,
                            },
                        );
                    }
                    Err(e) => {
                        let mut old = self
                            .mailbox
                            .lock()
                            .unwrap()
                            .readings
                            .get(&r.key)
                            .cloned()
                            .unwrap_or_default();
                        old.key = r.key.clone();
                        old.id = r.id;
                        old.error = Some(e.user_message());
                        old.confirmed = true;
                        self.publish(old);
                    }
                }
            }
            let due: Vec<_> = checks
                .iter()
                .filter(|(_, c)| c.due <= now || !self.current(&c.request))
                .map(|(k, _)| k.clone())
                .collect();
            for key in due {
                let mut c = checks.remove(&key).unwrap();
                if !self.current(&c.request) {
                    continue;
                }
                match api.read_sdr(&c.target) {
                    Ok(raw) => {
                        let expected = percent_to_raw(Percent::new(c.request.percent));
                        if raw != expected && c.attempt < 3 {
                            c.due =
                                Instant::now() + Duration::from_millis([50, 100, 200][c.attempt]);
                            c.attempt += 1;
                            checks.insert(key, c);
                        } else {
                            self.publish(Reading {
                                key,
                                id: c.request.id,
                                percent: raw_to_percent(raw).value,
                                raw,
                                hdr: true,
                                error: None,
                                confirmed: c.request.final_value || raw == expected,
                                write_ms: c.write_ms,
                            });
                        }
                    }
                    Err(e) => {
                        let mut old = self
                            .mailbox
                            .lock()
                            .unwrap()
                            .readings
                            .get(&key)
                            .cloned()
                            .unwrap_or_default();
                        old.key = key;
                        old.id = c.request.id;
                        old.error = Some(e.user_message());
                        old.confirmed = true;
                        self.publish(old);
                    }
                }
            }
            if now >= refresh_at {
                refresh_at = now + Duration::from_millis(500);
                let mut key = self.selected.lock().unwrap().clone();
                if self.follow_mouse {
                    if let Some((x, y)) = hdr_sdr_widget_lib::win32::geometry::cursor_pos() {
                        if let Ok(next) = hdr_sdr_widget_lib::win32::display::key_at_point(x, y) {
                            if !next.is_empty() {
                                key = next;
                                *self.selected.lock().unwrap() = key.clone();
                            }
                        }
                    }
                }
                if key.is_empty() {
                    if let Ok(targets) = api.enumerate() {
                        if let Some(t) = targets.iter().find(|t| t.is_primary).or(targets.first()) {
                            key = t.key.clone();
                            *self.selected.lock().unwrap() = key.clone();
                        }
                    }
                }
                let (idle, id) = {
                    let m = self.mailbox.lock().unwrap();
                    (
                        !checks.contains_key(&key) && !m.pending.contains_key(&key),
                        *m.latest.get(&key).unwrap_or(&0),
                    )
                };
                if idle && !key.is_empty() {
                    let refreshed = api.rebind(&key).and_then(|t| {
                        let hdr = api.read_hdr(&t)?.is_usable();
                        let raw = api.read_sdr(&t)?;
                        Ok((hdr, raw))
                    });
                    match refreshed {
                        Ok((hdr, raw)) => self.publish(Reading {
                            key,
                            id,
                            percent: raw_to_percent(raw).value,
                            raw,
                            hdr,
                            confirmed: true,
                            ..Default::default()
                        }),
                        Err(e) => {
                            let mut old = self
                                .mailbox
                                .lock()
                                .unwrap()
                                .readings
                                .get(&key)
                                .cloned()
                                .unwrap_or_default();
                            old.key = key;
                            old.id = id;
                            old.hdr = false;
                            old.error = Some(e.user_message());
                            self.publish(old);
                        }
                    }
                }
            }
            let m = self.mailbox.lock().unwrap();
            let _ = self.wake.wait_timeout(m, Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn worker() -> Worker {
        Worker {
            mailbox: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            selected: Mutex::new("a".into()),
            follow_mouse: false,
        }
    }
    #[test]
    fn burst_is_bounded_and_final_request_survives() {
        let w = worker();
        for n in 0..1000 {
            w.submit("a".into(), (n % 101) as u8, false);
        }
        let id = w.submit("a".into(), 73, true);
        let m = w.mailbox.lock().unwrap();
        assert_eq!(m.pending.len(), 1);
        let r = &m.pending["a"];
        assert_eq!((r.id, r.percent, r.final_value), (id, 73, true));
    }
    #[test]
    fn stale_confirmation_cannot_override_latest() {
        let w = worker();
        let old = w.submit("a".into(), 30, false);
        let new = w.submit("a".into(), 80, true);
        w.publish(Reading {
            key: "a".into(),
            id: new,
            percent: 80,
            ..Default::default()
        });
        w.publish(Reading {
            key: "a".into(),
            id: old,
            percent: 30,
            ..Default::default()
        });
        assert_eq!(w.reading().percent, 80);
    }
    #[test]
    fn idle_refresh_does_not_rewrite_terminal_outcome() {
        let w = worker();
        let id = w.submit("a".into(), 80, true);
        w.publish(Reading {
            key: "a".into(),
            id,
            percent: 30,
            raw: 2500,
            confirmed: true,
            error: Some("driver failure".into()),
            ..Default::default()
        });
        w.publish(Reading {
            key: "a".into(),
            id,
            percent: 30,
            raw: 2500,
            confirmed: true,
            ..Default::default()
        });
        assert_eq!(
            w.mailbox.lock().unwrap().outcomes["a"].error.as_deref(),
            Some("driver failure")
        );
        assert_eq!(w.reading().error.as_deref(), Some("driver failure"));
    }
    struct Fake {
        raw: std::cell::Cell<u32>,
        fail: bool,
        snap: bool,
    }
    impl DisplayApi for Fake {
        fn enumerate(&self) -> Result<Vec<DisplayTarget>, hdr_sdr_widget_lib::error::AppError> {
            Ok(vec![self.rebind("a")?])
        }
        fn rebind(&self, key: &str) -> Result<DisplayTarget, hdr_sdr_widget_lib::error::AppError> {
            Ok(DisplayTarget {
                key: key.into(),
                name: "test".into(),
                index: 0,
                is_primary: true,
                output_tech: 0,
                adapter_luid: 0,
                target_id: 0,
                refresh_hz: 160.0,
            })
        }
        fn read_hdr(
            &self,
            _: &DisplayTarget,
        ) -> Result<hdr_sdr_widget_lib::core::model::HdrState, hdr_sdr_widget_lib::error::AppError>
        {
            Ok(hdr_sdr_widget_lib::core::model::HdrState {
                supported: true,
                enabled: true,
                bits_per_color: 10,
            })
        }
        fn read_sdr(&self, _: &DisplayTarget) -> Result<u32, hdr_sdr_widget_lib::error::AppError> {
            Ok(self.raw.get())
        }
        fn write_sdr(
            &self,
            _: &DisplayTarget,
            raw: u32,
        ) -> Result<(), hdr_sdr_widget_lib::error::AppError> {
            if self.fail {
                return Err(hdr_sdr_widget_lib::error::AppError::ApiFailed(31));
            }
            self.raw.set(if self.snap { raw / 100 * 100 } else { raw });
            Ok(())
        }
    }
    #[test]
    fn worker_confirms_final_value_and_driver_snap() {
        for snap in [false, true] {
            let w = Arc::new(worker());
            let runner = w.clone();
            let thread = std::thread::spawn(move || {
                runner.run_with(&Fake {
                    raw: std::cell::Cell::new(2500),
                    fail: false,
                    snap,
                })
            });
            let result = w.submit_confirmed("a".into(), 73);
            assert_eq!(result.effective_raw(), Some(if snap { 4600 } else { 4650 }));
            w.stop();
            thread.join().unwrap();
        }
    }
    #[test]
    fn driver_failure_keeps_last_confirmed_value() {
        let w = Arc::new(worker());
        w.publish(Reading {
            key: "a".into(),
            percent: 30,
            raw: 2500,
            confirmed: true,
            ..Default::default()
        });
        let runner = w.clone();
        let thread = std::thread::spawn(move || {
            runner.run_with(&Fake {
                raw: std::cell::Cell::new(2500),
                fail: true,
                snap: false,
            })
        });
        let result = w.submit_confirmed("a".into(), 80);
        assert!(matches!(
            result,
            hdr_sdr_widget_lib::core::model::WriteResult::Failed { .. }
        ));
        assert_eq!(w.reading().raw, 2500);
        w.stop();
        thread.join().unwrap();
    }
}
