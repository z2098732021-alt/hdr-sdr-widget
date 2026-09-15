//! 读写编排：节流、去重、回读校验、吸附纠正、缓存、降级触发。
//!
//! 对应 ARCHITECTURE.md §5.1 的 `BrightnessController`。
//!
//! # 为什么要有 `DisplayApi` / `Clock` 两个 trait
//!
//! 本项目最容易出 bug 的是业务逻辑（节流、去重、吸附），而不是 API 调用本身。
//! 把硬件访问与时间源抽象成 trait 后，这些逻辑可以脱离真机 100% 单测：
//! - [`FakeApi`] 能模拟"系统把值吸附到 100 的倍数""连续返回 ERROR_GEN_FAILURE"等场景；
//! - [`VirtualClock`] 让节流测试瞬间跑完，不必真的 sleep 30ms。
//!
//! # 写入完整流程
//!
//! ```text
//! 解析稳定键 → 校验 HDR → 钳位 → 去重 → 节流 → 写入 → 延迟回读（50/100/200ms）
//!            → 一致? Applied : Adjusted(吸附)
//! ```

use std::collections::HashMap;
use std::time::Instant;

use crate::core::convert::{percent_to_raw, snap_raw};
use crate::core::model::{DisplayState, DisplayTarget, HdrState, Percent, WriteResult};
use crate::error::AppError;

/// 显示器访问抽象。真机实现见 [`crate::win32::display`]，测试实现见 [`FakeApi`]。
pub trait DisplayApi {
    /// 枚举当前全部活动显示器。
    fn enumerate(&self) -> Result<Vec<DisplayTarget>, AppError>;
    /// 按稳定键重新解析出当前的易失句柄。
    fn rebind(&self, key: &str) -> Result<DisplayTarget, AppError>;
    /// 读取 SDR 内容亮度 raw 值。
    fn read_sdr(&self, target: &DisplayTarget) -> Result<u32, AppError>;
    /// 读取 HDR（高级色彩）状态。
    fn read_hdr(&self, target: &DisplayTarget) -> Result<HdrState, AppError>;
    /// 写入 SDR 内容亮度 raw 值。
    fn write_sdr(&self, target: &DisplayTarget, raw: u32) -> Result<(), AppError>;
}

/// 时间源抽象。真机用 [`SystemClock`]，测试用 [`VirtualClock`]。
pub trait Clock {
    /// 单调递增的毫秒计时。
    fn now_ms(&self) -> u64;
    /// 阻塞等待（真机）或推进虚拟时间（测试）。
    fn sleep_ms(&self, ms: u64);
}

/// 真实时间源。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock {
    /// 进程启动时刻，用于把 `Instant` 换算成毫秒。
    epoch: Option<Instant>,
}

impl SystemClock {
    /// 构造一个以当前时刻为原点的时间源。
    #[must_use]
    pub fn new() -> Self {
        Self { epoch: Some(Instant::now()) }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        match self.epoch {
            Some(epoch) => {
                let elapsed = epoch.elapsed();
                elapsed.as_secs() * 1000 + u64::from(elapsed.subsec_millis())
            }
            None => 0,
        }
    }

    fn sleep_ms(&self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

/// 虚拟时间源：不真正 sleep，只推进计数器。仅供测试使用。
#[derive(Debug, Default)]
pub struct VirtualClock {
    /// 当前虚拟毫秒数。
    now: std::cell::Cell<u64>,
}

impl VirtualClock {
    /// 从 0 开始的虚拟时钟。
    #[must_use]
    pub fn new() -> Self {
        Self { now: std::cell::Cell::new(0) }
    }

    /// 当前虚拟时间。
    #[must_use]
    pub fn now_ms_value(&self) -> u64 {
        self.now.get()
    }
}

impl Clock for VirtualClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn sleep_ms(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }
}

/// 当前生效的写入路线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteRoute {
    /// 主路线：私有 API `SET_SDR_WHITE_LEVEL`。
    #[default]
    Primary,
    /// 降级路线：`dwmapi` 序号 171。值不落系统持久层，注销重登会回弹。
    Fallback,
    /// 只读模式：两条路线都不可用，UI 应禁用滑块并给出"前往系统设置"入口。
    ReadOnly,
}

/// 亮度控制器：对外唯一的读写编排入口。
pub struct BrightnessController<A: DisplayApi, C: Clock = SystemClock> {
    api: A,
    clock: C,
    /// 最近一次枚举到的显示器快照。
    targets: Vec<DisplayTarget>,
    /// 每台显示器**上次已知的系统实际值**，是去重的唯一依据。
    known: HashMap<String, u32>,
    /// 每台显示器上次物理写入的虚拟/真实时刻（毫秒）。
    last_write_ms: HashMap<String, u64>,
    min_write_interval_ms: u64,
    /// 写入后回读的重试延迟序列（风险 R9：系统异步应用延迟）。
    read_back_delays_ms: Vec<u64>,
    consecutive_failures: u32,
    failure_threshold: u32,
    route: WriteRoute,
    last_error: Option<AppError>,
}

/// 默认节流间隔（毫秒）。架构决策 D4 / Q13：30ms 最跟手。
pub const DEFAULT_MIN_WRITE_INTERVAL_MS: u64 = 30;
/// 默认回读重试延迟（毫秒）。风险 R9：50 / 100 / 200ms 共三次机会。
pub const DEFAULT_READ_BACK_DELAYS_MS: [u64; 3] = [50, 100, 200];
/// 连续失败多少次后切换到降级路线。
pub const DEFAULT_FAILURE_THRESHOLD: u32 = 3;

impl<A: DisplayApi, C: Clock> BrightnessController<A, C> {
    /// 用默认参数构造控制器。
    pub fn new(api: A, clock: C) -> Self {
        Self {
            api,
            clock,
            targets: Vec::new(),
            known: HashMap::new(),
            last_write_ms: HashMap::new(),
            min_write_interval_ms: DEFAULT_MIN_WRITE_INTERVAL_MS,
            read_back_delays_ms: DEFAULT_READ_BACK_DELAYS_MS.to_vec(),
            consecutive_failures: 0,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            route: WriteRoute::Primary,
            last_error: None,
        }
    }

    /// 覆盖节流间隔（毫秒）。
    pub fn with_min_write_interval_ms(mut self, ms: u64) -> Self {
        self.min_write_interval_ms = ms;
        self
    }

    /// 覆盖回读重试延迟序列。
    pub fn with_read_back_delays_ms(mut self, delays: Vec<u64>) -> Self {
        assert!(!delays.is_empty(), "回读延迟序列不能为空");
        self.read_back_delays_ms = delays;
        self
    }

    /// 覆盖连续失败阈值。
    pub fn with_failure_threshold(mut self, threshold: u32) -> Self {
        self.failure_threshold = threshold;
        self
    }

    /// 当前生效的写入路线。
    #[must_use]
    pub fn route(&self) -> WriteRoute {
        self.route
    }

    /// 最近一次错误（供诊断面板展示）。
    #[must_use]
    pub fn last_error(&self) -> Option<&AppError> {
        self.last_error.as_ref()
    }

    /// 当前缓存的显示器快照。
    #[must_use]
    pub fn targets(&self) -> &[DisplayTarget] {
        &self.targets
    }

    /// 强制重枚举显示器，并全量读取每台的状态。
    ///
    /// 面板打开、显示器切换、收到显示变化消息时都应调用（架构决策 D6）。
    pub fn refresh(&mut self) -> Result<Vec<DisplayState>, AppError> {
        let targets = self.api.enumerate()?;
        self.targets = targets;

        let mut states = Vec::with_capacity(self.targets.len());
        // 先克隆快照，避免对 self.targets 借用与 &mut self 冲突。
        let snapshot = self.targets.clone();
        for target in &snapshot {
            let raw = self.api.read_sdr(target);
            let hdr = self.api.read_hdr(target).unwrap_or_default();

            let (raw, readable) = match raw {
                Ok(v) => {
                    self.known.insert(target.key.clone(), v);
                    (v, true)
                }
                Err(err) => {
                    self.last_error = Some(err);
                    // 读取失败时退回上次已知值，保持 UI 不至于跳变到 0。
                    (self.known.get(&target.key).copied().unwrap_or(0), false)
                }
            };

            let percent = crate::core::convert::raw_to_percent(raw);
            let nits = crate::core::convert::raw_to_nits(raw);

            states.push(DisplayState {
                target: target.clone(),
                hdr,
                raw,
                percent,
                nits,
                readable,
            });
        }

        if states.iter().any(|s| s.readable) {
            self.last_error = self.last_error.take();
        }

        Ok(states)
    }

    /// 按稳定键取一台显示器。找不到就重新枚举一次再找（应对热插拔竞态）。
    pub fn resolve(&mut self, key: &str) -> Result<DisplayTarget, AppError> {
        if let Some(t) = self.targets.iter().find(|t| t.key == key) {
            return Ok(t.clone());
        }
        // 缓存里没有 —— 可能是首次 refresh 前的调用，或显示器刚插上。
        self.targets = self.api.enumerate()?;
        self.targets
            .iter()
            .find(|t| t.key == key)
            .cloned()
            .ok_or(AppError::TargetNotFound)
    }

    /// 按百分比设置亮度。**这是 UI 唯一应该调用的接口。**
    pub fn set_percent(&mut self, key: &str, percent: Percent) -> Result<WriteResult, AppError> {
        self.set_raw(key, percent_to_raw(percent))
    }

    /// 按 raw 值设置亮度。
    ///
    /// 返回 [`WriteResult::Adjusted`] 表示系统把值吸附到了别的刻度，
    /// UI 应当**平滑磁吸**过去而不是回弹（架构决策 D5）。
    pub fn set_raw(&mut self, key: &str, raw: u32) -> Result<WriteResult, AppError> {
        if self.route == WriteRoute::ReadOnly {
            return Err(AppError::NotSupported);
        }

        let target = self.resolve(key)?;

        // HDR 未开启时写入不会有任何可见效果，直接拦下而不是假装成功。
        let hdr = self.api.read_hdr(&target)?;
        if !hdr.is_usable() {
            let err = if hdr.supported {
                AppError::HdrDisabled
            } else {
                AppError::NotSupported
            };
            self.last_error = Some(err.clone());
            return Err(err);
        }

        let requested = crate::core::convert::clamp_raw(raw);

        // 去重：系统当前值已经是目标值，不必重复写入。
        if self.known.get(key) == Some(&requested) {
            return Ok(WriteResult::Applied { raw: requested });
        }

        // 节流：补足与上次写入之间的间隔，保证不丢最后一次请求。
        let now = self.clock.now_ms();
        if let Some(last) = self.last_write_ms.get(key) {
            let elapsed = now.saturating_sub(*last);
            if elapsed < self.min_write_interval_ms {
                self.clock.sleep_ms(self.min_write_interval_ms - elapsed);
            }
        }

        match self.api.write_sdr(&target, requested) {
            Ok(()) => {}
            Err(err) => {
                self.last_error = Some(err.clone());
                self.consecutive_failures += 1;
                if self.consecutive_failures >= self.failure_threshold {
                    self.route = WriteRoute::Fallback;
                }
                return Err(err);
            }
        }

        let write_at = self.clock.now_ms();
        self.last_write_ms.insert(key.to_string(), write_at);

        // 回读校验：系统可能异步应用，按 50/100/200ms 重试（风险 R9）。
        let mut actual = match self.api.read_sdr(&target) {
            Ok(v) => v,
            Err(err) => {
                self.last_error = Some(err.clone());
                return Err(err);
            }
        };
        if actual != requested {
            for delay in self.read_back_delays_ms.clone() {
                self.clock.sleep_ms(delay);
                match self.api.read_sdr(&target) {
                    Ok(v) => {
                        actual = v;
                        if actual == requested {
                            break;
                        }
                    }
                    Err(err) => {
                        self.last_error = Some(err.clone());
                        return Err(err);
                    }
                }
            }
        }

        self.consecutive_failures = 0;
        self.last_error = None;
        self.known.insert(key.to_string(), actual);

        if actual == requested {
            Ok(WriteResult::Applied { raw: actual })
        } else {
            Ok(WriteResult::Adjusted { requested, actual })
        }
    }

    /// 把同一百分比应用到全部显示器。
    pub fn apply_to_all(&mut self, percent: Percent) -> Result<Vec<WriteResult>, AppError> {
        let keys: Vec<String> = self.targets.iter().map(|t| t.key.clone()).collect();
        if keys.is_empty() {
            self.targets = self.api.enumerate()?;
        }
        let keys: Vec<String> = self.targets.iter().map(|t| t.key.clone()).collect();

        let mut results = Vec::with_capacity(keys.len());
        for key in &keys {
            match self.set_raw(key, percent_to_raw(percent)) {
                Ok(result) => results.push(result),
                Err(err) => results.push(WriteResult::failed(&err)),
            }
        }
        Ok(results)
    }

    /// 手动把路线降级到只读（例如用户在设置里关闭了写入）。
    pub fn force_read_only(&mut self) {
        self.route = WriteRoute::ReadOnly;
    }

    /// 回到主路线并清空失败计数。
    pub fn reset_route(&mut self) {
        self.route = WriteRoute::Primary;
        self.consecutive_failures = 0;
        self.last_error = None;
    }

    /// 返回该 raw 值若被系统按 50 步长吸附后的落点。
    ///
    /// 供 UI 在拖动时预判落点，避免"松手后跳一下"的观感。
    #[must_use]
    pub fn predict_snap(&self, raw: u32) -> u32 {
        snap_raw(raw)
    }
}

/// 基于真实 Win32 API 的控制器类型别名。
pub type RealController = BrightnessController<crate::win32::display::RealApi, SystemClock>;

/// 用真实 API 与真实时钟构造控制器。
#[must_use]
pub fn real_controller() -> RealController {
    BrightnessController::new(crate::win32::display::RealApi::new(), SystemClock::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 测试用假 API：可配置吸附粒度、失败序列、HDR 状态。
    struct FakeApi {
        /// 每台显示器的当前值。
        values: RefCell<HashMap<String, u32>>,
        /// 写入失败序列：每次写入弹出一个错误，为空则成功。
        failures: RefCell<Vec<AppError>>,
        /// 吸附粒度：写入值会被吸附到该粒度的倍数（0 表示不吸附）。
        snap_grid: u32,
        /// HDR 状态。
        hdr: HdrState,
        /// 写入调用记录（key, raw）。
        writes: RefCell<Vec<(String, u32)>>,
        /// 读取调用次数，用于验证去重/回读次数。
        reads: RefCell<u32>,
        /// 写入后前 N 次读取返回旧值（模拟系统异步应用延迟）。
        stale_reads: RefCell<u32>,
    }

    impl FakeApi {
        fn new(initial: &[(&str, u32)]) -> Self {
            Self {
                values: RefCell::new(
                    initial.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
                ),
                failures: RefCell::new(Vec::new()),
                snap_grid: 0,
                hdr: HdrState { supported: true, enabled: true, bits_per_color: 12 },
                writes: RefCell::new(Vec::new()),
                reads: RefCell::new(0),
                stale_reads: RefCell::new(0),
            }
        }

        fn with_snap_grid(mut self, grid: u32) -> Self {
            self.snap_grid = grid;
            self
        }

        fn with_failures(mut self, failures: Vec<AppError>) -> Self {
            self.failures = RefCell::new(failures);
            self
        }

        fn with_hdr(mut self, hdr: HdrState) -> Self {
            self.hdr = hdr;
            self
        }

        fn with_stale_reads(mut self, n: u32) -> Self {
            self.stale_reads = RefCell::new(n);
            self
        }

        fn write_count(&self) -> usize {
            self.writes.borrow().len()
        }

        fn read_count(&self) -> u32 {
            *self.reads.borrow()
        }

        fn value_of(&self, key: &str) -> u32 {
            self.values.borrow().get(key).copied().unwrap_or(0)
        }

        fn target(key: &str) -> DisplayTarget {
            DisplayTarget {
                key: key.to_string(),
                name: "Fake".to_string(),
                index: 0,
                is_primary: true,
                output_tech: 5,
                adapter_luid: 1,
                target_id: 1,
                refresh_hz: 60.0,
            }
        }
    }

    impl DisplayApi for FakeApi {
        fn enumerate(&self) -> Result<Vec<DisplayTarget>, AppError> {
            Ok(self.values.borrow().keys().map(|k| Self::target(k)).collect())
        }

        fn rebind(&self, key: &str) -> Result<DisplayTarget, AppError> {
            if self.values.borrow().contains_key(key) {
                Ok(Self::target(key))
            } else {
                Err(AppError::TargetNotFound)
            }
        }

        fn read_sdr(&self, target: &DisplayTarget) -> Result<u32, AppError> {
            *self.reads.borrow_mut() += 1;
            if *self.stale_reads.borrow() > 0 {
                *self.stale_reads.borrow_mut() -= 1;
                // 返回旧值（1000），模拟系统尚未应用。
                return Ok(1000);
            }
            self.values
                .borrow()
                .get(&target.key)
                .copied()
                .ok_or(AppError::TargetNotFound)
        }

        fn read_hdr(&self, _target: &DisplayTarget) -> Result<HdrState, AppError> {
            Ok(self.hdr)
        }

        fn write_sdr(&self, target: &DisplayTarget, raw: u32) -> Result<(), AppError> {
            self.writes.borrow_mut().push((target.key.clone(), raw));
            if !self.failures.borrow().is_empty() {
                return Err(self.failures.borrow_mut().remove(0));
            }
            let applied = if self.snap_grid > 0 {
                let base = crate::core::convert::RAW_MIN;
                let offset = raw.saturating_sub(base);
                let rem = offset % self.snap_grid;
                base + offset - rem
            } else {
                raw
            };
            self.values.borrow_mut().insert(target.key.clone(), applied);
            Ok(())
        }
    }

    const KEY: &str = "fake-key";

    fn ctrl(api: FakeApi) -> BrightnessController<FakeApi, VirtualClock> {
        BrightnessController::new(api, VirtualClock::new())
    }

    #[test]
    fn refresh_读到全部显示器() {
        let mut c = ctrl(FakeApi::new(&[(KEY, 2850)]));
        let states = c.refresh().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].raw, 2850);
        assert_eq!(states[0].percent, Percent::new(37));
        assert!((states[0].nits - 228.0).abs() < 1e-9);
        assert!(states[0].readable);
        assert!(states[0].hdr.is_usable());
    }

    #[test]
    fn 写入成功回读一致返回_applied() {
        let mut c = ctrl(FakeApi::new(&[(KEY, 2850)]));
        c.refresh().unwrap();
        let r = c.set_percent(KEY, Percent::new(62)).unwrap();
        assert_eq!(r, WriteResult::Applied { raw: 4100 });
        assert_eq!(r.effective_raw(), Some(4100));
    }

    #[test]
    fn 去重_同值不重复写入() {
        let api = FakeApi::new(&[(KEY, 2850)]);
        let mut c = ctrl(api);
        c.refresh().unwrap();
        let baseline = c.api.read_count();

        c.set_percent(KEY, Percent::new(37)).unwrap();
        assert_eq!(c.api.write_count(), 0, "系统值已等于目标值，不应产生写入");
        // 去重依据的是控制器持有的已知值，不该为了"比一下"就去读系统 ——
        // `set_percent` 是最高频的调用（拖动时 30ms 一次），每次回读会成为
        // 最热的 IO 路径，而它的结果对去重判定毫无增量。
        assert_eq!(c.api.read_count(), baseline, "去重不应触发额外的系统读取");

        c.set_percent(KEY, Percent::new(50)).unwrap();
        assert_eq!(c.api.write_count(), 1);
        let after_write = c.api.read_count();

        c.set_percent(KEY, Percent::new(50)).unwrap();
        assert_eq!(c.api.write_count(), 1, "重复请求同一值应被去重");
        assert_eq!(c.api.read_count(), after_write, "重复请求不应触发回读");
    }

    #[test]
    fn 节流_补足间隔才写() {
        let clock = VirtualClock::new();
        let api = FakeApi::new(&[(KEY, 2850)]);
        let mut c = BrightnessController::new(api, clock);
        c.refresh().unwrap();

        c.set_percent(KEY, Percent::new(40)).unwrap();
        assert_eq!(c.clock.now_ms_value(), 0, "首次写入无需等待");

        c.set_percent(KEY, Percent::new(41)).unwrap();
        assert_eq!(c.clock.now_ms_value(), 30, "第二次写入应补足 30ms 间隔");
    }

    #[test]
    fn 吸附_返回_adjusted_并记录实际值() {
        // 系统把值吸附到 100 的倍数：4100 → 4100；4120 → 4100。
        let api = FakeApi::new(&[(KEY, 2850)]).with_snap_grid(100);
        let mut c = ctrl(api);
        c.refresh().unwrap();

        let r = c.set_raw(KEY, 4120).unwrap();
        assert_eq!(r, WriteResult::Adjusted { requested: 4120, actual: 4100 });
        // 回读重试会把 known 更新为实际值，后续去重据此工作。
        assert_eq!(c.api.value_of(KEY), 4100);
    }

    #[test]
    fn 回读重试_系统延迟应用也能识别成功() {
        // 前 1 次读取返回旧值，第 2 次才返回新值。
        let api = FakeApi::new(&[(KEY, 2850)]).with_stale_reads(1);
        let mut c = ctrl(api);
        c.refresh().unwrap();
        let r = c.set_percent(KEY, Percent::new(62)).unwrap();
        assert_eq!(r, WriteResult::Applied { raw: 4100 });
    }

    #[test]
    fn hdr_未开启时拒绝写入并分类错误() {
        let api = FakeApi::new(&[(KEY, 2850)])
            .with_hdr(HdrState { supported: true, enabled: false, bits_per_color: 8 });
        let mut c = ctrl(api);
        c.refresh().unwrap();
        let err = c.set_percent(KEY, Percent::new(62)).unwrap_err();
        assert_eq!(err, AppError::HdrDisabled);
        assert_eq!(c.api.write_count(), 0);
    }

    #[test]
    fn 不支持_hdr_时归类为_not_supported() {
        let api = FakeApi::new(&[(KEY, 2850)])
            .with_hdr(HdrState { supported: false, enabled: false, bits_per_color: 8 });
        let mut c = ctrl(api);
        c.refresh().unwrap();
        let err = c.set_percent(KEY, Percent::new(62)).unwrap_err();
        assert_eq!(err, AppError::NotSupported);
    }

    #[test]
    fn 连续失败达阈值切换降级路线() {
        let api = FakeApi::new(&[(KEY, 2850)]).with_failures(vec![
            AppError::ApiFailed(31),
            AppError::ApiFailed(31),
            AppError::ApiFailed(31),
        ]);
        let mut c = ctrl(api);
        c.refresh().unwrap();

        assert_eq!(c.route(), WriteRoute::Primary);
        assert!(c.set_percent(KEY, Percent::new(40)).is_err());
        assert_eq!(c.route(), WriteRoute::Primary);
        assert!(c.set_percent(KEY, Percent::new(41)).is_err());
        assert_eq!(c.route(), WriteRoute::Primary);
        assert!(c.set_percent(KEY, Percent::new(42)).is_err());
        assert_eq!(c.route(), WriteRoute::Fallback, "连续 3 次失败应切降级");
    }

    #[test]
    fn 成功一次即清零失败计数() {
        let api = FakeApi::new(&[(KEY, 2850)])
            .with_failures(vec![AppError::ApiFailed(31), AppError::ApiFailed(31)]);
        let mut c = ctrl(api);
        c.refresh().unwrap();
        assert!(c.set_percent(KEY, Percent::new(40)).is_err());
        assert!(c.set_percent(KEY, Percent::new(41)).is_err());
        assert!(c.set_percent(KEY, Percent::new(42)).is_ok());
        // 再失败两次也不应立刻降级，因为计数已被清零。
        assert!(c.set_percent(KEY, Percent::new(43)).is_ok());
        assert_eq!(c.route(), WriteRoute::Primary);
    }

    #[test]
    fn 只读模式下任何写入都被拒绝() {
        let mut c = ctrl(FakeApi::new(&[(KEY, 2850)]));
        c.refresh().unwrap();
        c.force_read_only();
        assert_eq!(c.set_percent(KEY, Percent::new(50)).unwrap_err(), AppError::NotSupported);
        c.reset_route();
        assert!(c.set_percent(KEY, Percent::new(50)).is_ok());
    }

    #[test]
    fn 目标不存在返回_target_not_found() {
        let mut c = ctrl(FakeApi::new(&[(KEY, 2850)]));
        c.refresh().unwrap();
        assert_eq!(
            c.set_percent("不存在的键", Percent::new(50)).unwrap_err(),
            AppError::TargetNotFound
        );
    }

    #[test]
    fn 百分比越界先钳位再写入() {
        let mut c = ctrl(FakeApi::new(&[(KEY, 2850)]));
        c.refresh().unwrap();
        // raw 由 percent 算出，天然在界内；直接喂越界 raw 验证钳位。
        assert_eq!(c.set_raw(KEY, 99999).unwrap(), WriteResult::Applied { raw: 6000 });
        assert_eq!(c.api.value_of(KEY), 6000);
    }

    #[test]
    fn 应用到全部显示器() {
        let api = FakeApi::new(&[("a", 2850), ("b", 1000), ("c", 6000)]);
        let mut c = ctrl(api);
        c.refresh().unwrap();
        let results = c.apply_to_all(Percent::new(45)).unwrap();
        assert_eq!(results.len(), 3);
        for r in &results {
            assert_eq!(*r, WriteResult::Applied { raw: 3250 });
        }
        assert_eq!(c.api.value_of("a"), 3250);
        assert_eq!(c.api.value_of("b"), 3250);
        assert_eq!(c.api.value_of("c"), 3250);
    }

    #[test]
    fn 吸附落点可预判() {
        let c = ctrl(FakeApi::new(&[(KEY, 2850)]));
        assert_eq!(c.predict_snap(2851), 2850);
        assert_eq!(c.predict_snap(2876), 2900);
        assert_eq!(c.predict_snap(7000), 6000);
    }

    #[test]
    fn 找不到的键会触发重枚举() {
        let mut c = ctrl(FakeApi::new(&[(KEY, 2850)]));
        // 不调用 refresh，直接 resolve：应触发内部 enumerate。
        let t = c.resolve(KEY).unwrap();
        assert_eq!(t.key, KEY);
        assert_eq!(c.targets().len(), 1);
    }

    #[test]
    fn 虚拟时钟_节流不真睡眠() {
        let clock = VirtualClock::new();
        let api = FakeApi::new(&[(KEY, 2850)]);
        let mut c = BrightnessController::new(api, clock).with_min_write_interval_ms(1000);
        c.refresh().unwrap();
        let start = std::time::Instant::now();
        c.set_percent(KEY, Percent::new(40)).unwrap();
        c.set_percent(KEY, Percent::new(41)).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
        assert_eq!(c.clock.now_ms_value(), 1000);
    }
}

/// 供 `win32::display` 使用的真实 API 适配器在 `display.rs` 中定义；
/// 这里再导出一次，方便调用方只 import 本模块。
pub use crate::win32::display::RealApi;
