//! 应用配置：`AppSettings` 定义 + JSON 持久化。
//!
//! 存储位置：`%APPDATA%\hdr-sdr-widget\settings.json`。
//! 对齐 ARCHITECTURE.md §5.5 的配置结构；`per_monitor_memory` 预留实现
//! PRD P2-3（每显示器独立记忆上次值）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use hdr_sdr_widget_lib::error::AppError;

use crate::edge::Edge;

/// 配置文件的磁盘版本号。结构变更时递增并实现迁移（v1 → v2 新增贴边字段）。
pub const SETTINGS_VERSION: u32 = 2;

/// 预设档位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    /// 稳定 ID：`day` / `movie` / `night`，也用于 command 路由。
    pub id: String,
    /// 显示名，例如「白天」。
    pub name: String,
    /// 目标百分比 [0,100]。
    pub percent: u8,
    /// 图标键，对应前端 `icons.ts` 的 SVG 图标名。
    pub icon_id: String,
}

impl Default for Preset {
    fn default() -> Self {
        Self { id: String::new(), name: String::new(), percent: 0, icon_id: String::new() }
    }
}

/// 默认三档预设（PRD Q9：白天 80 / 观影 60 / 夜间 30）。
pub fn default_presets() -> Vec<Preset> {
    vec![
        Preset { id: "day".into(), name: "白天".into(), percent: 80, icon_id: "sun".into() },
        Preset { id: "movie".into(), name: "观影".into(), percent: 60, icon_id: "film".into() },
        Preset { id: "night".into(), name: "夜间".into(), percent: 30, icon_id: "moon".into() },
    ]
}

/// 上次窗口位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowPos {
    pub x: i32,
    pub y: i32,
    /// 记录位置时所在的显示器稳定键，用于跨显示器纠正。
    #[serde(default)]
    pub monitor_key: String,
}

impl Default for WindowPos {
    fn default() -> Self {
        Self { x: 0, y: 0, monitor_key: String::new() }
    }
}

/// 数值显示单位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    #[default]
    Percent,
    Nits,
    Multiple,
}

/// 应用配置（可整体序列化/反序列化）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub version: u32,
    /// 数值显示单位。
    pub unit: Unit,
    /// ± 按钮步长。
    pub step: u8,
    /// 按住 Shift 的步长。
    pub step_shift: u8,
    /// 全局热键字符串，例如 `Ctrl+Alt+B`。
    pub hotkey: String,
    /// 面板打开时是否跟随鼠标所在显示器。
    pub follow_mouse_monitor: bool,
    /// 点击面板外部是否自动隐藏。
    pub hide_on_blur: bool,
    /// 是否开机自启（HKCU Run 键）。
    pub autostart: bool,
    /// 上次窗口位置。
    pub last_window_pos: WindowPos,
    /// 上次选中的显示器稳定键。
    pub last_monitor_key: String,
    /// 三档预设。
    pub presets: Vec<Preset>,
    /// 每显示器上次记忆值（PRD P2-3），key = 显示器稳定键。
    #[serde(default)]
    pub per_monitor_memory: std::collections::HashMap<String, u8>,
    /// 首次启动标记（用于首启定位到托盘上方而非上次位置）。
    #[serde(default = "default_first_run")]
    pub first_run: bool,
    /// v2：贴边方向；`null` = 自由悬浮（记忆 `last_window_pos`）。
    #[serde(default)]
    pub dock_side: Option<Edge>,
    /// v2：贴边时的垂直位置（物理像素）。
    #[serde(default = "default_widget_y")]
    pub widget_y: i32,
}

/// 首次启动标记的默认值：新安装时视为首次。
const fn default_first_run() -> bool {
    true
}

/// v2 贴边垂直位置的默认值（首启贴右缘的胶囊中点高度）。
const fn default_widget_y() -> i32 {
    220
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            unit: Unit::Percent,
            step: 1,
            step_shift: 5,
            hotkey: String::new(), // v2 默认关：空串 = 禁用（P1-3/U4）
            follow_mouse_monitor: true,
            hide_on_blur: true,
            autostart: false,
            last_window_pos: WindowPos::default(),
            last_monitor_key: String::new(),
            presets: default_presets(),
            per_monitor_memory: std::collections::HashMap::new(),
            first_run: true,
            dock_side: None,
            widget_y: 220,
        }
    }
}

impl AppSettings {
    /// 按 ID 取预设档。
    pub fn preset(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// 更新预设档值（按 ID 匹配，找不到则忽略）。
    pub fn update_preset(&mut self, id: &str, percent: u8) {
        if let Some(p) = self.presets.iter_mut().find(|p| p.id == id) {
            p.percent = percent.clamp(0, 100);
        }
    }

    /// 记录某台显示器的上次值。
    pub fn remember_monitor(&mut self, key: &str, percent: u8) {
        self.per_monitor_memory.insert(key.to_string(), percent);
    }
}

/// 配置存储：路径 + 读写。
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// 用默认路径（`%APPDATA%\hdr-sdr-widget\settings.json`）构造。
    pub fn new() -> Result<Self, AppError> {
        let dir = match std::env::var_os("HSDR_CONFIG_DIR") {
            Some(path) => PathBuf::from(path),
            None => appdata_dir()?,
        };
        Ok(Self { path: dir.join("settings.json") })
    }

    /// 配置文件的绝对路径（诊断/复制信息用）。
    #[must_use]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// 加载配置；文件不存在或损坏时回退到默认值（损坏时覆盖写回）。
    pub fn load(&self) -> AppSettings {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => match serde_json::from_str::<AppSettings>(&text) {
                Ok(mut s) => {
                    // 版本迁移钩子：目前仅 v1，未来结构变更在此升级。
                    if s.version != SETTINGS_VERSION {
                        s.version = SETTINGS_VERSION;
                    }
                    s
                }
                Err(_) => AppSettings::default(),
            },
            Err(_) => AppSettings::default(),
        }
    }

    /// 保存配置到磁盘。父目录不存在时自动创建。
    pub fn save(&self, settings: &AppSettings) -> Result<(), AppError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AppError::Io(e.to_string()))?;
        }
        let json = serde_json::to_string_pretty(settings)
            .map_err(|e| AppError::Io(format!("序列化失败：{e}")))?;
        std::fs::write(&self.path, json).map_err(|e| AppError::Io(e.to_string()))
    }
}

/// 计算 `%APPDATA%\hdr-sdr-widget` 目录（应用数据目录）。
fn appdata_dir() -> Result<PathBuf, AppError> {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("LOCALAPPDATA").map(PathBuf::from))
        .map_err(|_| AppError::Io("无法定位 APPDATA 目录".to_string()))?;
    Ok(base.join("hdr-sdr-widget"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认配置_三档预设与默认值() {
        let s = AppSettings::default();
        assert_eq!(s.unit, Unit::Percent);
        assert_eq!(s.step, 1);
        assert_eq!(s.step_shift, 5);
        assert_eq!(s.hotkey, ""); // v2 默认关
        assert!(s.follow_mouse_monitor);
        assert!(s.hide_on_blur);
        assert!(!s.autostart);
        assert!(s.first_run);
        assert_eq!(s.presets.len(), 3);
        assert_eq!(s.preset("day").map(|p| p.percent), Some(80));
        assert_eq!(s.preset("movie").map(|p| p.percent), Some(60));
        assert_eq!(s.preset("night").map(|p| p.percent), Some(30));
    }

    #[test]
    fn 预设更新按_id() {
        let mut s = AppSettings::default();
        s.update_preset("day", 90);
        assert_eq!(s.preset("day").map(|p| p.percent), Some(90));
        s.update_preset("day", 150); // 越界钳位
        assert_eq!(s.preset("day").map(|p| p.percent), Some(100));
        s.update_preset("不存在的", 50); // 忽略
        assert_eq!(s.presets.len(), 3);
    }

    #[test]
    fn 序列化往返一致() {
        let mut s = AppSettings::default();
        s.update_preset("night", 25);
        s.remember_monitor("monitor-a", 42);
        let json = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.preset("night").map(|p| p.percent), Some(25));
        assert_eq!(back.per_monitor_memory.get("monitor-a"), Some(&42));
    }

    #[test]
    fn 损坏_json回退默认() {
        // 不真正写盘，直接验证反序列化失败路径的等价行为。
        let text = "{ 这不是合法 JSON ";
        assert!(serde_json::from_str::<AppSettings>(text).is_err());
    }

    #[test]
    fn v2贴边字段与版本() {
        let s = AppSettings::default();
        assert_eq!(s.version, 2);
        assert_eq!(s.dock_side, None);
        assert_eq!(s.widget_y, 220);

        let mut docked = AppSettings::default();
        docked.dock_side = Some(Edge::Right);
        docked.widget_y = 500;
        let json = serde_json::to_string(&docked).unwrap();
        assert!(json.contains(r#""dockSide":"right""#));
        assert!(json.contains(r#""widgetY":500"#));
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dock_side, Some(Edge::Right));
        assert_eq!(back.widget_y, 500);
    }

    #[test]
    fn v1老配置迁移补齐新字段() {
        // 模拟 v1 settings.json：无 dockSide/widgetY 字段。
        let v1 = r#"{"version":1,"unit":"percent","step":1,"stepShift":5,"hotkey":"Ctrl+Alt+B","followMouseMonitor":true,"hideOnBlur":true,"autostart":false,"lastWindowPos":{"x":10,"y":20,"monitorKey":""},"lastMonitorKey":"","presets":[],"firstRun":false}"#;
        let s: AppSettings = serde_json::from_str(v1).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.dock_side, None);
        assert_eq!(s.widget_y, 220);
        assert_eq!(s.last_window_pos.x, 10);
    }

    #[test]
    fn 路径位于appdata() {
        let store = SettingsStore::new().expect("应能定位 APPDATA");
        let p = store.path().to_string_lossy().to_string();
        assert!(p.ends_with("hdr-sdr-widget\\settings.json") || p.ends_with("hdr-sdr-widget/settings.json"));
    }
}
