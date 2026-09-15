//! 单位换算：percent ↔ raw ↔ nits ↔ 倍数。
//!
//! **本模块全部是纯函数，零 IO、零全局状态，可 100% 单测。**
//!
//! 公式（三重验证锁定，见 ARCHITECTURE.md §5.2）：
//! ```text
//! raw     = 1000 + 50 × percent
//! percent = (raw - 1000) / 50
//! nits    = raw × 0.08 = 80 + 4 × percent
//! ```
//!
//! > 侦察表中 30%（raw 2000）与 55%（raw 3000）两行是社区转录笔误，
//! > 正确值应分别为 2500 / 3750。**以公式与本机实测 2850 → 37% 为准。**

use crate::core::model::Percent;

/// percent = 0 时的 raw 值。
pub const RAW_MIN: u32 = 1000;
/// percent = 100 时的 raw 值。
pub const RAW_MAX: u32 = 6000;
/// 每 1% 对应的 raw 增量。
pub const RAW_PER_PERCENT: u32 = 50;
/// percent = 0 时的绝对亮度（cd/m²）。
pub const NITS_MIN: f64 = 80.0;
/// percent = 100 时的绝对亮度（cd/m²）。
pub const NITS_MAX: f64 = 480.0;
/// raw → nits 的系数。
pub const NITS_PER_RAW: f64 = 0.08;
/// 每 1% 对应的 nits 增量。
pub const NITS_PER_PERCENT: f64 = 4.0;

/// 百分比 → raw 值。
#[inline]
#[must_use]
pub fn percent_to_raw(p: Percent) -> u32 {
    RAW_MIN + RAW_PER_PERCENT * u32::from(p.value)
}

/// raw 值 → 百分比（向下取整，超出范围自动钳位）。
///
/// 向下取整而非四舍五入，是为了让"系统吸附后的实际值"能如实反映给用户：
/// 若系统返回 raw=2874（非合法刻度），报 37% 而非 38%，用户看到的是
/// 系统真实落在的档位下界。
#[inline]
#[must_use]
pub fn raw_to_percent(raw: u32) -> Percent {
    Percent::clamp(raw.saturating_sub(RAW_MIN) / RAW_PER_PERCENT)
}

/// raw 值 → 绝对亮度（cd/m²）。
#[inline]
#[must_use]
pub fn raw_to_nits(raw: u32) -> f64 {
    f64::from(raw) * NITS_PER_RAW
}

/// 百分比 → 绝对亮度（cd/m²）。
#[inline]
#[must_use]
pub fn percent_to_nits(p: Percent) -> f64 {
    NITS_MIN + NITS_PER_PERCENT * f64::from(p.value)
}

/// 百分比 → 倍数（1.0 = 80 nits）。
#[inline]
#[must_use]
pub fn percent_to_multiple(p: Percent) -> f64 {
    percent_to_nits(p) / NITS_MIN
}

/// nits → 百分比（用于 UI 的 nits 输入模式，四舍五入到最近档位）。
#[inline]
#[must_use]
pub fn nits_to_percent(nits: f64) -> Percent {
    let raw = (nits / NITS_PER_RAW).round();
    if raw <= f64::from(RAW_MIN) {
        return Percent::new(Percent::MIN);
    }
    if raw >= f64::from(RAW_MAX) {
        return Percent::new(Percent::MAX);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw_u32 = raw as u32;
    // 四舍五入到最近的 50 刻度：+25 后再整除。
    Percent::clamp((raw_u32.saturating_sub(RAW_MIN) + RAW_PER_PERCENT / 2) / RAW_PER_PERCENT)
}

/// 把 raw 值钳位到合法区间 [1000, 6000]。
#[inline]
#[must_use]
pub fn clamp_raw(raw: u32) -> u32 {
    raw.clamp(RAW_MIN, RAW_MAX)
}

/// 该 raw 值是否正好落在合法刻度上（即 1000 + 50k）。
#[inline]
#[must_use]
pub fn is_on_grid(raw: u32) -> bool {
    raw >= RAW_MIN && raw <= RAW_MAX && (raw - RAW_MIN) % RAW_PER_PERCENT == 0
}

/// 把任意 raw 值吸附到最近的合法刻度（含区间钳位）。
#[inline]
#[must_use]
pub fn snap_raw(raw: u32) -> u32 {
    let clamped = clamp_raw(raw);
    let offset = clamped - RAW_MIN;
    let rem = offset % RAW_PER_PERCENT;
    if rem == 0 {
        return clamped;
    }
    let lower = clamped - rem;
    let upper = if lower + RAW_PER_PERCENT > RAW_MAX { RAW_MAX } else { lower + RAW_PER_PERCENT };
    // 距离相等时取上界（与系统设置滑块"四舍五入"行为一致）。
    if rem * 2 >= RAW_PER_PERCENT {
        upper
    } else {
        lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 侦察报告中 8 行对照表的自洽部分（已剔除 30%/55% 两行笔误的 raw 列）。
    const TABLE: [(u32, u8, f64); 8] = [
        (1000, 0, 80.0),
        (1500, 10, 120.0),
        (2000, 20, 160.0),
        (3000, 40, 240.0),
        (3500, 50, 280.0),
        (4000, 60, 320.0),
        (5000, 80, 400.0),
        (6000, 100, 480.0),
    ];

    #[test]
    fn 对照表全部自洽() {
        for (raw, percent, nits) in TABLE {
            let p = Percent::new(percent);
            assert_eq!(percent_to_raw(p), raw, "percent {percent} → raw");
            assert_eq!(raw_to_percent(raw), p, "raw {raw} → percent");
            assert!((raw_to_nits(raw) - nits).abs() < 1e-9, "raw {raw} → nits");
            assert!((percent_to_nits(p) - nits).abs() < 1e-9, "percent {percent} → nits");
        }
    }

    #[test]
    fn 本机实测值_2850() {
        assert_eq!(raw_to_percent(2850), Percent::new(37));
        assert!((raw_to_nits(2850) - 228.0).abs() < 1e-9);
        assert_eq!(percent_to_raw(Percent::new(37)), 2850);
        assert!((percent_to_nits(Percent::new(37)) - 228.0).abs() < 1e-9);
    }

    #[test]
    fn 边界值_0_50_100() {
        assert_eq!(raw_to_percent(1000), Percent::new(0));
        assert_eq!(raw_to_percent(3500), Percent::new(50));
        assert_eq!(raw_to_percent(6000), Percent::new(100));
        assert_eq!(percent_to_raw(Percent::new(0)), 1000);
        assert_eq!(percent_to_raw(Percent::new(50)), 3500);
        assert_eq!(percent_to_raw(Percent::new(100)), 6000);
    }

    #[test]
    fn 全区间往返闭合() {
        for percent in 0..=100u8 {
            let p = Percent::new(percent);
            let raw = percent_to_raw(p);
            assert!(raw >= RAW_MIN && raw <= RAW_MAX);
            assert_eq!(raw_to_percent(raw), p, "percent {percent} 往返失败");
            assert!(is_on_grid(raw));
            assert_eq!(snap_raw(raw), raw);
        }
    }

    #[test]
    fn raw_钳位() {
        assert_eq!(clamp_raw(0), RAW_MIN);
        assert_eq!(clamp_raw(999), RAW_MIN);
        assert_eq!(clamp_raw(6001), RAW_MAX);
        assert_eq!(clamp_raw(u32::MAX), RAW_MAX);
        assert_eq!(clamp_raw(2850), 2850);
    }

    #[test]
    fn 越界_raw_转_percent_钳到边界() {
        assert_eq!(raw_to_percent(0), Percent::new(0));
        assert_eq!(raw_to_percent(500), Percent::new(0));
        assert_eq!(raw_to_percent(10_000), Percent::new(100));
        assert_eq!(raw_to_percent(u32::MAX), Percent::new(100));
    }

    #[test]
    fn 倍数换算() {
        assert!((percent_to_multiple(Percent::new(0)) - 1.0).abs() < 1e-9);
        assert!((percent_to_multiple(Percent::new(50)) - 3.5).abs() < 1e-9);
        assert!((percent_to_multiple(Percent::new(100)) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn nits_输入_四舍五入到最近档位() {
        assert_eq!(nits_to_percent(80.0), Percent::new(0));
        assert_eq!(nits_to_percent(228.0), Percent::new(37));
        assert_eq!(nits_to_percent(231.0), Percent::new(38));
        assert_eq!(nits_to_percent(10.0), Percent::new(0));
        assert_eq!(nits_to_percent(9999.0), Percent::new(100));
        assert_eq!(nits_to_percent(480.0), Percent::new(100));
    }

    #[test]
    fn 刻度判定() {
        assert!(is_on_grid(1000));
        assert!(is_on_grid(2850));
        assert!(is_on_grid(6000));
        assert!(!is_on_grid(2851));
        assert!(!is_on_grid(2874));
        assert!(!is_on_grid(999));
        assert!(!is_on_grid(6001));
    }

    #[test]
    fn 吸附行为() {
        assert_eq!(snap_raw(2851), 2850);
        assert_eq!(snap_raw(2874), 2850);
        assert_eq!(snap_raw(2876), 2900); // 26 > 25，取上界
        assert_eq!(snap_raw(2875), 2900); // 相等取上界
        assert_eq!(snap_raw(999), 1000);
        assert_eq!(snap_raw(9999), 6000);
    }
}
