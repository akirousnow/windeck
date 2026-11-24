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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionEffect {
    Off,
    Spring,
}

impl TransitionEffect {
    pub fn parse(val: &str) -> Option<Self> {
        match val.to_ascii_lowercase().replace('-', "_").as_str() {
            "off" | "none" | "false" | "disabled" => Some(Self::Off),
            "spring" | "dynamic" | "bouncy" | "on" | "true" | "enabled" | "smooth" | "subtle" => {
                Some(Self::Spring)
            }
            _ => None,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Spring => "spring",
        }
    }

    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Spring)
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

pub const COLUMNS_HARD_MIN: usize = 1;
pub const COLUMNS_HARD_MAX: usize = 12;

impl LocalShortcut {
    pub fn normalize_key_name(raw: &str) -> Option<String> {
        let lower = raw.trim().to_ascii_lowercase();
        match lower.as_str() {
            "delete" | "del" => Some("delete".to_string()),
            "backspace" | "back" => Some("backspace".to_string()),
            "+" | "=" | "plus" | "equal" | "equals" => Some("+".to_string()),
            "-" | "_" | "minus" => Some("-".to_string()),
            "[" | "]" | "," | "." | "/" => Some(lower),
            "up" | "down" | "left" | "right" => Some(lower),
            k if k.len() == 1
                && (k.as_bytes()[0].is_ascii_alphabetic() || k.as_bytes()[0].is_ascii_digit()) =>
            {
                Some(k.to_string())
            }
            k if k.starts_with('f') && k.len() <= 3 => {
                let n: u32 = k[1..].parse().ok()?;
                (1..=12).contains(&n).then(|| format!("f{n}"))
            }
            _ => None,
        }
    }

    pub fn parse(spec: &str) -> Option<Self> {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return None;
        }

        let (mod_prefix, trailing_plus) = if trimmed == "+" {
            ("", true)
        } else if let Some(prefix) = trimmed.strip_suffix("++") {
            (prefix, true)
        } else {
            (trimmed, false)
        };

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut key: Option<String> = trailing_plus.then(|| "+".to_string());

        for part in mod_prefix.split('+') {
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
            "up" => "Up".to_string(),
            "down" => "Down".to_string(),
            "left" => "Left".to_string(),
            "right" => "Right".to_string(),
            "+" => "+".to_string(),
            "-" => "-".to_string(),
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
        let key_matches = match self.key.as_str() {
            "+" => key == "+" || key == "=",
            "-" => key == "-" || key == "_",
            expected => expected.eq_ignore_ascii_case(key),
        };
        if !key_matches {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDesktopAction {
    Previous,
    Next,
    /// Zero-based native virtual desktop index (`0..=8` for keys `1..=9`).
    Index(usize),
}

/// Modifier used inside WinDeck overlay to move the selected window across native virtual desktops
/// (`<Mod> + ↑/↓` for previous/next desktop, `<Mod> + 1..9` for direct jump).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDesktopModifier {
    Alt,
    Ctrl,
    Shift,
    Off,
}

impl MoveDesktopModifier {
    pub fn parse(val: &str) -> Option<Self> {
        match val.trim().to_ascii_lowercase().as_str() {
            "alt" => Some(Self::Alt),
            "ctrl" | "control" => Some(Self::Ctrl),
            "shift" => Some(Self::Shift),
            "off" | "none" | "false" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Alt => "alt",
            Self::Ctrl => "ctrl",
            Self::Shift => "shift",
            Self::Off => "off",
        }
    }

    pub fn match_move_action(
        self,
        key: &str,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) -> Option<MoveDesktopAction> {
        let parse_digit = |k: &str| -> Option<MoveDesktopAction> {
            let b = k.as_bytes();
            if b.len() == 1 && (b'1'..=b'9').contains(&b[0]) {
                Some(MoveDesktopAction::Index((b[0] - b'1') as usize))
            } else {
                None
            }
        };

        match self {
            Self::Off => None,
            Self::Alt => {
                if !alt || ctrl || shift {
                    return None;
                }
                match key {
                    "up" => Some(MoveDesktopAction::Previous),
                    "down" => Some(MoveDesktopAction::Next),
                    other => parse_digit(other),
                }
            }
            Self::Ctrl => {
                if !ctrl || alt || shift {
                    return None;
                }
                match key {
                    "up" => Some(MoveDesktopAction::Previous),
                    "down" => Some(MoveDesktopAction::Next),
                    other => parse_digit(other),
                }
            }
            Self::Shift => {
                if ctrl || alt {
                    return None;
                }
                if shift {
                    return match key {
                        "up" => Some(MoveDesktopAction::Previous),
                        "down" => Some(MoveDesktopAction::Next),
                        other => parse_digit(other),
                    };
                }
                // On Windows, GPUI translates Shift+1..9 into shifted symbols ("!".."(")
                // and clears `modifiers.shift`.
                match key {
                    "!" => Some(MoveDesktopAction::Index(0)),
                    "@" => Some(MoveDesktopAction::Index(1)),
                    "#" => Some(MoveDesktopAction::Index(2)),
                    "$" => Some(MoveDesktopAction::Index(3)),
                    "%" => Some(MoveDesktopAction::Index(4)),
                    "^" => Some(MoveDesktopAction::Index(5)),
                    "&" => Some(MoveDesktopAction::Index(6)),
                    "*" => Some(MoveDesktopAction::Index(7)),
                    "(" => Some(MoveDesktopAction::Index(8)),
                    _ => None,
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub hotkey: String,
    pub force_quit_key: String,
    pub move_desktop_modifier: MoveDesktopModifier,
    pub columns_inc_key: String,
    pub columns_dec_key: String,
    pub columns: ColumnsConfig,
    pub columns_min: usize,
    pub columns_max: usize,
    pub background: BackdropStyle,
    pub blur_opacity: f32,
    pub transition_effect: TransitionEffect,
    pub transition_tab: bool,
    pub transition_selection: bool,
    pub transition_switch: bool,
    pub monitor: OverlayMonitorPolicy,
    pub immersive: bool,
    pub label_position: LabelPosition,
    pub preview_width: f32,
    pub preview_height: f32,
    pub preview_fit: PreviewFit,
    pub cross_desktop_nav: bool,
    pub hide_on_blur: bool,
    pub auto_update: bool,
    pub win_arrow_switch_desktop: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Q".to_string(),
            force_quit_key: "Delete".to_string(),
            move_desktop_modifier: MoveDesktopModifier::Alt,
            columns_inc_key: "Ctrl++".to_string(),
            columns_dec_key: "Ctrl+-".to_string(),
            columns: ColumnsConfig::Auto,
            columns_min: 1,
            columns_max: 8,
            background: BackdropStyle::Blurred,
            blur_opacity: 0.28,
            transition_effect: TransitionEffect::Spring,
            transition_tab: true,
            transition_selection: true,
            transition_switch: true,
            monitor: OverlayMonitorPolicy::Cursor,
            immersive: true,
            label_position: LabelPosition::BottomRight,
            preview_width: 380.0,
            preview_height: 226.0,
            preview_fit: PreviewFit::Cover,
            cross_desktop_nav: true,
            hide_on_blur: true,
            auto_update: true,
            win_arrow_switch_desktop: false,
        }
    }
}

const DEFAULT_CONFIG_TOML: &str = r#"# WinDeck 配置文件（保存后在托盘右键点击「重新加载配置」即可生效）

# 全局呼出快捷键（格式：主键 Ctrl / Alt / Win + 任意字母 A-Z，例如: "Alt+Q", "Ctrl+E", "Win+Q"）
# 呼出后保持主键不松、按方向键选窗，松开主键即可自动跳转到目标窗口；短按松开则停留在界面内自由操作
hotkey = "Alt+Q"

# 强制退出指定窗口快捷键（在切换器内选中或悬停目标窗口后按下，例如: "Delete", "Shift+Delete", "Ctrl+W", "Alt+F4"）
force_quit_key = "Delete"

# 快捷移动窗口到其他原生虚拟桌面修饰键（配合 ↑/↓ 移动至上/下桌面，1~9 直达指定桌面）: "alt" | "ctrl" | "shift" | "off"
move_desktop_modifier = "alt"

# 实时增减单行展示列数的快捷键（支持单键或组合键，如 "Ctrl++", "Ctrl+-", "Alt+]", "Alt+["）
columns_inc_key = "Ctrl++"
columns_dec_key = "Ctrl+-"

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

# 转场特效（仅支持通过配置文件修改）: "spring" (灵动，默认开启) | "off" (关闭)
transition_effect = "spring"

# 转场特效分场景开关（仅当 transition_effect = "spring" 时生效，仅支持通过配置文件修改）:
#   transition_tab       = 顶部虚拟桌面标签切换过渡（共有卡片滑移 + 交叉淡入淡出）
#   transition_selection = 键盘 Tab / 方向键焦点框滑移与平滑滚动
#   transition_switch    = 确认切窗口与跨桌面两阶段转场特效
transition_tab = true
transition_selection = true
transition_switch = true

# 每行卡片列数: "auto" (按固定预览宽度与屏幕自动排满) 或正整数 (如 4)
columns = "auto"

# 单行展示列数上下限 (1 ~ 12，约束自动列数与快捷键增减范围)
columns_min = 1
columns_max = 8

# 多显示器弹出位置: "cursor" (鼠标当前所在屏) | "primary" (主显示器)
monitor = "cursor"

# 方向键移到当前虚拟桌面组末尾时，是否自动衔接下一个虚拟桌面组
cross_desktop_nav = true

# 点击切换器外部（失去焦点）时是否自动隐藏
hide_on_blur = true

# 自动检查 GitHub Release 新版本更新（每次打开时后台静默检测）
auto_update = true

# 系统补丁：将系统 Win + 左/右方向键 映射为切换虚拟桌面（覆盖系统分屏，使用原生动画；管理员窗口不生效）
win_arrow_switch_desktop = false
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

pub fn virtual_spaces_file_path() -> PathBuf {
    config_dir().join("virtual_spaces.json")
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
                "move_desktop_modifier" => {
                    if let Some(m) = MoveDesktopModifier::parse(val) {
                        cfg.move_desktop_modifier = m;
                    }
                }
                "columns_inc_key" if !val.is_empty() => cfg.columns_inc_key = val.to_string(),
                "columns_dec_key" if !val.is_empty() => cfg.columns_dec_key = val.to_string(),
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
                        if (COLUMNS_HARD_MIN..=COLUMNS_HARD_MAX).contains(&n) {
                            cfg.columns = ColumnsConfig::Fixed(n);
                        }
                    }
                }
                "columns_min" => {
                    if let Ok(n) = val.parse::<usize>() {
                        cfg.columns_min = n.clamp(COLUMNS_HARD_MIN, COLUMNS_HARD_MAX);
                    }
                }
                "columns_max" => {
                    if let Ok(n) = val.parse::<usize>() {
                        cfg.columns_max = n.clamp(COLUMNS_HARD_MIN, COLUMNS_HARD_MAX);
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
                "transition_effect" => {
                    if let Some(effect) = TransitionEffect::parse(val) {
                        cfg.transition_effect = effect;
                    }
                }
                "transition_tab" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.transition_tab = b;
                    }
                }
                "transition_selection" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.transition_selection = b;
                    }
                }
                "transition_switch" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.transition_switch = b;
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
                "win_arrow_switch_desktop" => {
                    if let Ok(b) = val.parse::<bool>() {
                        cfg.win_arrow_switch_desktop = b;
                    }
                }
                _ => {}
            }
        }

        cfg.columns_min = cfg.columns_min.clamp(COLUMNS_HARD_MIN, COLUMNS_HARD_MAX);
        cfg.columns_max = cfg.columns_max.clamp(cfg.columns_min, COLUMNS_HARD_MAX);

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

    pub fn parsed_columns_inc_key(&self) -> LocalShortcut {
        LocalShortcut::parse(&self.columns_inc_key)
            .unwrap_or_else(|| LocalShortcut::parse("Ctrl++").expect("default columns_inc_key valid"))
    }

    pub fn parsed_columns_dec_key(&self) -> LocalShortcut {
        LocalShortcut::parse(&self.columns_dec_key)
            .unwrap_or_else(|| LocalShortcut::parse("Ctrl+-").expect("default columns_dec_key valid"))
    }

    pub fn effective_columns_bounds(&self) -> (usize, usize) {
        let min_c = self.columns_min.clamp(COLUMNS_HARD_MIN, COLUMNS_HARD_MAX);
        let max_c = self.columns_max.clamp(min_c, COLUMNS_HARD_MAX);
        (min_c, max_c)
    }

    pub fn tab_transition_enabled(&self) -> bool {
        self.transition_effect.is_enabled() && self.transition_tab
    }

    pub fn selection_transition_enabled(&self) -> bool {
        self.transition_effect.is_enabled() && self.transition_selection
    }

    pub fn switch_transition_enabled(&self) -> bool {
        self.transition_effect.is_enabled() && self.transition_switch
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
        let (columns_min, columns_max) = self.effective_columns_bounds();
        format!(
            r#"# WinDeck 配置文件（由设置页面自动保存，也可手动编辑后在托盘点「重新加载配置」）

# 全局呼出快捷键（主键 Ctrl / Alt / Win + 任意字母 A-Z，按住主键 + 方向键选窗，松开自动跳转）
hotkey = "{hotkey}"

# 强制退出指定窗口快捷键（选中或悬停目标窗口后按下，如 "Delete", "Shift+Delete", "Ctrl+W", "Alt+F4"）
force_quit_key = "{force_quit_key}"

# 快捷移动窗口到其他原生虚拟桌面修饰键（配合 ↑/↓ 移动至上/下桌面，1~9 直达指定桌面）: "alt" | "ctrl" | "shift" | "off"
move_desktop_modifier = "{move_desktop_modifier}"

# 实时增减单行展示列数的快捷键（支持单键或组合键，如 "Ctrl++", "Ctrl+-", "Alt+]", "Alt+["）
columns_inc_key = "{columns_inc_key}"
columns_dec_key = "{columns_dec_key}"

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

# 转场特效（仅支持通过配置文件修改）: "spring" (灵动) | "off" (关闭)
transition_effect = "{transition_effect}"
transition_tab = {transition_tab}
transition_selection = {transition_selection}
transition_switch = {transition_switch}

# 每行卡片列数: "auto" 或正整数 (如 4)
columns = {columns}

# 单行展示列数上下限 (1 ~ 12，约束自动列数与快捷键增减范围)
columns_min = {columns_min}
columns_max = {columns_max}

# 多显示器弹出位置: "cursor" (鼠标当前所在屏) | "primary" (主显示器)
monitor = "{monitor}"

# 方向键移到当前虚拟桌面组末尾时，是否自动衔接下一个虚拟桌面组
cross_desktop_nav = {cross_desktop_nav}

# 点击切换器外部（失去焦点）时是否自动隐藏
hide_on_blur = {hide_on_blur}

# 自动检查 GitHub Release 新版本更新（每次打开时后台静默检测）
auto_update = {auto_update}

# 系统补丁：将系统 Win + 左/右方向键 映射为切换虚拟桌面（覆盖系统分屏，使用原生动画；管理员窗口不生效）
win_arrow_switch_desktop = {win_arrow_switch_desktop}
"#,
            hotkey = self.parsed_hotkey().canonical_label(),
            force_quit_key = self.parsed_force_quit_key().canonical_label(),
            move_desktop_modifier = self.move_desktop_modifier.as_config_str(),
            columns_inc_key = self.parsed_columns_inc_key().canonical_label(),
            columns_dec_key = self.parsed_columns_dec_key().canonical_label(),
            immersive = self.immersive,
            preview_width = self.preview_width.round() as i32,
            preview_height = self.preview_height.round() as i32,
            preview_fit = self.preview_fit.as_config_str(),
            label_position = self.label_position.as_config_str(),
            background = background_str,
            blur_opacity = self.blur_opacity.clamp(0.05, 0.85),
            transition_effect = self.transition_effect.as_config_str(),
            transition_tab = self.transition_tab,
            transition_selection = self.transition_selection,
            transition_switch = self.transition_switch,
            columns = columns_str,
            columns_min = columns_min,
            columns_max = columns_max,
            monitor = monitor_str,
            cross_desktop_nav = self.cross_desktop_nav,
            hide_on_blur = self.hide_on_blur,
            auto_update = self.auto_update,
            win_arrow_switch_desktop = self.win_arrow_switch_desktop,
        )
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        fs::write(config_file_path(), self.to_toml())
    }

    /// Computes how many tiles to place in a single row, clamped to `[columns_min, columns_max]`.
    pub fn resolve_columns(&self, content_width_px: f32, tile_outer_w: f32, col_gap: f32) -> usize {
        let (min_c, max_c) = self.effective_columns_bounds();
        let natural_fit = (((content_width_px + col_gap) / (tile_outer_w + col_gap)).floor()
            as usize)
            .max(1);
        match self.columns {
            ColumnsConfig::Fixed(n) => n.clamp(min_c, max_c),
            ColumnsConfig::Auto => natural_fit.clamp(min_c, max_c),
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
            transition_effect = "spring"
            transition_tab = false
            transition_selection = true
            transition_switch = false
            columns = 4
            background = "transparent"
            monitor = "primary"
            cross_desktop_nav = false
            hide_on_blur = false
            auto_update = false
            win_arrow_switch_desktop = true
        "#;
        let cfg = AppConfig::parse(raw);
        assert_eq!(cfg.hotkey, "Ctrl+E");
        assert!(!cfg.immersive);
        assert_eq!(cfg.preview_width, 420.0);
        assert_eq!(cfg.preview_height, 250.0);
        assert_eq!(cfg.preview_fit, PreviewFit::Contain);
        assert_eq!(cfg.label_position, LabelPosition::Center);
        assert!((cfg.blur_opacity - 0.22).abs() < 0.001);
        assert_eq!(cfg.transition_effect, TransitionEffect::Spring);
        assert!(!cfg.transition_tab);
        assert!(cfg.transition_selection);
        assert!(!cfg.transition_switch);
        assert!(!cfg.tab_transition_enabled());
        assert!(cfg.selection_transition_enabled());
        assert!(!cfg.switch_transition_enabled());
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
        assert_eq!(round_tripped.transition_effect, cfg.transition_effect);
        assert_eq!(round_tripped.transition_tab, cfg.transition_tab);
        assert_eq!(round_tripped.transition_selection, cfg.transition_selection);
        assert_eq!(round_tripped.transition_switch, cfg.transition_switch);
        assert_eq!(round_tripped.columns, cfg.columns);

        let default_cfg = AppConfig::default();
        assert_eq!(default_cfg.transition_effect, TransitionEffect::Spring);
        assert!(default_cfg.tab_transition_enabled());
        assert!(default_cfg.selection_transition_enabled());
        assert!(default_cfg.switch_transition_enabled());
        assert!(!default_cfg.win_arrow_switch_desktop);

        let legacy_cfg = AppConfig::parse("transition_effect = \"smooth\"\n");
        assert_eq!(legacy_cfg.transition_effect, TransitionEffect::Spring);

        let off_cfg = AppConfig::parse("transition_effect = \"off\"\ntransition_tab = true\n");
        assert_eq!(off_cfg.transition_effect, TransitionEffect::Off);
        assert!(!off_cfg.tab_transition_enabled());
        assert!(!off_cfg.selection_transition_enabled());
        assert!(!off_cfg.switch_transition_enabled());
        assert_eq!(round_tripped.background, cfg.background);
        assert_eq!(round_tripped.monitor, cfg.monitor);
        assert_eq!(round_tripped.cross_desktop_nav, cfg.cross_desktop_nav);
        assert_eq!(round_tripped.hide_on_blur, cfg.hide_on_blur);
        assert!(!cfg.auto_update);
        assert_eq!(round_tripped.auto_update, cfg.auto_update);
        assert!(cfg.win_arrow_switch_desktop);
        assert_eq!(
            round_tripped.win_arrow_switch_desktop,
            cfg.win_arrow_switch_desktop
        );
    }

    #[test]
    fn move_desktop_modifier_parses_round_trips_and_matches_keys() {
        assert_eq!(AppConfig::default().move_desktop_modifier, MoveDesktopModifier::Alt);
        assert_eq!(MoveDesktopModifier::parse("ctrl"), Some(MoveDesktopModifier::Ctrl));
        assert_eq!(MoveDesktopModifier::parse("shift"), Some(MoveDesktopModifier::Shift));
        assert_eq!(MoveDesktopModifier::parse("off"), Some(MoveDesktopModifier::Off));

        let custom = AppConfig::parse("move_desktop_modifier = \"shift\"\n");
        assert_eq!(custom.move_desktop_modifier, MoveDesktopModifier::Shift);
        assert_eq!(
            AppConfig::parse(&custom.to_toml()).move_desktop_modifier,
            MoveDesktopModifier::Shift
        );

        // Alt mode
        assert_eq!(
            MoveDesktopModifier::Alt.match_move_action("up", false, true, false),
            Some(MoveDesktopAction::Previous)
        );
        assert_eq!(
            MoveDesktopModifier::Alt.match_move_action("down", false, true, false),
            Some(MoveDesktopAction::Next)
        );
        assert_eq!(
            MoveDesktopModifier::Alt.match_move_action("3", false, true, false),
            Some(MoveDesktopAction::Index(2))
        );
        assert_eq!(
            MoveDesktopModifier::Alt.match_move_action("down", true, true, false),
            None
        );

        // Shift mode handles both explicit Shift+Up/Down and Windows GPUI shifted symbols ("!".."(")
        assert_eq!(
            MoveDesktopModifier::Shift.match_move_action("up", false, false, true),
            Some(MoveDesktopAction::Previous)
        );
        assert_eq!(
            MoveDesktopModifier::Shift.match_move_action("!", false, false, false),
            Some(MoveDesktopAction::Index(0))
        );
        assert_eq!(
            MoveDesktopModifier::Shift.match_move_action("(", false, false, false),
            Some(MoveDesktopAction::Index(8))
        );
        assert_eq!(
            MoveDesktopModifier::Shift.match_move_action("tab", false, false, true),
            None
        );

        // Off mode disables all move shortcuts
        assert_eq!(
            MoveDesktopModifier::Off.match_move_action("down", false, true, false),
            None
        );
    }

    #[test]
    fn columns_shortcuts_and_bounds_parse_match_and_round_trip() {
        let def = AppConfig::default();
        assert_eq!(def.parsed_columns_inc_key().canonical_label(), "Ctrl++");
        assert_eq!(def.parsed_columns_dec_key().canonical_label(), "Ctrl+-");
        assert_eq!(def.effective_columns_bounds(), (1, 8));

        // Ctrl++ matches both main-keyboard '=' and shifted/numpad '+'
        let inc = def.parsed_columns_inc_key();
        assert!(inc.matches("=", true, false, false, None));
        assert!(inc.matches("+", true, false, false, None));
        assert!(!inc.matches("=", false, false, false, None));

        // Ctrl+- matches both '-' and '_'
        let dec = def.parsed_columns_dec_key();
        assert!(dec.matches("-", true, false, false, None));
        assert!(dec.matches("_", true, false, false, None));
        assert!(!dec.matches("-", false, false, false, None));

        // Alternate syntaxes parse cleanly
        assert_eq!(
            LocalShortcut::parse("Ctrl+=").unwrap().canonical_label(),
            "Ctrl++"
        );
        assert_eq!(
            LocalShortcut::parse("Alt+Shift++").unwrap().canonical_label(),
            "Alt+Shift++"
        );
        assert_eq!(LocalShortcut::parse("+").unwrap().canonical_label(), "+");
        assert_eq!(LocalShortcut::parse("Ctrl+]").unwrap().canonical_label(), "Ctrl+]");

        // Config parse + round-trip + inverted min/max normalization
        let cfg = AppConfig::parse(
            "columns = 6\ncolumns_min = 3\ncolumns_max = 10\ncolumns_inc_key = \"Alt+]\"\ncolumns_dec_key = \"Alt+[\"\n",
        );
        assert_eq!(cfg.columns, ColumnsConfig::Fixed(6));
        assert_eq!(cfg.effective_columns_bounds(), (3, 10));
        assert_eq!(cfg.parsed_columns_inc_key().canonical_label(), "Alt+]");
        assert_eq!(cfg.parsed_columns_dec_key().canonical_label(), "Alt+[");
        assert_eq!(cfg.resolve_columns(1000.0, 400.0, 20.0), 6);

        let rt = AppConfig::parse(&cfg.to_toml());
        assert_eq!(rt.columns, ColumnsConfig::Fixed(6));
        assert_eq!(rt.effective_columns_bounds(), (3, 10));
        assert_eq!(rt.parsed_columns_inc_key().canonical_label(), "Alt+]");
        assert_eq!(rt.parsed_columns_dec_key().canonical_label(), "Alt+[");

        // Inverted min > max clamps max up to min, and Auto respects [min, max]
        let clamped = AppConfig::parse("columns = \"auto\"\ncolumns_min = 5\ncolumns_max = 2\n");
        assert_eq!(clamped.effective_columns_bounds(), (5, 5));
        assert_eq!(clamped.resolve_columns(1200.0, 400.0, 20.0), 5);
    }
}
