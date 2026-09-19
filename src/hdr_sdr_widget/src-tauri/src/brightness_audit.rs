//! Opt-in integration audit; isolated profile required by the launch script.
use crate::brightness::{Backend, Mode, Worker, WriteResult};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub fn start(worker: Arc<Worker>, output: PathBuf) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let original = loop {
            let r = worker.reading();
            if r.can_control {
                break r;
            }
            if Instant::now() >= deadline {
                let _ = std::fs::write(&output, "{\"error\":\"display discovery timeout\"}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let key = original.key.clone();
        let mut cases = Vec::new();
        worker.set_mode(&key, Mode::Software);
        let ready = wait_backend(&worker, &key, Backend::Software);
        cases.push(serde_json::json!({"case":"enter_software_without_write","passed":ready && worker.reading_for(&key).percent==100,"reading":worker.reading_for(&key)}));
        for percent in [50, 0, 100] {
            let result = worker.submit_confirmed(key.clone(), percent);
            let reading = worker.reading_for(&key);
            let windows = worker.dimmer_snapshot();
            let window_ok = if percent == 100 {
                windows.is_empty()
            } else {
                windows.iter().any(|w| {
                    w.key == key
                        && w.alpha == crate::dimmer::alpha(percent)
                        && w.excluded
                        && w.click_through
                        && w.no_activate
                        && w.visible
                })
            };
            let passed = matches!(result,WriteResult::Applied { percent: p, backend: Backend::Software } if p == percent)
                && (reading.software_transmission - crate::dimmer::transmission(percent)).abs()
                    < 0.001
                && window_ok;
            cases.push(serde_json::json!({"case":format!("software_{percent}"),"passed":passed,"result":result,"reading":reading,"windows":windows}));
        }
        for p in 0..1000 {
            worker.submit(key.clone(), (p % 101) as u8, false);
        }
        let result = worker.submit_confirmed(key.clone(), 73);
        cases.push(serde_json::json!({"case":"burst_latest_value","passed":matches!(result,WriteResult::Applied{percent:73,..}),"result":result}));
        worker.restore_software();
        let deadline = Instant::now() + Duration::from_secs(3);
        while worker.reading_for(&key).percent != 100 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        cases.push(serde_json::json!({"case":"restore_all_software","passed":worker.reading_for(&key).percent==100}));
        worker.set_mode(&key, Mode::Auto);
        let ready = wait_backend(&worker, &key, original.backend);
        cases.push(serde_json::json!({"case":"return_to_auto","passed":ready,"reading":worker.reading_for(&key)}));
        let report = serde_json::json!({"original":original,"cases":cases,"hardwareWrites":false});
        let _ = std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap());
    });
}
fn wait_backend(worker: &Worker, key: &str, backend: Backend) -> bool {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let r = worker.reading_for(key);
        if r.can_control && r.backend == backend {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
