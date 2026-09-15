//! 验证执行体：只读探针 / 写入回环（V2）/ 步长扫描（V6）。
//!
//! 这是本项目"先验证再动手"纪律的落点。三个子命令：
//!
//! | 命令 | 用途 | 是否改动屏幕 |
//! | --- | --- | --- |
//! | `probe read` | 只读枚举 + 读 SDR/HDR 状态（V1 基线） | 否 |
//! | `probe loop` | 写入回环验证（V2）：读A→写B→回读B'→延时回读B''→还原A→回读A' | 是，约 2–3 秒 |
//! | `probe sweep` | 步长扫描（V6）：实测写入 API 的真实取值粒度 | 是，约 15–30 秒 |
//!
//! # 无论如何都会还原
//!
//! 两个会改屏幕的命令都用 [`RestoreGuard`] 做 RAII 保护：守卫在离开作用域时
//! **无条件**把亮度写回原始值，panic、提前 `return`、甚至 `?` 提前返回都不例外。
//! 此外还注册了控制台 Ctrl+C / Ctrl+Break / 关闭窗口处理器，确保用户中断时
//! 同样触发还原。

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hdr_sdr_widget_lib::core::convert::{percent_to_raw, raw_to_nits, raw_to_percent};
use hdr_sdr_widget_lib::core::model::{DisplayTarget, Percent};
use hdr_sdr_widget_lib::win32::display::{enumerate_targets, read_advanced_color, read_sdr_white,
    write_sdr_white};

/// 用户是否已请求中断（Ctrl+C / 关闭控制台）。
static ABORT: AtomicBool = AtomicBool::new(false);

/// 需要在异常路径上还原的原始值。
static ORIGINAL: OnceLock<Mutex<Option<(String, u32)>>> = OnceLock::new();

/// 写入后等待系统应用的稳定时间（毫秒）。风险 R9：写入后立刻读可能读到旧值。
const SETTLE_MS: u64 = 60;

/// V2 中用于检测"回弹"的延时（毫秒）。
const REBOUND_CHECK_MS: u64 = 1000;

fn original_slot() -> &'static Mutex<Option<(String, u32)>> {
    ORIGINAL.get_or_init(|| Mutex::new(None))
}

/// 记录需要还原的原始值。
fn remember_original(key: &str, raw: u32) {
    if let Ok(mut slot) = original_slot().lock() {
        *slot = Some((key.to_string(), raw));
    }
}

/// 取回已记录的原始值。
fn take_original() -> Option<(String, u32)> {
    original_slot().lock().ok().and_then(|mut slot| slot.take())
}

/// 把亮度还原到记录的原始值。
///
/// 返回是否成功还原。失败时会在 stderr 打印原因，方便人工介入——因为此时
/// 用户的屏幕亮度已经不是他原来的设置了。
fn restore_original() -> bool {
    let Some((key, raw)) = take_original() else {
        return true;
    };

    match enumerate_targets() {
        Ok(targets) => match targets.into_iter().find(|t| t.key == key) {
            Some(target) => match write_sdr_white(&target, raw) {
                Ok(()) => {
                    // 回读确认，还原失败必须让用户知道。
                    std::thread::sleep(Duration::from_millis(SETTLE_MS));
                    match read_sdr_white(&target) {
                        Ok(actual) if actual == raw => true,
                        Ok(actual) => {
                            eprintln!(
                                "\n[警告] 已请求还原到 raw={raw}，但回读为 raw={actual}。\n\
                                 请手动在系统设置中把 SDR 内容亮度调回 {}%。",
                                raw_to_percent(raw).value
                            );
                            false
                        }
                        Err(err) => {
                            eprintln!("\n[警告] 还原后回读失败：{err}");
                            false
                        }
                    }
                }
                Err(err) => {
                    eprintln!(
                        "\n[警告] 还原失败：{err}\n请手动在系统设置中把 SDR 内容亮度调回 {}%。",
                        raw_to_percent(raw).value
                    );
                    false
                }
            },
            None => {
                eprintln!(
                    "\n[警告] 无法找到原始显示器（key={key}），可能已被拔出或重新枚举。\n\
                     请手动检查 SDR 内容亮度设置。"
                );
                false
            }
        },
        Err(err) => {
            eprintln!("\n[警告] 还原时枚举显示器失败：{err}");
            false
        }
    }
}

/// RAII 还原守卫。
///
/// 一旦构造，离开作用域时**必定**尝试把亮度还原到构造时记录的值。
struct RestoreGuard {
    /// 是否仍需要在 Drop 时还原（还原成功后置 false，避免重复写）。
    armed: bool,
}

impl RestoreGuard {
    /// 记录当前值并在作用域结束时保证还原。
    fn capture(target: &DisplayTarget) -> Result<(Self, u32), String> {
        let raw = read_sdr_white(target).map_err(|e| format!("读取当前值失败：{e}"))?;
        remember_original(&target.key, raw);
        Ok((Self { armed: true }, raw))
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            let _ = restore_original();
        }
    }
}

/// 控制台控制事件处理器：Ctrl+C / Ctrl+Break / 关闭窗口 / 注销 / 关机。
///
/// 触发时先置中断标志，再**立刻**执行一次还原，保证用户不会因为中断而
/// 把屏幕亮度留在陌生值上。
unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> i32 {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            ABORT.store(true, Ordering::SeqCst);
            let _ = restore_original();
            1 // 已处理
        }
        _ => 0,
    }
}

#[link(name = "kernel32")]
extern "system" {
    /// 注册/注销控制台控制事件处理器。
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

/// 安装控制台控制处理器。
fn install_ctrl_handler() {
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }
}

/// 用户是否已请求中断。
fn aborted() -> bool {
    ABORT.load(Ordering::SeqCst)
}

/// 可被中断的等待。每隔 10ms 检查一次中断标志。
fn interruptible_sleep(ms: u64) {
    let step = Duration::from_millis(10);
    let mut remaining = Duration::from_millis(ms);
    while !remaining.is_zero() && !aborted() {
        let slice = step.min(remaining);
        std::thread::sleep(slice);
        remaining -= slice;
    }
}

// ---------------------------------------------------------------------------
// 输出辅助
// ---------------------------------------------------------------------------

/// 打印分隔线。
fn rule(title: &str) {
    println!();
    println!("════════ {} ════════", title);
}

/// 刷新标准输出，避免缓冲导致用户看不到实时进度。
fn flush() {
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------------------
// 子命令 1：只读探针（V1 基线）
// ---------------------------------------------------------------------------

/// 只读枚举并输出全部状态。
fn cmd_read() -> i32 {
    rule("V1 只读探针基线");

    let targets = match enumerate_targets() {
        Ok(t) => t,
        Err(err) => {
            eprintln!("枚举显示器失败：{err}");
            return 2;
        }
    };

    println!("活动显示器数量 = {}", targets.len());
    println!();

    let mut all_ok = true;
    for target in &targets {
        println!("── 显示器 #{} ──", target.index);
        println!("  友好名      : {}", target.name);
        println!("  devicePath  : {}", target.key);
        println!("  主显示器    : {}", if target.is_primary { "是" } else { "否" });
        println!("  输出接口    : {} ({})", target.output_tech, tech_name(target.output_tech));
        println!("  刷新率      : {:.3} Hz", target.refresh_hz);

        match read_advanced_color(target) {
            Ok(hdr) => {
                println!(
                    "  HDR         : 支持={} 已开启={} 位深={}bit 可用={}",
                    hdr.supported,
                    hdr.enabled,
                    hdr.bits_per_color,
                    if hdr.is_usable() { "是" } else { "否" }
                );
            }
            Err(err) => {
                println!("  HDR         : 读取失败（{err}）");
                all_ok = false;
            }
        }

        match read_sdr_white(target) {
            Ok(raw) => {
                let percent = raw_to_percent(raw);
                let nits = raw_to_nits(raw);
                println!("  raw         : {raw}");
                println!("  percent     : {}%", percent.value);
                println!("  nits        : {nits:.1}");
                // 交叉校验：两条独立公式必须给出同一个 nits。
                let nits_by_formula = 80.0 + 4.0 * f64::from(percent.value);
                if (nits_by_formula - nits).abs() > 0.01 {
                    println!(
                        "  [不一致] 公式校验失败：raw×0.08={nits:.1} 但 80+4×{}={nits_by_formula:.1}",
                        percent.value
                    );
                    all_ok = false;
                }
                if percent_to_raw(percent) != raw {
                    println!(
                        "  [提示] raw={raw} 不是 50 的倍数，回环值为 {}",
                        percent_to_raw(percent)
                    );
                }
            }
            Err(err) => {
                println!("  raw         : 读取失败（{err}）");
                all_ok = false;
            }
        }
        println!();
    }

    // 结构体尺寸自检（与 Python 探针交叉验证）。
    println!("── 结构体尺寸自检 ──");
    for (name, size) in hdr_sdr_widget_lib::win32::display::struct_size_report() {
        println!("  {name:<20} = {size}");
    }

    // 注册表旁证（只读）。
    rule("V4 注册表旁证（只读）");
    if !hdr_sdr_widget_lib::core::registry::can_read_store() {
        println!("  MonitorDataStore 不可读（无权限），跳过旁证");
    } else {
        let entries = hdr_sdr_widget_lib::core::registry::read_all();
        if entries.is_empty() {
            println!("  未在注册表中找到 SDRWhiteLevel 记录（部分显示器/驱动不写此项，属正常）");
        } else {
            for (key, raw) in &entries {
                println!("  {raw:<8} ← {}", shorten_key(key));
            }
        }
    }

    println!();
    if all_ok {
        println!("判定：PASS（只读探针全部读取成功）");
        0
    } else {
        println!("判定：FAIL（存在读取失败项）");
        1
    }
}

/// 把超长的设备路径裁剪成可读形式，保留头尾。
fn shorten_key(key: &str) -> String {
    if key.chars().count() <= 72 {
        key.to_string()
    } else {
        let chars: Vec<char> = key.chars().collect();
        let head: String = chars[..36].iter().collect();
        let tail: String = chars[chars.len() - 28..].iter().collect();
        format!("{head}…{tail}")
    }
}

/// 输出接口类型码转中文。
fn tech_name(code: u32) -> &'static str {
    match code {
        0 => "HD15 (VGA)",
        1 => "S-Video",
        2 => "复合视频",
        3 => "分量视频",
        4 => "DVI",
        5 => "HDMI",
        6 => "LVDS / 内置面板",
        8 => "D端子",
        9 => "SDI",
        10 => "DisplayPort 外置",
        11 => "DisplayPort 内置",
        12 => "UDI 外置",
        13 => "UDI 内置",
        14 => "SDTV 复合",
        15 => "Miracast",
        16 => "间接有线",
        17 => "间接无线",
        0x8000_0000 => "内部/未初始化",
        _ => "未知",
    }
}

// ---------------------------------------------------------------------------
// 子命令 2：写入回环验证（V2）
// ---------------------------------------------------------------------------

/// V2：读A → 写B → 回读B' → 延时回读B'' → 还原A → 回读A'。
fn cmd_loop(delta_percent: u8) -> i32 {
    rule("V2 写入回环验证");

    let target = match first_target_or_fail() {
        Ok(t) => t,
        Err(code) => return code,
    };

    print_warning();

    // 守卫一旦构造，离开本函数时必定还原原始值。
    let (guard, a) = match RestoreGuard::capture(&target) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };

    let a_percent = raw_to_percent(a);
    println!("[1] 读取当前值 A");
    println!("    A   = raw {a:<6} ({}%, {:.1} nits)", a_percent.value, raw_to_nits(a));

    // 目标值：在 A 基础上加 delta，越界时改减，保证一定与 A 不同。
    let b_percent = {
        let up = a_percent.value.saturating_add(delta_percent);
        if up <= 100 {
            Percent::new(up)
        } else {
            Percent::new(a_percent.value.saturating_sub(delta_percent))
        }
    };
    let b = percent_to_raw(b_percent);
    println!("[2] 计算目标值 B");
    println!("    B   = raw {b:<6} ({}%, {:.1} nits)", b_percent.value, raw_to_nits(b));
    flush();

    println!("[3] 写入 B …");
    flush();
    let write_rc = write_sdr_white(&target, b);
    match &write_rc {
        Ok(()) => println!("    rc = 0（成功）"),
        Err(err) => println!("    rc = 失败：{err}"),
    }
    flush();

    let mut b1 = None;
    let mut b2 = None;
    if write_rc.is_ok() {
        interruptible_sleep(SETTLE_MS);
        println!("[4] 延时 {SETTLE_MS}ms 后回读 B'");
        match read_sdr_white(&target) {
            Ok(v) => {
                println!("    B'  = raw {v:<6} ({}%, {:.1} nits)", raw_to_percent(v).value, raw_to_nits(v));
                b1 = Some(v);
            }
            Err(err) => println!("    回读失败：{err}"),
        }
        flush();

        println!("[5] 再等 {REBOUND_CHECK_MS}ms 回读 B''（检查是否回弹）");
        flush();
        interruptible_sleep(REBOUND_CHECK_MS);
        match read_sdr_white(&target) {
            Ok(v) => {
                println!("    B'' = raw {v:<6} ({}%, {:.1} nits)", raw_to_percent(v).value, raw_to_nits(v));
                b2 = Some(v);
            }
            Err(err) => println!("    回读失败：{err}"),
        }
        flush();
    }

    println!("[6] 还原 A（由 RAII 守卫在作用域结束时执行）");
    flush();

    // 显式 drop 守卫，让还原发生在此处，便于紧接着回读确认。
    drop(guard);

    println!("[7] 回读确认 A'");
    let a1 = read_sdr_white(&target).ok();
    match a1 {
        Some(v) => println!("    A'  = raw {v:<6} ({}%, {:.1} nits)", raw_to_percent(v).value, raw_to_nits(v)),
        None => println!("    回读失败"),
    }

    // ---- 判定 ----
    rule("V2 判定");
    println!("┌──────────┬────────┬─────────┬──────────┐");
    println!("│ 步骤     │ 期望   │ 实测    │ 结果     │");
    println!("├──────────┼────────┼─────────┼──────────┤");

    let write_ok = write_rc.is_ok();
    let b1_ok = b1 == Some(b);
    let b2_ok = b2 == Some(b);
    let a1_ok = a1 == Some(a);

    println!(
        "│ 写入B    │ rc=0   │ {:<7} │ {:<8} │",
        if write_ok { "rc=0" } else { "失败" },
        mark(write_ok)
    );
    println!(
        "│ 回读B'   │ {:<6} │ {:<7} │ {:<8} │",
        b,
        b1.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
        mark(b1_ok)
    );
    println!(
        "│ 回读B''  │ {:<6} │ {:<7} │ {:<8} │",
        b,
        b2.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
        mark(b2_ok)
    );
    println!(
        "│ 还原A'   │ {:<6} │ {:<7} │ {:<8} │",
        a,
        a1.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
        mark(a1_ok)
    );
    println!("└──────────┴────────┴─────────┴──────────┘");

    let pass = write_ok && b1_ok && b2_ok && a1_ok;
    println!();
    if aborted() {
        println!("判定：ABORTED（用户中断，原始值已被还原）");
        130
    } else if pass {
        println!("判定：PASS —— 写入真实生效，1 秒后不回弹，且能正确还原");
        0
    } else {
        println!("判定：FAIL —— 详见上表");
        1
    }
}

/// 打印改动屏幕的事前提示。
fn print_warning() {
    println!("⚠ 本操作会在数秒内改变你的屏幕 SDR 内容亮度，随后自动还原。");
    println!("  Ctrl+C 可随时中断，中断时同样会自动还原。");
    println!();
}

/// 一个判定项的 ✓ / ✗ 标记。
fn mark(ok: bool) -> &'static str {
    if ok {
        "✓ 通过"
    } else {
        "✗ 失败"
    }
}

/// 取第一台显示器，失败则打印原因。
fn first_target_or_fail() -> Result<DisplayTarget, i32> {
    match enumerate_targets() {
        Ok(mut targets) if !targets.is_empty() => Ok(targets.swap_remove(0)),
        Ok(_) => {
            eprintln!("未枚举到任何活动显示器");
            Err(2)
        }
        Err(err) => {
            eprintln!("枚举显示器失败：{err}");
            Err(2)
        }
    }
}

// ---------------------------------------------------------------------------
// 子命令 3：步长扫描（V6）
// ---------------------------------------------------------------------------

/// 一次扫描采样点的结果。
struct Sample {
    requested: u32,
    actual: Option<u32>,
}

/// V6：实测写入 API 的真实取值粒度。
///
/// 分两个阶段：
/// - **阶段 A**：percent 全扫（0–100 共 101 点），确认 percent 级别是否存在吸附。
/// - **阶段 B**：raw 细粒度试探，确认系统是否只接受 50 的倍数。
fn cmd_sweep() -> i32 {
    rule("V6 步长 / 对齐扫描");

    let target = match first_target_or_fail() {
        Ok(t) => t,
        Err(code) => return code,
    };

    print_warning();
    println!("预计耗时 15–30 秒。屏幕亮度会平滑扫过全程，两端不会长时间停留。");
    println!();

    let (guard, a) = match RestoreGuard::capture(&target) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };
    println!("原始值 A = raw {a}（{}%）", raw_to_percent(a).value);
    println!();

    let started = Instant::now();

    // ---------- 阶段 A：percent 全扫 ----------
    println!("── 阶段 A：percent 全扫（0% → 100%，共 101 点）──");
    println!("  目标：确认每个整数百分比写入后，回读值是否等于请求值。");
    flush();

    let mut samples_a: Vec<Sample> = Vec::with_capacity(101);
    let mut mismatches: Vec<(u8, u32, u32)> = Vec::new();

    for percent in 0..=100u8 {
        if aborted() {
            break;
        }
        let requested = percent_to_raw(Percent::new(percent));
        let actual = write_and_read_back(&target, requested);
        if let Some(v) = actual {
            if v != requested {
                mismatches.push((percent, requested, v));
            }
        }
        samples_a.push(Sample { requested, actual });

        // 每 10 个点打一个进度点，避免刷屏。
        if percent % 10 == 0 {
            print!("  {percent:>3}% …");
            flush();
        }
    }
    println!(" 完成");

    let a_ok_count = samples_a.iter().filter(|s| s.actual == Some(s.requested)).count();

    // 先把亮度复位到原始值附近，避免停在 100% 进入阶段 B。
    if !aborted() {
        let _ = write_sdr_white(&target, a);
        interruptible_sleep(SETTLE_MS);
    }

    // ---------- 阶段 B：raw 细粒度试探 ----------
    println!();
    println!("── 阶段 B：raw 细粒度试探 ──");
    println!("  目标：确认系统是否只接受 50 的倍数（即 percent 刻度），");
    println!("        还是接受任意 raw 值。基线 = A = raw {a}。");

    // 以 A 为中心，试探 ±1 / ±5 / ±10 / ±25 / ±49 等偏移。
    let offsets: [i64; 12] = [-49, -25, -10, -5, -1, 1, 5, 10, 25, 49, 50, -50];
    let mut samples_b: Vec<Sample> = Vec::with_capacity(offsets.len());
    let mut accepted_non_grid: Vec<(u32, u32)> = Vec::new();

    for offset in offsets {
        if aborted() {
            break;
        }
        let candidate = (i64::from(a) + offset).clamp(1000, 6000) as u32;
        let actual = write_and_read_back(&target, candidate);
        let on_grid = (candidate - 1000) % 50 == 0;
        if let Some(v) = actual {
            if v == candidate && !on_grid {
                accepted_non_grid.push((candidate, v));
            }
        }
        samples_b.push(Sample { requested: candidate, actual });
    }

    println!("  完成");

    // ---------- 还原 ----------
    drop(guard);
    let restored = read_sdr_white(&target).ok();

    // ---------- 输出 ----------
    rule("V6 扫描结果");

    println!("【阶段 A】percent 全扫：{a_ok_count} / {} 个点回读完全一致",
             samples_a.len());
    if mismatches.is_empty() {
        println!("  ✓ 0%–100% 全部 101 个整数百分比写入后回读值 = 请求值，无吸附、无对齐限制。");
    } else {
        println!("  ✗ 有 {} 个点被吸附：", mismatches.len());
        println!("    ┌──────────┬──────────┬──────────┐");
        println!("    │ percent  │ 请求 raw │ 实际 raw │");
        println!("    ├──────────┼──────────┼──────────┤");
        for (percent, requested, actual) in &mismatches {
            println!("    │ {percent:>8} │ {requested:>8} │ {actual:>8} │");
        }
        println!("    └──────────┴──────────┴──────────┘");
    }

    println!();
    println!("【阶段 B】raw 细粒度试探（基线 raw = {a}）：");
    println!("    ┌──────────┬──────────┬──────────────┬──────────┐");
    println!("    │ 请求 raw │ 实际 raw │ 是否为50倍数 │ 是否被吸附│");
    println!("    ├──────────┼──────────┼──────────────┼──────────┤");
    for s in &samples_b {
        let on_grid = (s.requested - 1000) % 50 == 0;
        let actual_txt = s.actual.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string());
        let snapped = match s.actual {
            Some(v) if v != s.requested => "是",
            Some(_) => "否",
            None => "-",
        };
        println!(
            "    │ {:<8} │ {:<8} │ {:<12} │ {:<8} │",
            s.requested,
            actual_txt,
            if on_grid { "是" } else { "否" },
            snapped
        );
    }
    println!("    └──────────┴──────────┴──────────────┴──────────┘");

    println!();
    println!("【结论】");
    if aborted() {
        println!("  扫描被用户中断，数据不完整，不给出结论。");
    } else {
        // 真实步长判定
        if mismatches.is_empty() {
            println!("  1. percent 级别：真实步长 = 1%（raw 50）。");
            println!("     0–100 全部 101 个档位写入后回读一致，**不存在吸附与对齐限制**。");
            println!("     → UI 拖动可用 1% 步进，不会出现跳动或回弹。");
        } else {
            // 从不一致的点里推断实际粒度
            let deltas: Vec<u32> = mismatches
                .iter()
                .map(|(_, req, act)| req.abs_diff(*act))
                .filter(|d| *d > 0)
                .collect();
            let max_delta = deltas.iter().copied().max().unwrap_or(0);
            let min_delta = deltas.iter().copied().min().unwrap_or(0);
            println!("  1. percent 级别：存在吸附。偏差范围 {min_delta}–{max_delta} raw。");
            println!("     → UI 必须按实测粒度对齐，并在写入后按回读值平滑磁吸。");
        }

        if accepted_non_grid.is_empty() {
            println!("  2. raw 级别：非 50 倍数的 raw 值**全部被吸附回 50 的倍数**。");
            println!("     → 内部真值用整数 percent 是正确的（架构决策 D1 得到实测支持）。");
        } else {
            println!("  2. raw 级别：以下非 50 倍数的 raw 值被系统原样接受：");
            for (req, act) in &accepted_non_grid {
                println!("       raw {req} → {act}");
            }
            println!("     → 系统接受任意 raw 值，UI 可用比 1% 更细的步进。");
        }
    }

    println!();
    match restored {
        Some(v) if v == a => println!("  3. 还原：✓ 原始值 raw {a} 已恢复"),
        Some(v) => println!("  3. 还原：✗ 期望 raw {a}，实际 raw {v}"),
        None => println!("  3. 还原：✗ 回读失败"),
    }
    println!();
    println!("总耗时 {:.1} 秒", started.elapsed().as_secs_f64());

    if aborted() {
        130
    } else if mismatches.is_empty() && restored == Some(a) {
        0
    } else {
        1
    }
}

/// 写入一个 raw 值，等待系统稳定后回读。
///
/// 返回 `None` 表示写入或回读失败。
fn write_and_read_back(target: &DisplayTarget, raw: u32) -> Option<u32> {
    if write_sdr_white(target, raw).is_err() {
        return None;
    }
    interruptible_sleep(SETTLE_MS);
    read_sdr_white(target).ok()
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 打印用法。
fn print_usage() {
    println!("用法：probe <子命令> [选项]");
    println!();
    println!("子命令：");
    println!("  read              只读探针：枚举显示器并读出 SDR/HDR 状态（不改动屏幕）");
    println!("  loop  [--delta N] 写入回环验证 V2（默认 N=10，即当前值 +10%）");
    println!("  sweep             步长/对齐扫描 V6（会改变屏幕亮度 15–30 秒后自动还原）");
    println!();
    println!("示例：");
    println!("  probe read");
    println!("  probe loop --delta 10");
    println!("  probe sweep");
}

fn main() -> std::process::ExitCode {
    install_ctrl_handler();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        args.push("read".to_string());
    }

    let command = args[0].clone();

    // 解析 --delta
    let mut delta: u8 = 10;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--delta" && i + 1 < args.len() {
            delta = args[i + 1].parse::<u8>().unwrap_or(10).clamp(1, 100);
            i += 2;
        } else {
            i += 1;
        }
    }

    let code = match command.as_str() {
        "read" => cmd_read(),
        "loop" => cmd_loop(delta),
        "sweep" => cmd_sweep(),
        "-h" | "--help" | "help" => {
            print_usage();
            0
        }
        other => {
            eprintln!("未知子命令：{other}");
            println!();
            print_usage();
            2
        }
    };

    std::process::ExitCode::from(code as u8)
}
