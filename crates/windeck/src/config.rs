use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use win_platform::{BackdropStyle, HotkeySpec, OverlayMonitorPolicy, PrimaryModifier};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnsConfig {
    Auto,
    Fixed(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelPosition {
    BottomRight,
    BottomLeft,
    Center,
    Below,
}

impl LabelPosition {
    pub fn parse(val: &str) -> Option<Self> {
        match val.to_ascii_lowercase().replace('-', "_").as_str() {
            "bottom_right" | "br" | "right" => Some(Self::BottomRight),
            "bottom_left" | "bl" | "left" => Some(Self::BottomLeft),
            "center" | "middle" => Some(Self::Center),
            "below" | "bottom" | "outside" => Some(Self::Below),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::BottomRight => Self::BottomLeft,
            Self::BottomLeft => Self::Center,
            Self::Center => Self::Below,
            Self::Below => Self::BottomRight,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::BottomRight => "bottom_right",
            Self::BottomLeft => "bottom_left",
            Self::Center => "center",
            Self::Below => "below",
        }
    }

    pub fn display_label(self) -> &'static str {
        match self {
            Self::BottomRight => "右下角内嵌",
            Self::BottomLeft => "左下角内嵌",
            Self::Center => "居中内嵌",
            Self::Below => "缩略图下方",
        }
    }

    pub fn is_embedded(self) -> bool {
        !matches!(self, Self::Below)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewFit {
    /// Fills 100% of the fixed preview rectangle with zero stretching (top-aligned crop for tall windows).
    Cover,
    /// Shows the entire window uncropped, aspect-fitted inside the fixed preview rectangle.
    Contain,
}

impl PreviewFit {
    pub fn parse(val: &str) -> Option<Self> {
        match val.to_ascii_lowercase().as_str() {
            "cover" | "fill" | "crop" => Some(Self::Cover),
            "contain" | "fit" => Some(Self::Contain),
            _ => None,
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Cover => Self::Contain,
            Self::Contain => Self::Cover,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
        }
    }

    pub fn display_label(self) -> &'static str {
        match self {
            Self::Cover => "铺满裁剪 (cover)",
            Self::Contain => "完整等比 (contain)",
        }
    }
}

/// In-overlay shortcut used for actions like force-quitting the hovered/selected window.
/// Supports single keys (`Delete`, `Backspace`, `X`, `F4`) or modifier combos (`Ctrl+W`, `Shift+Delete`, `Alt+F4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalShortcut {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
}

impl LocalShortcut {
    pub fn normalize_key_name(raw: &str) -> Option<String> {
        let lower = raw.trim().to_ascii_lowercase();
        match lower.as_str() {
            "delete" | "del" => Some("delete".to_string()),
            "backspace" | "back" => Some("backspace".to_string()),
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphabetic() => Some(k.to_string()),
            k if k.starts_with('f') && k.len() <= 3 => {
                let n: u32 = k[1..].parse().ok()?;
                (1..=12).contains(&n).then(|| format!("f{n}"))
            }
            _ => None,
        }
    }

    pub fn parse(spec: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut key: Option<String> = None;

        for part in spec.split('+') {
            let token = part.trim();
            if token.is_empty() {
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                other => {
                    let norm = Self::normalize_key_name(other)?;
                    if key.replace(norm).is_some() {
                        return None;
                    }
                }
            }
        }

        Some(Self {
            ctrl,
            alt,
            shift,
            key: key?,
        })
    }

    pub fn from_keystroke(key: &str, ctrl: bool, alt: bool, shift: bool) -> Option<Self> {
        let norm = Self::normalize_key_name(key)?;
        Some(Self {
            ctrl,
            alt,
            shift,
            key: norm,
        })
    }

    pub fn canonical_label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        let key_disp = match self.key.as_str() {
            "delete" => "Delete".to_string(),
            "backspace" => "Backspace".to_string(),
            other => other.to_ascii_uppercase(),
        };
        parts.push(key_disp);
        parts.join("+")
    }

    pub fn matches(
        &self,
        key: &str,
        ctrl: bool,
        alt: bool,
        shift: bool,
        held_primary: Option<PrimaryModifier>,
    ) -> bool {
        if !self.key.eq_ignore_ascii_case(key) {
            return false;
        }
        if self.ctrl || self.alt || self.shift {
            return ctrl == self.ctrl && alt == self.alt && shift == self.shift;
        }
        // Bare key (e.g. "Delete"): match with no modifiers, or while user is still holding the launch primary modifier
        let allowed_ctrl = matches!(held_primary, Some(PrimaryModifier::Ctrl));
        let allowed_alt = matches!(held_primary, Some(PrimaryModifier::Alt));
        (!ctrl || allowed_ctrl) && (!alt || allowed_alt) && !shift
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub hotkey: String,
    pub force_quit_key: String,
    pub columns: ColumnsConfig,
    pub background: BackdropStyle,
    pub blur_opacity: f32,
    pub monitor: OverlayMonitorPolicy,
    pub immersive: bool,
    pub label_position: LabelPosition,
    pub preview_width: f32,
    pub preview_height: f32,
    pub preview_fit: PreviewFit,
    pub cross_desktop_nav: bool,
    pub hide_on_blur: bool,
    pub auto_update: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Q".to_string(),
            force_quit_key: "Delete".to_string(),
            columns: ColumnsConfig::Auto,
            background: BackdropStyle::Blurred,
            blur_opacity: 0.28,
            monitor: OverlayMonitorPolicy::Cursor,
            immersive: true,
            label_position: LabelPosition::BottomRight,
            preview_width: 380.0,
            preview_height: 226.0,
            preview_fit: PreviewFit::Cover,
            cross_desktop_nav: true,
            hide_on_blur: true,
            auto_update: true,
        }
    }
}

const DEFAULT_CONFIG_TOML: &str = r#"# WinDeck 配置文件（保存后在托盘右键点击「重新加载配置」即可生效）

# 全局呼出快捷键（格式：主键 Ctrl / Alt / Win + 任意字母 A-Z，例如: "Alt+Q", "Ctrl+E", "Win+Q"）
# 呼出后保持主键不松、按方向键选窗，松开主键即可自动跳转到目标窗口；短按松开则停留在界面内自由操作
hotkey = "Alt+Q"

# 强制退出指定窗口快捷键（在切换器内选中或悬停目标窗口后按下，例如: "Delete", "Shift+Delete", "Ctrl+W", "Alt+F4"）
force_quit_key = "Delete"

# 沉浸式无边框全屏（仿 macOS Mission Control 调度中心）: true = 全屏无边框 | false = 居中浮层
immersive = true

# 每个窗口的固定预览尺寸（逻辑像素）
preview_width = 380
preview_height = 226

# 固定尺寸预览适配方式（界面内按 F 键可实时切换）:
#   "cover"   = 等比铺满固定框（无黑边、无拉伸，竖窗保留顶部内容）
#   "contain" = 等比完整显示整窗（不裁剪）
preview_fit = "cover"

# 软件名称与图标位置（界面内按 L 键可实时切换）:
#   "bottom_right" = 嵌入缩略图右下角（默认）
#   "bottom_left"  = 嵌入缩略图左下角
#   "center"       = 嵌入缩略图正中央
#   "below"        = 单独放到缩略图下方
label_position = "bottom_right"

# 背景风格: "blurred" (毛玻璃磨砂) | "transparent" (半透明暗幕) | "opaque" (深邃暗幕)
background = "blurred"

# 毛玻璃背景暗色叠加浓度 (0.05 ~ 0.85，越小高斯模糊越通透；界面内按 - / = 键可实时微调)
blur_opacity = 0.28

# 每行卡片列数: "auto" (按固定预览宽度与屏幕自动排满) 或正整数 (如 4)
columns = "auto"

# 多显示器弹出位置: "cursor" (鼠标当前所在屏) | "primary" (主显示器)
monitor = "cursor"

# 方向键移到当前虚拟桌面组末尾时，是否自动衔接下一个虚拟桌面组
cross_desktop_nav = true

# 点击切换器外部（失去焦点）时是否自动隐藏
hide_on_blur = true

# 自动检查 GitHub Release 新版本更新（每次打开时后台静默检测）
auto_update = true
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
                "force_quit_key" if !val.is_empty() => cfg.force_quit_key = val.to_string(),
                "immersive" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.immersive = b;
                    }
                }
                "preview_width" => {
                    if let Ok(v) = val.parse::<f32>() {
                        cfg.preview_width = v.clamp(160.0, 960.0);
                    }
                }
                "preview_height" => {
                    if let Ok(v) = val.parse::<f32>() {
                        cfg.preview_height = v.clamp(100.0, 640.0);
                    }
                }
                "preview_fit" => {
                    if let Some(fit) = PreviewFit::parse(val) {
                        cfg.preview_fit = fit;
                    }
                }
                "label_position" => {
                    if let Some(pos) = LabelPosition::parse(val) {
                        cfg.label_position = pos;
                    }
                }
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
                    "blurred" | "acrylic" => cfg.background = BackdropStyle::Blurred,
                    "transparent" => cfg.background = BackdropStyle::Transparent,
                    "opaque" => cfg.background = BackdropStyle::Opaque,
                    _ => {}
                },
                "blur_opacity" => {
                    if let Ok(v) = val.parse::<f32>() {
                        cfg.blur_opacity = v.clamp(0.05, 0.90);
                    }
                }
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
                "auto_update" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.auto_update = b;
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

    pub fn parsed_force_quit_key(&self) -> LocalShortcut {
        LocalShortcut::parse(&self.force_quit_key)
            .unwrap_or_else(|| LocalShortcut::parse("Delete").expect("default force_quit_key valid"))
    }

    pub fn to_toml(&self) -> String {
        let background_str = match self.background {
            BackdropStyle::Blurred => "blurred",
            BackdropStyle::Transparent => "transparent",
            BackdropStyle::Opaque => "opaque",
        };
        let columns_str = match self.columns {
            ColumnsConfig::Auto => "\"auto\"".to_string(),
            ColumnsConfig::Fixed(n) => n.to_string(),
        };
        let monitor_str = match self.monitor {
            OverlayMonitorPolicy::Cursor => "cursor",
            OverlayMonitorPolicy::Primary => "primary",
        };
        format!(
            r#"# WinDeck 配置文件（由设置页面自动保存，也可手动编辑后在托盘点「重新加载配置」）

# 全局呼出快捷键（主键 Ctrl / Alt / Win + 任意字母 A-Z，按住主键 + 方向键选窗，松开自动跳转）
hotkey = "{hotkey}"

# 强制退出指定窗口快捷键（选中或悬停目标窗口后按下，如 "Delete", "Shift+Delete", "Ctrl+W", "Alt+F4"）
force_quit_key = "{force_quit_key}"

# 沉浸式无边框全屏（仿 macOS Mission Control 调度中心）: true = 全屏无边框 | false = 居中浮层
immersive = {immersive}

# 每个窗口的固定预览尺寸（逻辑像素）
preview_width = {preview_width}
preview_height = {preview_height}

# 固定尺寸预览适配方式（界面内按 F 键可实时切换）: "cover" | "contain"
preview_fit = "{preview_fit}"

# 软件名称与图标位置（界面内按 L 键可实时切换）: "bottom_right" | "bottom_left" | "center" | "below"
label_position = "{label_position}"

# 背景风格: "blurred" (毛玻璃磨砂) | "transparent" (半透明暗幕) | "opaque" (深邃暗幕)
background = "{background}"

# 毛玻璃背景暗色叠加浓度 (0.05 ~ 0.85，越小高斯模糊越通透；界面内按 - / = 键可实时微调)
blur_opacity = {blur_opacity:.2}

# 每行卡片列数: "auto" 或正整数 (如 4)
columns = {columns}

# 多显示器弹出位置: "cursor" (鼠标当前所在屏) | "primary" (主显示器)
monitor = "{monitor}"

# 方向键移到当前虚拟桌面组末尾时，是否自动衔接下一个虚拟桌面组
cross_desktop_nav = {cross_desktop_nav}

# 点击切换器外部（失去焦点）时是否自动隐藏
hide_on_blur = {hide_on_blur}

# 自动检查 GitHub Release 新版本更新（每次打开时后台静默检测）
auto_update = {auto_update}
"#,
            hotkey = self.parsed_hotkey().canonical_label(),
            force_quit_key = self.parsed_force_quit_key().canonical_label(),
            immersive = self.immersive,
            preview_width = self.preview_width.round() as i32,
            preview_height = self.preview_height.round() as i32,
            preview_fit = self.preview_fit.as_config_str(),
            label_position = self.label_position.as_config_str(),
            background = background_str,
            blur_opacity = self.blur_opacity.clamp(0.05, 0.85),
            columns = columns_str,
            monitor = monitor_str,
            cross_desktop_nav = self.cross_desktop_nav,
            hide_on_blur = self.hide_on_blur,
            auto_update = self.auto_update,
        )
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        fs::write(config_file_path(), self.to_toml())
    }

    /// Computes how many fixed-size tiles fit in a single row cleanly.
    pub fn resolve_columns(&self, content_width_px: f32, tile_outer_w: f32, col_gap: f32) -> usize {
        let max_fit = (((content_width_px + col_gap) / (tile_outer_w + col_gap)).floor() as usize)
            .max(1)
            .min(8);
        match self.columns {
            ColumnsConfig::Fixed(n) => n.clamp(1, max_fit),
            ColumnsConfig::Auto => max_fit,
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
            hotkey = "Ctrl+E"
            force_quit_key = "Shift+Delete"
            immersive = false
            preview_width = 420
            preview_height = 250
            preview_fit = "contain"
            label_position = "center"
            blur_opacity = 0.22
            columns = 4
            background = "transparent"
            monitor = "primary"
            cross_desktop_nav = false
            hide_on_blur = false
            auto_update = false
        "#;
        let cfg = AppConfig::parse(raw);
        assert_eq!(cfg.hotkey, "Ctrl+E");
        assert!(!cfg.immersive);
        assert_eq!(cfg.preview_width, 420.0);
        assert_eq!(cfg.preview_height, 250.0);
        assert_eq!(cfg.preview_fit, PreviewFit::Contain);
        assert_eq!(cfg.label_position, LabelPosition::Center);
        assert!((cfg.blur_opacity - 0.22).abs() < 0.001);
        assert_eq!(cfg.columns, ColumnsConfig::Fixed(4));
        assert_eq!(cfg.background, BackdropStyle::Transparent);
        assert_eq!(cfg.monitor, OverlayMonitorPolicy::Primary);
        assert!(!cfg.cross_desktop_nav);
        assert!(!cfg.hide_on_blur);
        assert!(HotkeySpec::parse(&cfg.hotkey).is_ok());

        // Round-trip serialize -> parse must preserve every setting identically
        let round_tripped = AppConfig::parse(&cfg.to_toml());
        assert_eq!(round_tripped.hotkey, "Ctrl+E");
        assert_eq!(round_tripped.force_quit_key, "Shift+Delete");
        assert!(round_tripped.parsed_force_quit_key().matches("delete", false, false, true, None));
        assert!(!round_tripped.parsed_force_quit_key().matches("delete", false, false, false, None));
        let default_fq = LocalShortcut::parse("Delete").unwrap();
        assert!(default_fq.matches("delete", false, false, false, None));
        assert!(default_fq.matches("delete", false, true, false, Some(PrimaryModifier::Alt)));
        assert_eq!(round_tripped.immersive, cfg.immersive);
        assert_eq!(round_tripped.preview_width, cfg.preview_width);
        assert_eq!(round_tripped.preview_height, cfg.preview_height);
        assert_eq!(round_tripped.preview_fit, cfg.preview_fit);
        assert_eq!(round_tripped.label_position, cfg.label_position);
        assert!((round_tripped.blur_opacity - cfg.blur_opacity).abs() < 0.005);
        assert_eq!(round_tripped.columns, cfg.columns);
        assert_eq!(round_tripped.background, cfg.background);
        assert_eq!(round_tripped.monitor, cfg.monitor);
        assert_eq!(round_tripped.cross_desktop_nav, cfg.cross_desktop_nav);
        assert_eq!(round_tripped.hide_on_blur, cfg.hide_on_blur);
        assert!(!cfg.auto_update);
        assert_eq!(round_tripped.auto_update, cfg.auto_update);
    }
}
