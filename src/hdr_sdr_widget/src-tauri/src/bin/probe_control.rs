//! Read-only DDC probe, or opt-in SDR integration launch with HDR restoration.
use hdr_sdr_widget_lib::win32::{ddc, display};
use std::{path::PathBuf, time::Duration};
use windows::Win32::{Devices::Display::*, Foundation::LUID};

fn set_hdr(key: &str, enabled: bool) -> Result<(), String> {
    let target = display::rebind(key).map_err(|e| e.user_message())?;
    let mut state = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE::default();
    state.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(16),
        size: std::mem::size_of_val(&state) as u32,
        adapterId: LUID {
            LowPart: target.adapter_luid as u32,
            HighPart: (target.adapter_luid >> 32) as i32,
        },
        id: target.target_id,
    };
    state.Anonymous.value = u32::from(enabled);
    let mut rc = unsafe { DisplayConfigSetDeviceInfo(&state.header) };
    if rc == 87 || rc == 50 {
        state.header.r#type = DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE;
        rc = unsafe { DisplayConfigSetDeviceInfo(&state.header) };
    }
    if rc != 0 {
        return Err(format!("HDR toggle failed: {rc}"));
    }
    std::thread::sleep(Duration::from_secs(2));
    let current = display::read_advanced_color(&display::rebind(key).map_err(|e|e.user_message())?).map_err(|e|e.user_message())?;
    if current.enabled != enabled { return Err(format!("HDR state did not change to {enabled}")); }
    Ok(())
}
struct Restore(String, bool);
impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(e) = set_hdr(&self.0, self.1) {
            eprintln!("RESTORE FAILED: {e}");
        }
    }
}
fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args().collect();
    let targets = display::enumerate_targets().map_err(|e| e.user_message())?;
    if args.get(1).map(String::as_str) == Some("read") {
        let records:Vec<_>=targets.iter().map(|t| {
            let read=ddc::Client::default().call(&t.key,None);
            serde_json::json!({"name":t.name,"key":t.key,"ddc":read.as_ref().ok().map(|l|serde_json::json!({"current":l.current,"maximum":l.maximum,"percent":l.percent()})),"error":read.err()})
        }).collect();
        println!("{}", serde_json::to_string_pretty(&records).unwrap());
        return Ok(());
    }
    if args.len() != 4 || args[1] != "sdr-audit" {
        return Err("Usage: probe-control read | sdr-audit <widget.exe> <output-directory>".into());
    }
    let binary = std::fs::canonicalize(&args[2]).map_err(|e| e.to_string())?;
    let output = PathBuf::from(&args[3]);
    std::fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let output = std::fs::canonicalize(output).map_err(|e| e.to_string())?;
    let target = targets.first().ok_or("No monitor")?;
    let original = display::read_advanced_color(target).map_err(|e| e.user_message())?;
    let restore = Restore(target.key.clone(), original.enabled);
    set_hdr(&target.key, false)?;
    let profile = output.join("profile");
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(binary)
        .env("HSDR_CONFIG_DIR", profile)
        .env(
            "HSDR_BRIGHTNESS_AUDIT",
            output.join("brightness-audit.json"),
        )
        .env("HSDR_STATUS_DUMP", output.join("diagnostics.json"))
        .env("HSDR_STATUS_DELAY", "20000")
        .env("HSDR_IGNORE_FULLSCREEN", "1")
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            if !status.success() {
                return Err(format!("Widget exit: {status}"));
            }
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            return Err("Widget timed out".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(restore);
    let current =
        display::read_advanced_color(&display::rebind(&target.key).map_err(|e| e.user_message())?)
            .map_err(|e| e.user_message())?;
    if current.enabled != original.enabled {
        return Err("HDR was not restored".into());
    }
    println!("SDR audit completed; HDR restored to {}", current.enabled);
    Ok(())
}
