use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::WindowBackgroundAppearance;
use win_platform::{HotkeySpec, OverlayMonitorPolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnsConfig {
    Auto,
    Fixed(usize),
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub hotkey: String,
    pub columns: ColumnsConfig,
    pub background: WindowBackgroundAppearance,
    pub monitor: OverlayMonitorPolicy,
    pub cross_desktop_nav: bool,
    pub hide_on_blur: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Q".to_string(),
            columns: ColumnsConfig::Auto,
            background: WindowBackgroundAppearance::Blurred,
            monitor: OverlayMonitorPolicy::Cursor,
            cross_desktop_nav: true,
            hide_on_blur: true,
        }
    }
}

const DEFAULT_CONFIG_TOML: &str = r#"# WinDeck 配置文件（保存后在托盘右键点击「重新加载配置」即可生效）

# 全局呼出快捷键，例如: "Alt+Q", "Ctrl+Shift+Space", "Alt+`", "Ctrl+Alt+W"
hotkey = "Alt+Q"

# 每行卡片列数: "auto" (按屏幕宽度自动适配 3~5 列) 或正整数 (如 4)
columns = "auto"

# 背景风格: "blurred" (Win11 毛玻璃) | "transparent" (半透明暗色) | "opaque" (纯暗色)
background = "blurred"

# 多显示器弹出位置: "cursor" (鼠标当前所在屏) | "primary" (主显示器)
monitor = "cursor"

# 方向键移到当前虚拟桌面组末尾时，是否自动衔接下一个虚拟桌面组
cross_desktop_nav = true

# 点击切换器外部（失去焦点）时是否自动隐藏
hide_on_blur = true
"#;

pub fn config_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata).join("WinDeck")
    } else {
        PathBuf::from(".")
    }
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn log_file_path() -> PathBuf {
    config_dir().join("windeck.log")
}

pub fn append_log(message: &str) {
    let dir = config_dir();
    let _ = fs::create_dir_all(&dir);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path())
    {
        let _ = writeln!(file, "[{secs}] {message}");
    }
}

impl AppConfig {
    pub fn load_or_create() -> Self {
        let dir = config_dir();
        let path = config_file_path();
        let _ = fs::create_dir_all(&dir);

        if !path.exists() {
            let _ = fs::write(&path, DEFAULT_CONFIG_TOML);
            return Self::default();
        }

        match fs::read_to_string(&path) {
            Ok(content) => Self::parse(&content),
            Err(_) => Self::default(),
        }
    }

    pub fn parse(content: &str) -> Self {
        let mut cfg = Self::default();

        for raw_line in content.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().trim_matches('"').trim_matches('\'').trim();

            match key.as_str() {
                "hotkey" if !val.is_empty() => cfg.hotkey = val.to_string(),
                "columns" => {
                    if val.eq_ignore_ascii_case("auto") {
                        cfg.columns = ColumnsConfig::Auto;
                    } else if let Ok(n) = val.parse::<usize>() {
                        if (1..=8).contains(&n) {
                            cfg.columns = ColumnsConfig::Fixed(n);
                        }
                    }
                }
                "background" => match val.to_ascii_lowercase().as_str() {
                    "blurred" | "acrylic" => cfg.background = WindowBackgroundAppearance::Blurred,
                    "transparent" => cfg.background = WindowBackgroundAppearance::Transparent,
                    "opaque" => cfg.background = WindowBackgroundAppearance::Opaque,
                    _ => {}
                },
                "monitor" => match val.to_ascii_lowercase().as_str() {
                    "cursor" => cfg.monitor = OverlayMonitorPolicy::Cursor,
                    "primary" => cfg.monitor = OverlayMonitorPolicy::Primary,
                    _ => {}
                },
                "cross_desktop_nav" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.cross_desktop_nav = b;
                    }
                }
                "hide_on_blur" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.hide_on_blur = b;
                    }
                }
                _ => {}
            }
        }

        cfg
    }

    pub fn parsed_hotkey(&self) -> HotkeySpec {
        HotkeySpec::parse(&self.hotkey)
            .unwrap_or_else(|_| HotkeySpec::parse("Alt+Q").expect("default hotkey valid"))
    }

    pub fn resolve_columns(&self, viewport_width_px: f32) -> usize {
        match self.columns {
            ColumnsConfig::Fixed(n) => n.clamp(1, 8),
            ColumnsConfig::Auto => {
                if viewport_width_px >= 1950.0 {
                    5
                } else if viewport_width_px >= 1350.0 {
                    4
                } else if viewport_width_px >= 900.0 {
                    3
                } else {
                    2
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_custom_config() {
        let raw = r#"
            # comment
            hotkey = "Ctrl+Shift+Space"
            columns = 4
            background = "transparent"
            monitor = "primary"
            cross_desktop_nav = false
            hide_on_blur = false
        "#;
        let cfg = AppConfig::parse(raw);
        assert_eq!(cfg.hotkey, "Ctrl+Shift+Space");
        assert_eq!(cfg.columns, ColumnsConfig::Fixed(4));
        assert_eq!(cfg.background, WindowBackgroundAppearance::Transparent);
        assert_eq!(cfg.monitor, OverlayMonitorPolicy::Primary);
        assert!(!cfg.cross_desktop_nav);
        assert!(!cfg.hide_on_blur);
        assert!(HotkeySpec::parse(&cfg.hotkey).is_ok());
    }
}
