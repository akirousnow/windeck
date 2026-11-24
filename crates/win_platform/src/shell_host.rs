use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::System::SystemInformation::{GetTickCount, GetTickCount64};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, LASTINPUTINFO,
    MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_WIN, MapVirtualKeyW, RegisterHotKey,
    SendInput, UnregisterHotKey, VIRTUAL_KEY, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LCONTROL,
    VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU,
    VK_RSHIFT, VK_RWIN, VK_UP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, AppendMenuW, CallNextHookEx, CreateIconIndirect,
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowExW, GWLP_USERDATA, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, HHOOK, HICON, HWND_MESSAGE, ICONINFO, IMAGE_ICON, KBDLLHOOKSTRUCT,
    KillTimer, LLKHF_INJECTED, LR_DEFAULTCOLOR, LoadImageW, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, MSG, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowsHookExW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_POWERBROADCAST, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_POPUP, WTS_CONSOLE_CONNECT, WTS_CONSOLE_DISCONNECT, WTS_REMOTE_CONNECT,
    WTS_REMOTE_DISCONNECT, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use windows_core::{HSTRING, PCWSTR, w};
use windows_registry::CURRENT_USER;

const HOTKEY_ID: i32 = 1;
const WM_TRAY_ICON: u32 = WM_APP + 1;
const WM_REBIND_HOTKEY: u32 = WM_APP + 2;
const WM_ACTIVATE_EXISTING: u32 = WM_APP + 3;
const WM_HOLD_NAV: u32 = WM_APP + 4;
const WM_HOLD_RELEASED: u32 = WM_APP + 5;
const WM_DISARM_HOLD_HOOK: u32 = WM_APP + 6;
const WM_SHOW_BALLOON: u32 = WM_APP + 7;
const WM_SET_WIN_ARROW_PATCH: u32 = WM_APP + 8;
const WM_WIN_ARROW_SWITCH: u32 = WM_APP + 9;
const WM_WIN_ARROW_MASK: u32 = WM_APP + 10;
const WM_EXPLORER_RESTARTED: u32 = WM_APP + 11;
const WM_SYSTEM_RESUMED: u32 = WM_APP + 12;
const WM_SESSION_LOCKED: u32 = WM_APP + 13;
const WM_SESSION_UNLOCKED: u32 = WM_APP + 14;
const WM_SYNC_WATCHDOG_TIMER: u32 = WM_APP + 15;

const WM_KBD_SET_PATCH: u32 = WM_APP + 40;
const WM_KBD_ARM_HOLD: u32 = WM_APP + 41;
const WM_KBD_DISARM_HOLD: u32 = WM_APP + 42;
const WM_KBD_RESET_MODS: u32 = WM_APP + 43;
const WM_KBD_REFRESH_HOOK: u32 = WM_APP + 44;

const TIMER_HOOK_WATCHDOG: usize = 1;
const TIMER_MOD_RECONCILE: usize = 2;

const WATCHDOG_INTERVAL_MS: u32 = 30_000;
const WATCHDOG_STALE_ACTIVITY_MS: u64 = 25_000;
const WATCHDOG_MIN_IDLE_MS: u32 = 2_000;
const MOD_RECONCILE_INTERVAL_MS: u32 = 150;
const STALE_MOD_RECONCILE_MS: u64 = 150;
const SYNTHETIC_CTRL_GRACE_MS: u64 = 120;

const PBT_APMRESUMESUSPEND_CODE: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC_CODE: usize = 0x0012;

const WINDECK_SYNTHETIC_EXTRA_INFO: usize = 0x5744_4B53; // "WDKS"
// Unassigned virtual key (same convention as AutoHotkey `#MenuMaskKey`) that suppresses
// Start Menu activation on bare Win keyup without triggering any app or shell shortcut.
const VK_MENU_MASK: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

const PHYS_MOD_LWIN: u8 = 1 << 0;
const PHYS_MOD_RWIN: u8 = 1 << 1;
const PHYS_MOD_LCTRL: u8 = 1 << 2;
const PHYS_MOD_RCTRL: u8 = 1 << 3;
const PHYS_MOD_LALT: u8 = 1 << 4;
const PHYS_MOD_RALT: u8 = 1 << 5;
const PHYS_MOD_LSHIFT: u8 = 1 << 6;
const PHYS_MOD_RSHIFT: u8 = 1 << 7;

const PHYS_MOD_WIN_MASK: u8 = PHYS_MOD_LWIN | PHYS_MOD_RWIN;
const PHYS_MOD_CTRL_MASK: u8 = PHYS_MOD_LCTRL | PHYS_MOD_RCTRL;
const PHYS_MOD_ALT_MASK: u8 = PHYS_MOD_LALT | PHYS_MOD_RALT;
const PHYS_MOD_SHIFT_MASK: u8 = PHYS_MOD_LSHIFT | PHYS_MOD_RSHIFT;

const MENU_CMD_TOGGLE: u32 = 1001;
const MENU_CMD_RELOAD: u32 = 1002;
const MENU_CMD_OPEN_CONFIG: u32 = 1003;
const MENU_CMD_AUTOSTART: u32 = 1004;
const MENU_CMD_QUIT: u32 = 1005;
const MENU_CMD_SETTINGS: u32 = 1006;
const MENU_CMD_CHECK_UPDATE: u32 = 1007;

const RUN_REG_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE_NAME: &str = "WinDeck";
const SINGLE_INSTANCE_MUTEX_NAME: PCWSTR = w!("Local\\WinDeck_SingleInstance_v1");

/// RAII process-wide single-instance guard backed by a Win32 named mutex.
pub struct SingleInstanceGuard {
    handle: HANDLE,
}

unsafe impl Send for SingleInstanceGuard {}

impl SingleInstanceGuard {
    /// Returns `Ok(Some(guard))` if this process is the sole running WinDeck instance,
    /// or `Ok(None)` if another instance already holds the mutex.
    pub fn try_acquire() -> Result<Option<Self>> {
        let handle = unsafe { CreateMutexW(None, false, SINGLE_INSTANCE_MUTEX_NAME) }
            .context("CreateMutexW for WinDeck single-instance guard")?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }

    /// Explicitly closes the mutex handle before spawning a replacement process during self-update.
    pub fn release(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
            self.handle = HANDLE::default();
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Signals an already-running WinDeck instance (via its hidden `HWND_MESSAGE` host) to pop open.
pub fn notify_existing_instance() -> bool {
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        if let Ok(existing) = FindWindowExW(
            Some(HWND_MESSAGE),
            None,
            w!("WinDeckShellHostClass"),
            w!("WinDeckShellHost"),
        ) {
            if !existing.is_invalid() {
                return PostMessageW(Some(existing), WM_ACTIVATE_EXISTING, WPARAM(0), LPARAM(0))
                    .is_ok();
            }
        }
        false
    }
}

/// Checks whether WinDeck is registered under `HKCU\...\Run` for Windows login autostart.
pub fn is_autostart_enabled() -> bool {
    CURRENT_USER
        .open(RUN_REG_PATH)
        .ok()
        .and_then(|k| k.get_string(RUN_VALUE_NAME).ok())
        .is_some_and(|v| !v.trim().is_empty())
}

/// Enables or disables Windows login autostart (`"<current_exe>" --hidden`).
pub fn set_autostart_enabled(enabled: bool) -> Result<()> {
    let key = CURRENT_USER
        .create(RUN_REG_PATH)
        .context("open/create HKCU Run key")?;
    if enabled {
        let exe = std::env::current_exe().context("resolve current exe path for autostart")?;
        let cmd = format!("\"{}\" --hidden", exe.display());
        key.set_string(RUN_VALUE_NAME, &cmd)
            .context("write WinDeck autostart value")?;
    } else {
        let _ = key.remove_value(RUN_VALUE_NAME);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryModifier {
    Ctrl,
    Alt,
    Win,
}

impl PrimaryModifier {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Ctrl => "Ctrl",
            Self::Alt => "Alt",
            Self::Win => "Win",
        }
    }

    pub fn win32_mod_flag(self) -> u32 {
        match self {
            Self::Ctrl => MOD_CONTROL.0,
            Self::Alt => MOD_ALT.0,
            Self::Win => MOD_WIN.0,
        }
    }

    pub fn from_mod_flags(mods: u32) -> Option<Self> {
        if (mods & MOD_CONTROL.0) != 0 {
            Some(Self::Ctrl)
        } else if (mods & MOD_ALT.0) != 0 {
            Some(Self::Alt)
        } else if (mods & MOD_WIN.0) != 0 {
            Some(Self::Win)
        } else {
            None
        }
    }

    fn is_vk_down(vk: u16) -> bool {
        (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
    }

    pub fn is_physically_held(self) -> bool {
        match self {
            Self::Ctrl => Self::is_vk_down(VK_LCONTROL.0) || Self::is_vk_down(VK_RCONTROL.0),
            Self::Alt => Self::is_vk_down(VK_LMENU.0) || Self::is_vk_down(VK_RMENU.0),
            Self::Win => Self::is_vk_down(VK_LWIN.0) || Self::is_vk_down(VK_RWIN.0),
        }
    }

    fn matches_vk(self, vk: u32) -> bool {
        match self {
            Self::Ctrl => {
                vk == VK_LCONTROL.0 as u32 || vk == VK_RCONTROL.0 as u32 || vk == VK_CONTROL.0 as u32
            }
            Self::Alt => {
                vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 || vk == VK_MENU.0 as u32
            }
            Self::Win => vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32,
        }
    }

    fn sibling_still_held(self, released_vk: u32) -> bool {
        match self {
            Self::Ctrl => {
                if released_vk == VK_LCONTROL.0 as u32 {
                    Self::is_vk_down(VK_RCONTROL.0)
                } else if released_vk == VK_RCONTROL.0 as u32 {
                    Self::is_vk_down(VK_LCONTROL.0)
                } else {
                    false
                }
            }
            Self::Alt => {
                if released_vk == VK_LMENU.0 as u32 {
                    Self::is_vk_down(VK_RMENU.0)
                } else if released_vk == VK_RMENU.0 as u32 {
                    Self::is_vk_down(VK_LMENU.0)
                } else {
                    false
                }
            }
            Self::Win => {
                if released_vk == VK_LWIN.0 as u32 {
                    Self::is_vk_down(VK_RWIN.0)
                } else if released_vk == VK_RWIN.0 as u32 {
                    Self::is_vk_down(VK_LWIN.0)
                } else {
                    false
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldNavDirection {
    Left,
    Right,
    Up,
    Down,
}

impl HoldNavDirection {
    fn to_wparam(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Up => 2,
            Self::Down => 3,
        }
    }

    fn from_wparam(v: usize) -> Option<Self> {
        match v {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            2 => Some(Self::Up),
            3 => Some(Self::Down),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeySpec {
    pub primary: PrimaryModifier,
    pub modifiers: u32,
    pub vk: u32,
}

impl HotkeySpec {
    /// Parses a `(Ctrl | Alt | Win) + <Letter>` hotkey such as `"Alt+Q"`, `"Ctrl+E"`, or `"Win+Q"`.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut primary: Option<PrimaryModifier> = None;
        let mut vk: Option<u32> = None;

        for raw in spec.split('+') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "alt" => {
                    if primary.replace(PrimaryModifier::Alt).is_some() {
                        return Err(anyhow!(
                            "Hotkey '{spec}' must use a single primary modifier (Ctrl, Alt, or Win)"
                        ));
                    }
                }
                "ctrl" | "control" => {
                    if primary.replace(PrimaryModifier::Ctrl).is_some() {
                        return Err(anyhow!(
                            "Hotkey '{spec}' must use a single primary modifier (Ctrl, Alt, or Win)"
                        ));
                    }
                }
                "win" | "super" | "meta" => {
                    if primary.replace(PrimaryModifier::Win).is_some() {
                        return Err(anyhow!(
                            "Hotkey '{spec}' must use a single primary modifier (Ctrl, Alt, or Win)"
                        ));
                    }
                }
                other if other.len() == 1 => {
                    let ch = other.as_bytes()[0].to_ascii_uppercase();
                    if ch.is_ascii_alphabetic() {
                        if vk.replace(ch as u32).is_some() {
                            return Err(anyhow!("Hotkey '{spec}' must contain only one letter key"));
                        }
                    } else {
                        return Err(anyhow!(
                            "Unsupported hotkey key '{token}': secondary key must be an A-Z letter"
                        ));
                    }
                }
                _ => {
                    return Err(anyhow!(
                        "Unsupported hotkey token '{token}': format must be (Ctrl|Alt|Win)+<A-Z>"
                    ));
                }
            }
        }

        let primary = primary.ok_or_else(|| {
            anyhow!("Hotkey '{spec}' must include a primary modifier (Ctrl, Alt, or Win)")
        })?;
        let vk = vk.ok_or_else(|| {
            anyhow!("Hotkey '{spec}' must include a secondary letter key (A-Z)")
        })?;

        Ok(Self {
            primary,
            modifiers: MOD_NOREPEAT.0 | primary.win32_mod_flag(),
            vk,
        })
    }

    pub fn canonical_label(self) -> String {
        let letter = char::from_u32(self.vk).unwrap_or('Q');
        format!("{}+{}", self.primary.display_name(), letter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopSwitchDir {
    Left,
    Right,
}

impl DesktopSwitchDir {
    fn to_wparam(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }

    fn from_wparam(v: usize) -> Option<Self> {
        match v {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            _ => None,
        }
    }

    fn arrow_vk(self) -> VIRTUAL_KEY {
        match self {
            Self::Left => VK_LEFT,
            Self::Right => VK_RIGHT,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ShellEvent {
    ToggleOverlay {
        hotkey_triggered: bool,
        hold_armed: bool,
    },
    ShowOverlay,
    OpenSettings,
    CheckUpdate,
    OpenUpdateDialog,
    HoldNavigate(HoldNavDirection),
    HoldModifierReleased {
        commit: bool,
    },
    WinArrowComFallback(DesktopSwitchDir),
    ReloadConfig,
    OpenConfigDir,
    AutostartToggled(bool),
    ExplorerRestarted,
    Quit,
}

struct RebindRequest {
    hotkey: HotkeySpec,
    reply: mpsc::Sender<Result<()>>,
}

struct WinArrowPatchRequest {
    enabled: bool,
    reply: mpsc::Sender<Result<()>>,
}

struct ArmHoldRequest {
    primary: PrimaryModifier,
    reply: mpsc::Sender<bool>,
}

struct BalloonRequest {
    title: String,
    body: String,
}

struct ShellHostStartup {
    hwnd_raw: isize,
    hotkey_registered: bool,
    hotkey_error: Option<String>,
    win_arrow_patch_error: Option<String>,
}

struct OverlayFocusState {
    visible: AtomicBool,
    gpui_hwnd: AtomicIsize,
    host_hwnd: AtomicIsize,
}

impl OverlayFocusState {
    fn new() -> Self {
        Self {
            visible: AtomicBool::new(false),
            gpui_hwnd: AtomicIsize::new(0),
            host_hwnd: AtomicIsize::new(0),
        }
    }

    fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Release);
    }

    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    fn register_hwnds(&self, gpui_hwnd: HWND, host_hwnd: HWND) {
        self.gpui_hwnd.store(gpui_hwnd.0 as isize, Ordering::Release);
        self.host_hwnd.store(host_hwnd.0 as isize, Ordering::Release);
    }

    fn is_overlay_foreground_with(&self, fg_raw: isize) -> bool {
        if !self.is_visible() {
            return false;
        }
        let gpui = self.gpui_hwnd.load(Ordering::Acquire);
        let host = self.host_hwnd.load(Ordering::Acquire);
        if gpui == 0 && host == 0 {
            return true;
        }
        fg_raw != 0 && (fg_raw == gpui || fg_raw == host)
    }

    fn is_overlay_foreground(&self) -> bool {
        if !self.is_visible() {
            return false;
        }
        let fg = unsafe { GetForegroundWindow() }.0 as isize;
        self.is_overlay_foreground_with(fg)
    }
}

struct SharedHookSync {
    overlay: Arc<OverlayFocusState>,
    physical_mods: AtomicU8,
    synthetic_ctrl_until_ms: AtomicU64,
    last_hook_event_ms: AtomicU64,
    patch_enabled: AtomicBool,
}

impl SharedHookSync {
    fn new(overlay: Arc<OverlayFocusState>, patch_enabled: bool) -> Self {
        Self {
            overlay,
            physical_mods: AtomicU8::new(0),
            synthetic_ctrl_until_ms: AtomicU64::new(0),
            last_hook_event_ms: AtomicU64::new(0),
            patch_enabled: AtomicBool::new(patch_enabled),
        }
    }

    fn in_synthetic_ctrl_window(&self, now_ms: u64) -> bool {
        now_ms <= self.synthetic_ctrl_until_ms.load(Ordering::Acquire)
    }
}

struct BroadcastWindowState {
    shell_hwnd: HWND,
    taskbar_created_msg: u32,
    wts_registered: bool,
}

struct ShellHostState {
    on_event: Box<dyn Fn(ShellEvent) + Send>,
    tray_icon: HICON,
    tooltip: String,
    hotkey: HotkeySpec,
    hotkey_label: String,
    taskbar_created_msg: u32,
    hotkey_registered: bool,
    broadcast_hwnd: HWND,
    watchdog_armed: bool,
    hook_sync: Arc<SharedHookSync>,
    kbd_worker: KbdHookWorker,
}

fn monotonic_ms() -> u64 {
    unsafe { GetTickCount64() }
}

fn user_idle_ms() -> Option<u32> {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut lii) }.as_bool() {
        let now = unsafe { GetTickCount() };
        Some(now.wrapping_sub(lii.dwTime))
    } else {
        None
    }
}

fn is_shift_physically_held() -> bool {
    PrimaryModifier::is_vk_down(VK_LSHIFT.0) || PrimaryModifier::is_vk_down(VK_RSHIFT.0)
}

fn capture_async_mod_bits() -> u8 {
    let mut bits = 0u8;
    if PrimaryModifier::is_vk_down(VK_LWIN.0) {
        bits |= PHYS_MOD_LWIN;
    }
    if PrimaryModifier::is_vk_down(VK_RWIN.0) {
        bits |= PHYS_MOD_RWIN;
    }
    if PrimaryModifier::is_vk_down(VK_LCONTROL.0) {
        bits |= PHYS_MOD_LCTRL;
    }
    if PrimaryModifier::is_vk_down(VK_RCONTROL.0) {
        bits |= PHYS_MOD_RCTRL;
    }
    if PrimaryModifier::is_vk_down(VK_LMENU.0) {
        bits |= PHYS_MOD_LALT;
    }
    if PrimaryModifier::is_vk_down(VK_RMENU.0) {
        bits |= PHYS_MOD_RALT;
    }
    if PrimaryModifier::is_vk_down(VK_LSHIFT.0) {
        bits |= PHYS_MOD_LSHIFT;
    }
    if PrimaryModifier::is_vk_down(VK_RSHIFT.0) {
        bits |= PHYS_MOD_RSHIFT;
    }
    bits
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierSnapshot {
    win: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl ModifierSnapshot {
    fn from_bits(bits: u8) -> Self {
        Self {
            win: (bits & PHYS_MOD_WIN_MASK) != 0,
            ctrl: (bits & PHYS_MOD_CTRL_MASK) != 0,
            alt: (bits & PHYS_MOD_ALT_MASK) != 0,
            shift: (bits & PHYS_MOD_SHIFT_MASK) != 0,
        }
    }

    fn is_pure_win_only(self) -> bool {
        self.win && !self.ctrl && !self.alt && !self.shift
    }
}

/// Tracks physical (non-injected) modifier keys in the exact FIFO order delivered by
/// `WH_KEYBOARD_LL`, eliminating both the fast-release `GetAsyncKeyState(VK_LWIN)` race
/// and synthetic `SendInput(VK_LCONTROL)` self-pollution while reconciling missed keyups
/// after UAC / Secure Desktop / elevated-window focus switches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PhysicalModifierTracker {
    bits: u8,
    mismatch_since: [Option<u64>; 8],
}

impl PhysicalModifierTracker {
    fn index_for_vk(vk: u32) -> Option<usize> {
        match vk {
            x if x == VK_LWIN.0 as u32 => Some(0),
            x if x == VK_RWIN.0 as u32 => Some(1),
            x if x == VK_LCONTROL.0 as u32 => Some(2),
            x if x == VK_RCONTROL.0 as u32 => Some(3),
            x if x == VK_LMENU.0 as u32 => Some(4),
            x if x == VK_RMENU.0 as u32 => Some(5),
            x if x == VK_LSHIFT.0 as u32 => Some(6),
            x if x == VK_RSHIFT.0 as u32 => Some(7),
            _ => None,
        }
    }

    fn bits(&self) -> u8 {
        self.bits
    }

    fn any_held(&self) -> bool {
        self.bits != 0
    }

    fn clear(&mut self) {
        self.bits = 0;
        self.mismatch_since = [None; 8];
    }

    fn seed_from_async(&mut self, async_bits: u8, ignore_lctrl_async: bool) {
        self.bits = if ignore_lctrl_async {
            async_bits & !PHYS_MOD_LCTRL
        } else {
            async_bits
        };
        self.mismatch_since = [None; 8];
    }

    fn on_non_injected_key(&mut self, vk: u32, is_keydown: bool, is_keyup: bool) -> bool {
        let Some(idx) = Self::index_for_vk(vk) else {
            return false;
        };
        let mask = 1u8 << idx;
        if is_keydown {
            self.bits |= mask;
            self.mismatch_since[idx] = None;
        } else if is_keyup {
            self.bits &= !mask;
            self.mismatch_since[idx] = None;
        }
        true
    }

    fn reconcile_with_async(&mut self, async_bits: u8, now_ms: u64) -> u8 {
        for idx in 0..8 {
            let mask = 1u8 << idx;
            if (self.bits & mask) == 0 {
                self.mismatch_since[idx] = None;
                continue;
            }
            if (async_bits & mask) != 0 {
                self.mismatch_since[idx] = None;
            } else if let Some(since) = self.mismatch_since[idx] {
                if now_ms.saturating_sub(since) >= STALE_MOD_RECONCILE_MS {
                    self.bits &= !mask;
                    self.mismatch_since[idx] = None;
                }
            } else {
                self.mismatch_since[idx] = Some(now_ms);
            }
        }
        self.bits
    }

    fn snapshot_for_arrow(
        &mut self,
        async_bits: u8,
        now_ms: u64,
        ignore_lctrl_async: bool,
    ) -> ModifierSnapshot {
        self.reconcile_with_async(async_bits, now_ms);
        let filtered_async = if ignore_lctrl_async {
            async_bits & !PHYS_MOD_LCTRL
        } else {
            async_bits
        };
        ModifierSnapshot::from_bits(self.bits | filtered_async)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WinArrowDecision {
    Pass,
    SwallowOnly,
    SwallowAndMask,
    SwallowAndSwitch(DesktopSwitchDir),
}

#[derive(Debug, Default)]
struct WinArrowPatchState {
    enabled: bool,
    swallowed_left: bool,
    swallowed_right: bool,
}

impl WinArrowPatchState {
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset_swallowed();
        }
    }

    fn reset_swallowed(&mut self) {
        self.swallowed_left = false;
        self.swallowed_right = false;
    }

    fn any_swallowed(&self) -> bool {
        self.swallowed_left || self.swallowed_right
    }

    fn handle_key(
        &mut self,
        vk: u32,
        is_keydown: bool,
        is_keyup: bool,
        overlay_foreground: bool,
        mods: ModifierSnapshot,
    ) -> WinArrowDecision {
        let dir = if vk == VK_LEFT.0 as u32 {
            DesktopSwitchDir::Left
        } else if vk == VK_RIGHT.0 as u32 {
            DesktopSwitchDir::Right
        } else {
            return WinArrowDecision::Pass;
        };

        let swallowed = match dir {
            DesktopSwitchDir::Left => &mut self.swallowed_left,
            DesktopSwitchDir::Right => &mut self.swallowed_right,
        };

        // Always swallow the matching KEYUP for an arrow key whose KEYDOWN we swallowed,
        // even if Win was released first or the overlay just appeared.
        if is_keyup {
            if *swallowed {
                *swallowed = false;
                return WinArrowDecision::SwallowOnly;
            }
            return WinArrowDecision::Pass;
        }

        if !is_keydown {
            return WinArrowDecision::Pass;
        }

        if !self.enabled {
            *swallowed = false;
            return WinArrowDecision::Pass;
        }

        if mods.is_pure_win_only() {
            if !*swallowed {
                *swallowed = true;
                if overlay_foreground {
                    // Built-in Win+Arrow snap remains disabled even while WinDeck's own panel is
                    // foreground; emit a harmless menu-mask key so releasing Win won't open Start.
                    WinArrowDecision::SwallowAndMask
                } else {
                    WinArrowDecision::SwallowAndSwitch(dir)
                }
            } else {
                // Swallow OS key-repeat while held without spamming desktop switches.
                WinArrowDecision::SwallowOnly
            }
        } else {
            *swallowed = false;
            WinArrowDecision::Pass
        }
    }
}

fn keybd_input(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                dwExtraInfo: WINDECK_SYNTHETIC_EXTRA_INFO,
                ..Default::default()
            },
        },
    }
}

/// Builds the atomic `Ctrl + Win + Left/Right` `SendInput` batch.
/// Always wraps the arrow tap inside `VK_LCONTROL` down/up so a rapid follow-up switch can
/// never emit a bare `Win + Arrow` sequence that would trigger Windows' built-in Snap.
fn build_native_desktop_switch_inputs(dir: DesktopSwitchDir, win_already_held: bool) -> Vec<INPUT> {
    let arrow_vk = dir.arrow_vk();
    let inject_win = !win_already_held;
    let mut inputs = Vec::with_capacity(6);

    inputs.push(keybd_input(VK_LCONTROL, KEYBD_EVENT_FLAGS(0)));
    if inject_win {
        inputs.push(keybd_input(VK_LWIN, KEYEVENTF_EXTENDEDKEY));
    }
    inputs.push(keybd_input(arrow_vk, KEYEVENTF_EXTENDEDKEY));
    inputs.push(keybd_input(
        arrow_vk,
        KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
    ));
    if inject_win {
        inputs.push(keybd_input(
            VK_LWIN,
            KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
        ));
    }
    inputs.push(keybd_input(VK_LCONTROL, KEYEVENTF_KEYUP));
    inputs
}

fn build_menu_mask_inputs() -> [INPUT; 2] {
    [
        keybd_input(VK_MENU_MASK, KEYBD_EVENT_FLAGS(0)),
        keybd_input(VK_MENU_MASK, KEYEVENTF_KEYUP),
    ]
}

fn send_menu_mask_key() {
    let inputs = build_menu_mask_inputs();
    unsafe {
        let _ = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Synthesizes Windows' native `Ctrl + Win + Left/Right` virtual desktop switch shortcut.
/// Keeps a physically held `Win` key untouched so users can keep holding `Win` and tap
/// arrows repeatedly across multiple desktops without triggering the Start Menu.
/// Returns `false` only if `SendInput` was blocked (e.g. transient UIPI elevation boundary).
fn send_native_desktop_switch(dir: DesktopSwitchDir, sync: &SharedHookSync) -> bool {
    let phys = sync.physical_mods.load(Ordering::Acquire);
    if (phys & (PHYS_MOD_ALT_MASK | PHYS_MOD_SHIFT_MASK | PHYS_MOD_CTRL_MASK)) != 0
        || PrimaryModifier::Alt.is_physically_held()
        || is_shift_physically_held()
    {
        return true;
    }

    // Check live Win key state directly (we only ever inject LWin on the fast-release path,
    // and calls are serialized on this thread so no synthetic LWin is in flight here).
    let win_held = PrimaryModifier::Win.is_physically_held();
    let inputs = build_native_desktop_switch_inputs(dir, win_held);

    let now_ms = monotonic_ms();
    sync.synthetic_ctrl_until_ms
        .store(now_ms.saturating_add(SYNTHETIC_CTRL_GRACE_MS), Ordering::Release);

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };

    let done_ms = monotonic_ms();
    sync.synthetic_ctrl_until_ms.store(
        done_ms.saturating_add(SYNTHETIC_CTRL_GRACE_MS),
        Ordering::Release,
    );

    sent as usize == inputs.len()
}

struct HoldHookContext {
    primary: PrimaryModifier,
    navigated: bool,
}

#[derive(Default)]
struct KeyboardHookState {
    hook: Option<HHOOK>,
    kbd_hwnd: HWND,
    shell_hwnd: HWND,
    sync: Option<Arc<SharedHookSync>>,
    hold: Option<HoldHookContext>,
    modifiers: PhysicalModifierTracker,
    reconcile_timer_armed: bool,
    win_arrow_patch: WinArrowPatchState,
}

thread_local! {
    static KEYBOARD_HOOK: RefCell<KeyboardHookState> = RefCell::new(KeyboardHookState::default());
}

fn update_reconcile_timer(state: &mut KeyboardHookState) {
    if state.kbd_hwnd.is_invalid() {
        state.reconcile_timer_armed = false;
        return;
    }
    let want_timer = state.hook.is_some() && state.modifiers.any_held();
    if want_timer && !state.reconcile_timer_armed {
        unsafe {
            let _ = SetTimer(
                Some(state.kbd_hwnd),
                TIMER_MOD_RECONCILE,
                MOD_RECONCILE_INTERVAL_MS,
                None,
            );
        }
        state.reconcile_timer_armed = true;
    } else if !want_timer && state.reconcile_timer_armed {
        unsafe {
            let _ = KillTimer(Some(state.kbd_hwnd), TIMER_MOD_RECONCILE);
        }
        state.reconcile_timer_armed = false;
    }
}

fn publish_physical_mods(state: &KeyboardHookState) {
    if let Some(ref sync) = state.sync {
        sync.physical_mods
            .store(state.modifiers.bits(), Ordering::Release);
    }
}

fn seed_modifiers_from_os(state: &mut KeyboardHookState) {
    let ignore_lctrl = state
        .sync
        .as_ref()
        .is_some_and(|s| s.in_synthetic_ctrl_window(monotonic_ms()));
    state
        .modifiers
        .seed_from_async(capture_async_mod_bits(), ignore_lctrl);
    publish_physical_mods(state);
    update_reconcile_timer(state);
}

fn sync_keyboard_hook(state: &mut KeyboardHookState) -> Result<()> {
    let needed = state.hold.is_some() || state.win_arrow_patch.enabled;
    if needed && state.hook.is_none() {
        let hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_proc), None, 0) }
                .context("SetWindowsHookExW(WH_KEYBOARD_LL) failed")?;
        state.hook = Some(hook);
        if let Some(ref sync) = state.sync {
            sync.last_hook_event_ms
                .store(monotonic_ms(), Ordering::Relaxed);
        }
        seed_modifiers_from_os(state);
    } else if !needed {
        if let Some(hook) = state.hook.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        state.win_arrow_patch.reset_swallowed();
        state.modifiers.clear();
        publish_physical_mods(state);
        update_reconcile_timer(state);
    }
    Ok(())
}

fn reinstall_keyboard_hook(state: &mut KeyboardHookState, reset_modifiers: bool) -> Result<()> {
    if let Some(hook) = state.hook.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
    }
    state.win_arrow_patch.reset_swallowed();
    if reset_modifiers {
        state.modifiers.clear();
        publish_physical_mods(state);
    }
    let needed = state.hold.is_some() || state.win_arrow_patch.enabled;
    if needed {
        let hook =
            match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_proc), None, 0) } {
                Ok(h) => h,
                Err(e) => {
                    update_reconcile_timer(state);
                    return Err(anyhow!("SetWindowsHookExW(WH_KEYBOARD_LL) failed: {e}"));
                }
            };
        state.hook = Some(hook);
        if let Some(ref sync) = state.sync {
            sync.last_hook_event_ms
                .store(monotonic_ms(), Ordering::Relaxed);
        }
        seed_modifiers_from_os(state);
    } else {
        update_reconcile_timer(state);
    }
    Ok(())
}

fn init_keyboard_hook_context(
    kbd_hwnd: HWND,
    shell_hwnd: HWND,
    sync: Arc<SharedHookSync>,
    win_arrow_patch_enabled: bool,
) -> Result<()> {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.kbd_hwnd = kbd_hwnd;
        state.shell_hwnd = shell_hwnd;
        state.sync = Some(sync.clone());
        state.hold = None;
        state.modifiers.clear();
        state.win_arrow_patch.set_enabled(win_arrow_patch_enabled);
        let res = sync_keyboard_hook(&mut state);
        if res.is_err() {
            state.win_arrow_patch.set_enabled(false);
            sync.patch_enabled.store(false, Ordering::Release);
        } else {
            sync.patch_enabled
                .store(win_arrow_patch_enabled, Ordering::Release);
        }
        res
    })
}

fn set_win_arrow_patch_hook(enabled: bool) -> Result<()> {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        let prev = state.win_arrow_patch.enabled;
        state.win_arrow_patch.set_enabled(enabled);
        if let Err(e) = sync_keyboard_hook(&mut state) {
            state.win_arrow_patch.set_enabled(prev);
            let _ = sync_keyboard_hook(&mut state);
            if let Some(ref sync) = state.sync {
                sync.patch_enabled.store(prev, Ordering::Release);
            }
            return Err(anyhow!("安装键盘钩子失败（{e}）"));
        }
        if let Some(ref sync) = state.sync {
            sync.patch_enabled.store(enabled, Ordering::Release);
        }
        if !state.shell_hwnd.is_invalid() {
            unsafe {
                let _ = PostMessageW(
                    Some(state.shell_hwnd),
                    WM_SYNC_WATCHDOG_TIMER,
                    WPARAM(enabled as usize),
                    LPARAM(0),
                );
            }
        }
        Ok(())
    })
}

fn arm_hold_hook_local(primary: PrimaryModifier) -> bool {
    if !primary.is_physically_held() {
        disarm_hold_hook_local();
        return false;
    }
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.hold = Some(HoldHookContext {
            primary,
            navigated: false,
        });
        if sync_keyboard_hook(&mut state).is_err() {
            state.hold = None;
            return false;
        }
        true
    })
}

fn disarm_hold_hook_local() {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.hold = None;
        let _ = sync_keyboard_hook(&mut state);
    });
}

fn reset_hook_modifiers_local() {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.modifiers.clear();
        state.win_arrow_patch.reset_swallowed();
        publish_physical_mods(&state);
        update_reconcile_timer(&mut state);
    });
}

fn refresh_keyboard_hook_local(force: bool) {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        let needed = state.hold.is_some() || state.win_arrow_patch.enabled;
        if !needed {
            return;
        }
        if !force {
            if state.hold.is_some()
                || state.modifiers.any_held()
                || state.win_arrow_patch.any_swallowed()
            {
                return;
            }
            if let Some(ref sync) = state.sync {
                let last = sync.last_hook_event_ms.load(Ordering::Relaxed);
                let now = monotonic_ms();
                if state.hook.is_some() && now.saturating_sub(last) < WATCHDOG_STALE_ACTIVITY_MS {
                    return;
                }
            }
        }
        let _ = reinstall_keyboard_hook(&mut state, force);
    });
}

fn reconcile_hook_modifiers_local() {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        if !state.modifiers.any_held() {
            update_reconcile_timer(&mut state);
            return;
        }
        state
            .modifiers
            .reconcile_with_async(capture_async_mod_bits(), monotonic_ms());
        publish_physical_mods(&state);
        update_reconcile_timer(&mut state);
    });
}

fn teardown_keyboard_hook_local() {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.hold = None;
        state.win_arrow_patch.set_enabled(false);
        state.modifiers.clear();
        if state.reconcile_timer_armed && !state.kbd_hwnd.is_invalid() {
            unsafe {
                let _ = KillTimer(Some(state.kbd_hwnd), TIMER_MOD_RECONCILE);
            }
            state.reconcile_timer_armed = false;
        }
        if let Some(hook) = state.hook.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        if let Some(ref sync) = state.sync {
            sync.physical_mods.store(0, Ordering::Release);
            sync.patch_enabled.store(false, Ordering::Release);
        }
        state.sync = None;
        state.kbd_hwnd = HWND::default();
        state.shell_hwnd = HWND::default();
    });
}

unsafe extern "system" fn kbd_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_KBD_SET_PATCH => {
            let req_ptr = lparam.0 as *mut WinArrowPatchRequest;
            if !req_ptr.is_null() {
                let req = unsafe { Box::from_raw(req_ptr) };
                let res = set_win_arrow_patch_hook(req.enabled);
                let _ = req.reply.send(res);
            }
            return LRESULT(0);
        }
        WM_KBD_ARM_HOLD => {
            let req_ptr = lparam.0 as *mut ArmHoldRequest;
            if !req_ptr.is_null() {
                let req = unsafe { Box::from_raw(req_ptr) };
                let armed = arm_hold_hook_local(req.primary);
                let _ = req.reply.send(armed);
            }
            return LRESULT(0);
        }
        WM_KBD_DISARM_HOLD => {
            disarm_hold_hook_local();
            return LRESULT(0);
        }
        WM_KBD_RESET_MODS => {
            reset_hook_modifiers_local();
            return LRESULT(0);
        }
        WM_KBD_REFRESH_HOOK => {
            let force = wparam.0 != 0;
            refresh_keyboard_hook_local(force);
            return LRESULT(0);
        }
        WM_TIMER if wparam.0 == TIMER_MOD_RECONCILE => {
            reconcile_hook_modifiers_local();
            return LRESULT(0);
        }
        WM_CLOSE => {
            teardown_keyboard_hook_local();
            let _ = unsafe { DestroyWindow(hwnd) };
            return LRESULT(0);
        }
        WM_DESTROY => {
            teardown_keyboard_hook_local();
            unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            teardown_keyboard_hook_local();
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Dedicated, high-priority, zero-blocking thread solely responsible for pumping
/// `WH_KEYBOARD_LL` so tray RPCs, context menus, `SendInput`, or Explorer stalls can
/// never push `keyboard_ll_proc` past Windows' `LowLevelHooksTimeout`.
struct KbdHookWorker {
    kbd_hwnd_raw: isize,
    thread: Option<thread::JoinHandle<()>>,
}

struct KbdHookStartup {
    kbd_hwnd_raw: isize,
    patch_error: Option<String>,
}

impl KbdHookWorker {
    fn start(
        shell_hwnd: HWND,
        sync: Arc<SharedHookSync>,
        win_arrow_patch_enabled: bool,
    ) -> Result<(Self, Option<String>)> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<KbdHookStartup>>();
        let shell_hwnd_raw = shell_hwnd.0 as isize;

        let thread = thread::Builder::new()
            .name("windeck-kbd-hook".to_string())
            .spawn(move || {
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                }
                let init = (|| -> Result<(HWND, KbdHookStartup)> {
                    let hinstance = unsafe { GetModuleHandleW(None) }?;
                    let class_name = w!("WinDeckKbdHookClass");
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(kbd_wnd_proc),
                        hInstance: hinstance.into(),
                        lpszClassName: class_name,
                        ..Default::default()
                    };
                    unsafe { RegisterClassW(&wc) };

                    let kbd_hwnd = unsafe {
                        CreateWindowExW(
                            WINDOW_EX_STYLE(0),
                            class_name,
                            w!("WinDeckKbdHook"),
                            WINDOW_STYLE(0),
                            0,
                            0,
                            0,
                            0,
                            Some(HWND_MESSAGE),
                            None,
                            Some(hinstance.into()),
                            None,
                        )
                    }
                    .context("CreateWindowExW for WinDeckKbdHook")?;

                    let shell_hwnd = HWND(shell_hwnd_raw as *mut c_void);
                    let patch_error =
                        init_keyboard_hook_context(kbd_hwnd, shell_hwnd, sync, win_arrow_patch_enabled)
                            .err()
                            .map(|e| format!("{e:#}"));

                    Ok((
                        kbd_hwnd,
                        KbdHookStartup {
                            kbd_hwnd_raw: kbd_hwnd.0 as isize,
                            patch_error,
                        },
                    ))
                })();

                match init {
                    Ok((_hwnd, startup)) => {
                        let _ = ready_tx.send(Ok(startup));
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                }

                let mut msg = MSG::default();
                unsafe {
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                teardown_keyboard_hook_local();
            })
            .context("spawn windeck-kbd-hook thread")?;

        let startup = ready_rx
            .recv()
            .context("wait for windeck-kbd-hook startup")??;
        let patch_error = startup.patch_error;

        Ok((
            Self {
                kbd_hwnd_raw: startup.kbd_hwnd_raw,
                thread: Some(thread),
            },
            patch_error,
        ))
    }

    fn kbd_hwnd(&self) -> Option<HWND> {
        (self.kbd_hwnd_raw != 0).then_some(HWND(self.kbd_hwnd_raw as *mut c_void))
    }

    fn forward_patch_request(&self, req_ptr: *mut WinArrowPatchRequest) {
        let Some(hwnd) = self.kbd_hwnd() else {
            let req = unsafe { Box::from_raw(req_ptr) };
            let _ = req.reply.send(Err(anyhow!("键盘钩子线程未运行")));
            return;
        };
        if let Err(e) = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_KBD_SET_PATCH,
                WPARAM(0),
                LPARAM(req_ptr as isize),
            )
        } {
            let req = unsafe { Box::from_raw(req_ptr) };
            let _ = req
                .reply
                .send(Err(anyhow!("PostMessageW(WM_KBD_SET_PATCH) failed: {e}")));
        }
    }

    fn arm_hold(&self, primary: PrimaryModifier) -> bool {
        if !primary.is_physically_held() {
            self.disarm_hold();
            return false;
        }
        let Some(hwnd) = self.kbd_hwnd() else {
            return false;
        };
        let (tx, rx) = mpsc::channel();
        let req_ptr = Box::into_raw(Box::new(ArmHoldRequest { primary, reply: tx }));
        if unsafe {
            PostMessageW(
                Some(hwnd),
                WM_KBD_ARM_HOLD,
                WPARAM(0),
                LPARAM(req_ptr as isize),
            )
        }
        .is_err()
        {
            unsafe {
                drop(Box::from_raw(req_ptr));
            }
            return false;
        }
        rx.recv_timeout(Duration::from_millis(200)).unwrap_or(false)
    }

    fn disarm_hold(&self) {
        if let Some(hwnd) = self.kbd_hwnd() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_KBD_DISARM_HOLD, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn reset_modifiers(&self) {
        if let Some(hwnd) = self.kbd_hwnd() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_KBD_RESET_MODS, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn refresh_hook(&self, force: bool) {
        if let Some(hwnd) = self.kbd_hwnd() {
            unsafe {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_KBD_REFRESH_HOOK,
                    WPARAM(force as usize),
                    LPARAM(0),
                );
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(hwnd) = self.kbd_hwnd() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            self.kbd_hwnd_raw = 0;
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for KbdHookWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

unsafe extern "system" fn keyboard_ll_proc(
    n_code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if n_code < 0 {
        return unsafe { CallNextHookEx(None, n_code, wparam, lparam) };
    }

    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let is_injected = (kb.flags.0 & LLKHF_INJECTED.0) != 0;

    enum Action {
        Pass,
        Swallow,
    }

    let action = KEYBOARD_HOOK.with(|slot| {
        let Ok(mut state) = slot.try_borrow_mut() else {
            return Action::Pass;
        };

        let now_ms = monotonic_ms();
        if let Some(ref sync) = state.sync {
            sync.last_hook_event_ms.store(now_ms, Ordering::Relaxed);
        }

        // Never react to synthetic SendInput events (including our own Ctrl+Win+Arrow injection).
        if is_injected {
            return Action::Pass;
        }

        let msg = wparam.0 as u32;
        let vk = kb.vkCode;
        let is_keydown = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_keyup = msg == WM_KEYUP || msg == WM_SYSKEYUP;

        // Always update physical modifier tracking FIRST so hold-mode transitions and rapid
        // Win+Arrow taps share an exact FIFO view of real non-injected modifier keys.
        if state.modifiers.on_non_injected_key(vk, is_keydown, is_keyup) {
            publish_physical_mods(&state);
            update_reconcile_timer(&mut state);
        }

        let shell_hwnd = state.shell_hwnd;

        // Priority 1: Active hold-to-switch mode (takes precedence over Win+Arrow patch,
        // e.g. when the user configures `Win + Q` as the WinDeck launcher hotkey).
        if let Some(ctx) = state.hold.as_mut() {
            // 1a. Primary modifier physically released -> disarm hold & notify shell_hwnd
            if is_keyup && ctx.primary.matches_vk(vk) {
                if !ctx.primary.sibling_still_held(vk) {
                    let navigated = ctx.navigated;
                    state.hold = None;
                    let _ = sync_keyboard_hook(&mut state);
                    unsafe {
                        let _ = PostMessageW(
                            Some(shell_hwnd),
                            WM_HOLD_RELEASED,
                            WPARAM(navigated as usize),
                            LPARAM(0),
                        );
                    }
                }
                // Always pass modifier KEYUP through so Windows never sees a stuck modifier!
                return Action::Pass;
            }

            // 1b. Escape or Enter cancels hold-to-jump and passes through to GPUI immediately
            if is_keydown && (vk == VK_ESCAPE.0 as u32 || vk == VK_RETURN.0 as u32) {
                state.hold = None;
                let _ = sync_keyboard_hook(&mut state);
                unsafe {
                    let _ = PostMessageW(Some(shell_hwnd), WM_HOLD_RELEASED, WPARAM(0), LPARAM(0));
                }
                return Action::Pass;
            }

            // 1c. Arrow keys while primary modifier is held -> navigate WinDeck & swallow key
            //     (prevents Win+Arrow Aero Snap, Alt+Arrow system ding, and Ctrl+Arrow double-fire)
            let nav_dir = match vk {
                x if x == VK_LEFT.0 as u32 => Some(HoldNavDirection::Left),
                x if x == VK_RIGHT.0 as u32 => Some(HoldNavDirection::Right),
                x if x == VK_UP.0 as u32 => Some(HoldNavDirection::Up),
                x if x == VK_DOWN.0 as u32 => Some(HoldNavDirection::Down),
                _ => None,
            };

            if let Some(dir) = nav_dir {
                if !ctx.primary.is_physically_held() {
                    // Safety net: modifier was released without us seeing keyup
                    state.hold = None;
                    let _ = sync_keyboard_hook(&mut state);
                    unsafe {
                        let _ = PostMessageW(
                            Some(shell_hwnd),
                            WM_HOLD_RELEASED,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                    return Action::Pass;
                }
                if is_keydown {
                    ctx.navigated = true;
                    unsafe {
                        let _ = PostMessageW(
                            Some(shell_hwnd),
                            WM_HOLD_NAV,
                            WPARAM(dir.to_wparam()),
                            LPARAM(0),
                        );
                    }
                    return Action::Swallow;
                }
                if is_keyup && ctx.navigated {
                    return Action::Swallow;
                }
            }

            return Action::Pass;
        }

        // Priority 2: Global `Win + Left/Right` system patch.
        // Stage 1 unconditionally disables (swallows) built-in Win+Left/Right window snapping;
        // Stage 2 dispatches the native virtual desktop switch off the hook thread.
        if vk != VK_LEFT.0 as u32 && vk != VK_RIGHT.0 as u32 {
            return Action::Pass;
        }

        let (overlay_foreground, ignore_lctrl_async) = match state.sync.as_ref() {
            Some(sync) => (
                sync.overlay.is_overlay_foreground(),
                sync.in_synthetic_ctrl_window(now_ms),
            ),
            None => (false, false),
        };

        let mods = if is_keydown && state.win_arrow_patch.enabled {
            let snap = state.modifiers.snapshot_for_arrow(
                capture_async_mod_bits(),
                now_ms,
                ignore_lctrl_async,
            );
            publish_physical_mods(&state);
            update_reconcile_timer(&mut state);
            snap
        } else {
            ModifierSnapshot::from_bits(0)
        };

        match state
            .win_arrow_patch
            .handle_key(vk, is_keydown, is_keyup, overlay_foreground, mods)
        {
            WinArrowDecision::Pass => Action::Pass,
            WinArrowDecision::SwallowOnly => Action::Swallow,
            WinArrowDecision::SwallowAndMask => {
                unsafe {
                    let _ = PostMessageW(
                        Some(shell_hwnd),
                        WM_WIN_ARROW_MASK,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
                Action::Swallow
            }
            WinArrowDecision::SwallowAndSwitch(dir) => {
                unsafe {
                    let _ = PostMessageW(
                        Some(shell_hwnd),
                        WM_WIN_ARROW_SWITCH,
                        WPARAM(dir.to_wparam()),
                        LPARAM(0),
                    );
                }
                Action::Swallow
            }
        }
    });

    match action {
        Action::Swallow => LRESULT(1),
        Action::Pass => unsafe { CallNextHookEx(None, n_code, wparam, lparam) },
    }
}

fn create_programmatic_tray_icon() -> Result<HICON> {
    const W: i32 = 32;
    const H: i32 = 32;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: W,
            biHeight: -H,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits_ptr: *mut c_void = std::ptr::null_mut();
    let color_bmp = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0) }
        .context("CreateDIBSection for tray icon")?;

    if !bits_ptr.is_null() {
        let pixels = unsafe { std::slice::from_raw_parts_mut(bits_ptr as *mut u32, (W * H) as usize) };
        for y in 0..H {
            for x in 0..W {
                // Rounded dark slate tile with 2x2 glowing blue/cyan window cards
                let in_tile = x >= 2 && x < 30 && y >= 2 && y < 30;
                let corner = (x < 5 || x >= 27) && (y < 5 || y >= 27);
                let px = if !in_tile || corner {
                    0x00000000
                } else {
                    let c1 = (6..=14).contains(&x) && (6..=14).contains(&y);
                    let c2 = (17..=25).contains(&x) && (6..=14).contains(&y);
                    let c3 = (6..=14).contains(&x) && (17..=25).contains(&y);
                    let c4 = (17..=25).contains(&x) && (17..=25).contains(&y);
                    if c1 {
                        0xFF3B82F6 // bright blue (active card)
                    } else if c2 || c3 || c4 {
                        0xFF94A3B8 // light slate cards
                    } else {
                        0xFF151C28 // dark slate base
                    }
                };
                pixels[(y * W + x) as usize] = px;
            }
        }
    }

    let mask_bmp = unsafe { CreateBitmap(W, H, 1, 1, None) };
    let icon_info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask_bmp,
        hbmColor: color_bmp,
    };
    let icon = unsafe { CreateIconIndirect(&icon_info) }.context("CreateIconIndirect")?;
    unsafe {
        let _ = DeleteObject(color_bmp.into());
        let _ = DeleteObject(mask_bmp.into());
    }
    Ok(icon)
}

fn load_embedded_or_fallback_tray_icon() -> Result<HICON> {
    if let Ok(hinstance) = unsafe { GetModuleHandleW(None) } {
        // Resource ID 1 = IDI_APPICON embedded via windeck.rc
        if let Ok(handle) = unsafe {
            LoadImageW(
                Some(hinstance.into()),
                PCWSTR(1usize as *const u16),
                IMAGE_ICON,
                32,
                32,
                LR_DEFAULTCOLOR,
            )
        } {
            if !handle.is_invalid() {
                return Ok(HICON(handle.0));
            }
        }
    }
    create_programmatic_tray_icon()
}

fn add_tray_icon(hwnd: HWND, icon: HICON, tip: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_ICON,
        hIcon: icon,
        ..Default::default()
    };
    let utf16: Vec<u16> = tip.encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..utf16.len()].copy_from_slice(&utf16);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn update_tray_tooltip(hwnd: HWND, icon: HICON, tip: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_TIP,
        hIcon: icon,
        ..Default::default()
    };
    let utf16: Vec<u16> = tip.encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..utf16.len()].copy_from_slice(&utf16);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn show_tray_menu(hwnd: HWND, hotkey_label: &str) -> Option<ShellEvent> {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return None;
        };
        let toggle_label = HSTRING::from(format!("显示窗口切换器 ({hotkey_label})"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_CMD_TOGGLE as usize,
            PCWSTR(toggle_label.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_CMD_SETTINGS as usize,
            w!("设置..."),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_CMD_CHECK_UPDATE as usize,
            w!("检查更新..."),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_CMD_RELOAD as usize,
            w!("重新加载配置"),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_CMD_OPEN_CONFIG as usize,
            w!("打开配置文件目录"),
        );
        let autostart_on = is_autostart_enabled();
        let autostart_flags = if autostart_on {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING | MF_UNCHECKED
        };
        let _ = AppendMenuW(
            menu,
            autostart_flags,
            MENU_CMD_AUTOSTART as usize,
            w!("开机自启动"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_CMD_QUIT as usize,
            w!("退出 WinDeck"),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);

        match cmd.0 as u32 {
            MENU_CMD_TOGGLE => Some(ShellEvent::ToggleOverlay {
                hotkey_triggered: false,
                hold_armed: false,
            }),
            MENU_CMD_SETTINGS => {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                Some(ShellEvent::OpenSettings)
            }
            MENU_CMD_CHECK_UPDATE => {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                Some(ShellEvent::CheckUpdate)
            }
            MENU_CMD_RELOAD => Some(ShellEvent::ReloadConfig),
            MENU_CMD_OPEN_CONFIG => Some(ShellEvent::OpenConfigDir),
            MENU_CMD_AUTOSTART => {
                let target = !autostart_on;
                set_autostart_enabled(target)
                    .ok()
                    .map(|()| ShellEvent::AutostartToggled(target))
            }
            MENU_CMD_QUIT => Some(ShellEvent::Quit),
            _ => None,
        }
    }
}

fn sync_watchdog_timer(hwnd: HWND, state: &mut ShellHostState, enabled: bool) {
    if enabled && !state.watchdog_armed {
        unsafe {
            let _ = SetTimer(Some(hwnd), TIMER_HOOK_WATCHDOG, WATCHDOG_INTERVAL_MS, None);
        }
        state.watchdog_armed = true;
    } else if !enabled && state.watchdog_armed {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_HOOK_WATCHDOG);
        }
        state.watchdog_armed = false;
    }
}

/// Hidden top-level (`WS_POPUP`) window that receives OS-wide broadcasts (`TaskbarCreated`,
/// `WM_POWERBROADCAST`, `WM_WTSSESSION_CHANGE`) which Win32 never delivers to `HWND_MESSAGE`.
unsafe extern "system" fn broadcast_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
        if !cs.is_null() {
            let ptr = unsafe { (*cs).lpCreateParams } as isize;
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr) };
        }
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut BroadcastWindowState;
    if !state_ptr.is_null() {
        let state = unsafe { &mut *state_ptr };
        let shell_hwnd = state.shell_hwnd;

        if msg == state.taskbar_created_msg && state.taskbar_created_msg != 0 {
            unsafe {
                let _ = PostMessageW(Some(shell_hwnd), WM_EXPLORER_RESTARTED, WPARAM(0), LPARAM(0));
            }
            return LRESULT(0);
        }

        match msg {
            WM_POWERBROADCAST => {
                if wparam.0 == PBT_APMRESUMEAUTOMATIC_CODE || wparam.0 == PBT_APMRESUMESUSPEND_CODE
                {
                    unsafe {
                        let _ = PostMessageW(
                            Some(shell_hwnd),
                            WM_SYSTEM_RESUMED,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
                return LRESULT(1);
            }
            WM_WTSSESSION_CHANGE => {
                match wparam.0 as u32 {
                    WTS_SESSION_LOCK | WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT => unsafe {
                        let _ = PostMessageW(
                            Some(shell_hwnd),
                            WM_SESSION_LOCKED,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    },
                    WTS_SESSION_UNLOCK | WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT => unsafe {
                        let _ = PostMessageW(
                            Some(shell_hwnd),
                            WM_SESSION_UNLOCKED,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    },
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                if state.wts_registered {
                    unsafe {
                        let _ = WTSUnRegisterSessionNotification(hwnd);
                    }
                    state.wts_registered = false;
                }
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                unsafe {
                    drop(Box::from_raw(state_ptr));
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn create_broadcast_window(
    hinstance: windows::Win32::Foundation::HMODULE,
    shell_hwnd: HWND,
    taskbar_created_msg: u32,
) -> Option<HWND> {
    let class_name = w!("WinDeckShellBroadcastClass");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(broadcast_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };

    let state_ptr = Box::into_raw(Box::new(BroadcastWindowState {
        shell_hwnd,
        taskbar_created_msg,
        wts_registered: false,
    }));

    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            w!("WinDeckShellBroadcast"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            Some(state_ptr as *const c_void),
        )
    } {
        Ok(h) => h,
        Err(_) => {
            unsafe {
                drop(Box::from_raw(state_ptr));
            }
            return None;
        }
    };

    if unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }.is_ok() {
        unsafe { (*state_ptr).wts_registered = true };
    }
    Some(hwnd)
}

unsafe extern "system" fn shell_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
        if !cs.is_null() {
            let ptr = unsafe { (*cs).lpCreateParams } as isize;
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr) };
        }
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ShellHostState;
    if !state_ptr.is_null() {
        // Handle tray context menu BEFORE taking `&mut ShellHostState`, because `TrackPopupMenu`
        // runs a nested modal message loop that can re-enter `shell_wnd_proc`.
        if msg == WM_TRAY_ICON && ((lparam.0 & 0xFFFF) as u32) == WM_RBUTTONUP {
            let hotkey_label = unsafe { (*state_ptr).hotkey_label.clone() };
            if let Some(ev) = show_tray_menu(hwnd, &hotkey_label) {
                let live_ptr =
                    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ShellHostState;
                if !live_ptr.is_null() {
                    unsafe { ((*live_ptr).on_event)(ev) };
                }
            }
            return LRESULT(0);
        }

        let state = unsafe { &mut *state_ptr };

        if msg == state.taskbar_created_msg && state.taskbar_created_msg != 0 {
            add_tray_icon(hwnd, state.tray_icon, &state.tooltip);
            state.kbd_worker.refresh_hook(true);
            (state.on_event)(ShellEvent::ExplorerRestarted);
            return LRESULT(0);
        }

        match msg {
            WM_WIN_ARROW_SWITCH => {
                if let Some(dir) = DesktopSwitchDir::from_wparam(wparam.0)
                    && !send_native_desktop_switch(dir, &state.hook_sync)
                {
                    (state.on_event)(ShellEvent::WinArrowComFallback(dir));
                }
                return LRESULT(0);
            }
            WM_WIN_ARROW_MASK => {
                send_menu_mask_key();
                return LRESULT(0);
            }
            WM_SET_WIN_ARROW_PATCH => {
                let req_ptr = lparam.0 as *mut WinArrowPatchRequest;
                if !req_ptr.is_null() {
                    state.kbd_worker.forward_patch_request(req_ptr);
                }
                return LRESULT(0);
            }
            WM_SYNC_WATCHDOG_TIMER => {
                sync_watchdog_timer(hwnd, state, wparam.0 != 0);
                return LRESULT(0);
            }
            WM_EXPLORER_RESTARTED => {
                add_tray_icon(hwnd, state.tray_icon, &state.tooltip);
                state.kbd_worker.refresh_hook(true);
                (state.on_event)(ShellEvent::ExplorerRestarted);
                return LRESULT(0);
            }
            WM_SYSTEM_RESUMED | WM_SESSION_UNLOCKED => {
                state.kbd_worker.refresh_hook(true);
                return LRESULT(0);
            }
            WM_SESSION_LOCKED => {
                state.kbd_worker.reset_modifiers();
                return LRESULT(0);
            }
            WM_TIMER if wparam.0 == TIMER_HOOK_WATCHDOG => {
                if state.hook_sync.patch_enabled.load(Ordering::Acquire)
                    && state.hook_sync.physical_mods.load(Ordering::Acquire) == 0
                    && user_idle_ms().is_some_and(|idle| idle >= WATCHDOG_MIN_IDLE_MS)
                {
                    state.kbd_worker.refresh_hook(false);
                }
                return LRESULT(0);
            }
            WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
                // Propagate WM_HOTKEY's foreground activation right to GPUI's UI thread.
                let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                let opening = !state.hook_sync.overlay.is_visible();
                let hold_armed = if opening {
                    state.kbd_worker.arm_hold(state.hotkey.primary)
                } else {
                    state.kbd_worker.disarm_hold();
                    false
                };
                (state.on_event)(ShellEvent::ToggleOverlay {
                    hotkey_triggered: true,
                    hold_armed,
                });
                return LRESULT(0);
            }
            WM_HOLD_NAV => {
                if let Some(dir) = HoldNavDirection::from_wparam(wparam.0) {
                    (state.on_event)(ShellEvent::HoldNavigate(dir));
                }
                return LRESULT(0);
            }
            WM_HOLD_RELEASED => {
                let commit = wparam.0 != 0;
                if commit {
                    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                }
                (state.on_event)(ShellEvent::HoldModifierReleased { commit });
                return LRESULT(0);
            }
            WM_DISARM_HOLD_HOOK => {
                state.kbd_worker.disarm_hold();
                return LRESULT(0);
            }
            WM_TRAY_ICON => {
                let mouse_msg = (lparam.0 & 0xFFFF) as u32;
                if mouse_msg == WM_LBUTTONUP {
                    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                    state.kbd_worker.disarm_hold();
                    (state.on_event)(ShellEvent::ToggleOverlay {
                        hotkey_triggered: false,
                        hold_armed: false,
                    });
                } else if mouse_msg == NIN_BALLOONUSERCLICK {
                    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                    state.kbd_worker.disarm_hold();
                    (state.on_event)(ShellEvent::OpenUpdateDialog);
                }
                return LRESULT(0);
            }
            WM_SHOW_BALLOON => {
                let req_ptr = lparam.0 as *mut BalloonRequest;
                if !req_ptr.is_null() {
                    let req = unsafe { Box::from_raw(req_ptr) };
                    let mut nid = NOTIFYICONDATAW {
                        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                        hWnd: hwnd,
                        uID: 1,
                        uFlags: NIF_INFO,
                        dwInfoFlags: NIIF_INFO,
                        ..Default::default()
                    };
                    let title_u16: Vec<u16> = req
                        .title
                        .encode_utf16()
                        .take(nid.szInfoTitle.len() - 1)
                        .collect();
                    nid.szInfoTitle[..title_u16.len()].copy_from_slice(&title_u16);
                    let body_u16: Vec<u16> = req
                        .body
                        .encode_utf16()
                        .take(nid.szInfo.len() - 1)
                        .collect();
                    nid.szInfo[..body_u16.len()].copy_from_slice(&body_u16);
                    unsafe {
                        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                    }
                }
                return LRESULT(0);
            }
            WM_REBIND_HOTKEY => {
                let req_ptr = lparam.0 as *mut RebindRequest;
                if req_ptr.is_null() {
                    return LRESULT(0);
                }
                let req = unsafe { Box::from_raw(req_ptr) };
                state.kbd_worker.disarm_hold();

                if req.hotkey == state.hotkey && state.hotkey_registered {
                    let _ = req.reply.send(Ok(()));
                    return LRESULT(0);
                }

                let prev_hotkey = state.hotkey;
                let was_registered = state.hotkey_registered;
                if was_registered {
                    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID) };
                    state.hotkey_registered = false;
                }

                match unsafe {
                    RegisterHotKey(
                        Some(hwnd),
                        HOTKEY_ID,
                        HOT_KEY_MODIFIERS(req.hotkey.modifiers),
                        req.hotkey.vk,
                    )
                } {
                    Ok(()) => {
                        state.hotkey_registered = true;
                        state.hotkey = req.hotkey;
                        state.hotkey_label = req.hotkey.canonical_label();
                        state.tooltip =
                            format!("WinDeck — 全局窗口切换器 ({})", state.hotkey_label);
                        update_tray_tooltip(hwnd, state.tray_icon, &state.tooltip);
                        let _ = req.reply.send(Ok(()));
                    }
                    Err(e) => {
                        // Roll back to previous working hotkey so user is never left without a shortcut
                        if was_registered
                            && unsafe {
                                RegisterHotKey(
                                    Some(hwnd),
                                    HOTKEY_ID,
                                    HOT_KEY_MODIFIERS(prev_hotkey.modifiers),
                                    prev_hotkey.vk,
                                )
                            }
                            .is_ok()
                        {
                            state.hotkey_registered = true;
                        }
                        let _ = req.reply.send(Err(anyhow!(
                            "热键 {} 已被系统或其他程序占用（{e}）",
                            req.hotkey.canonical_label()
                        )));
                    }
                }
                return LRESULT(0);
            }
            WM_ACTIVATE_EXISTING => {
                let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                state.kbd_worker.disarm_hold();
                (state.on_event)(ShellEvent::ShowOverlay);
                return LRESULT(0);
            }
            WM_CLOSE => {
                sync_watchdog_timer(hwnd, state, false);
                state.kbd_worker.shutdown();
                if !state.broadcast_hwnd.is_invalid() {
                    let _ = unsafe { DestroyWindow(state.broadcast_hwnd) };
                    state.broadcast_hwnd = HWND::default();
                }
                if state.hotkey_registered {
                    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID) };
                    state.hotkey_registered = false;
                }
                remove_tray_icon(hwnd);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            WM_DESTROY => {
                sync_watchdog_timer(hwnd, state, false);
                state.kbd_worker.shutdown();
                if !state.broadcast_hwnd.is_invalid() {
                    let _ = unsafe { DestroyWindow(state.broadcast_hwnd) };
                    state.broadcast_hwnd = HWND::default();
                }
                unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                let mut boxed = unsafe { Box::from_raw(state_ptr) };
                boxed.kbd_worker.shutdown();
                if !boxed.broadcast_hwnd.is_invalid() {
                    let _ = unsafe { DestroyWindow(boxed.broadcast_hwnd) };
                    boxed.broadcast_hwnd = HWND::default();
                }
                if !boxed.tray_icon.is_invalid() {
                    let _ = unsafe { DestroyIcon(boxed.tray_icon) };
                }
                drop(boxed);
                return LRESULT(0);
            }
            _ => {}
        }
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Manages global hotkey + Windows system tray icon on a dedicated zero-CPU message thread,
/// paired with a separate high-priority `windeck-kbd-hook` thread for low-level keyboard interception.
pub struct ShellHost {
    hwnd_raw: isize,
    hotkey_registered: bool,
    hotkey_error: Option<String>,
    win_arrow_patch_error: Option<String>,
    overlay: Arc<OverlayFocusState>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ShellHost {
    pub fn start<F>(
        hotkey: HotkeySpec,
        hotkey_label: String,
        win_arrow_switch_desktop: bool,
        on_event: F,
    ) -> Result<Self>
    where
        F: Fn(ShellEvent) + Send + 'static,
    {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<ShellHostStartup>>();
        let overlay = Arc::new(OverlayFocusState::new());
        let hook_sync = Arc::new(SharedHookSync::new(overlay.clone(), win_arrow_switch_desktop));

        let thread = thread::Builder::new()
            .name("windeck-shell-host".to_string())
            .spawn(move || {
                let init = (|| -> Result<(HWND, ShellHostStartup)> {
                    let hinstance = unsafe { GetModuleHandleW(None) }?;
                    let class_name = w!("WinDeckShellHostClass");
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(shell_wnd_proc),
                        hInstance: hinstance.into(),
                        lpszClassName: class_name,
                        ..Default::default()
                    };
                    unsafe { RegisterClassW(&wc) };

                    let tray_icon = load_embedded_or_fallback_tray_icon()?;
                    let taskbar_created_msg =
                        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
                    let tooltip = format!("WinDeck — 全局窗口切换器 ({hotkey_label})");

                    let hwnd = match unsafe {
                        CreateWindowExW(
                            WINDOW_EX_STYLE(0),
                            class_name,
                            w!("WinDeckShellHost"),
                            WINDOW_STYLE(0),
                            0,
                            0,
                            0,
                            0,
                            Some(HWND_MESSAGE),
                            None,
                            Some(hinstance.into()),
                            None,
                        )
                    } {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = unsafe { DestroyIcon(tray_icon) };
                            return Err(e.into());
                        }
                    };

                    let (kbd_worker, patch_err) = match KbdHookWorker::start(
                        hwnd,
                        hook_sync.clone(),
                        win_arrow_switch_desktop,
                    ) {
                        Ok(pair) => pair,
                        Err(e) => {
                            let _ = unsafe { DestroyWindow(hwnd) };
                            let _ = unsafe { DestroyIcon(tray_icon) };
                            return Err(e);
                        }
                    };

                    let broadcast_hwnd =
                        create_broadcast_window(hinstance, hwnd, taskbar_created_msg)
                            .unwrap_or_default();

                    let state = Box::new(ShellHostState {
                        on_event: Box::new(on_event),
                        tray_icon,
                        tooltip: tooltip.clone(),
                        hotkey,
                        hotkey_label,
                        taskbar_created_msg,
                        hotkey_registered: false,
                        broadcast_hwnd,
                        watchdog_armed: false,
                        hook_sync: hook_sync.clone(),
                        kbd_worker,
                    });
                    let state_ptr = Box::into_raw(state);
                    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };

                    add_tray_icon(hwnd, tray_icon, &tooltip);

                    let (reg_ok, reg_err) = match unsafe {
                        RegisterHotKey(
                            Some(hwnd),
                            HOTKEY_ID,
                            HOT_KEY_MODIFIERS(hotkey.modifiers),
                            hotkey.vk,
                        )
                    } {
                        Ok(()) => {
                            unsafe { (*state_ptr).hotkey_registered = true };
                            (true, None)
                        }
                        Err(e) => (false, Some(format!("RegisterHotKey failed: {e}"))),
                    };

                    if win_arrow_switch_desktop && patch_err.is_none() {
                        unsafe {
                            sync_watchdog_timer(hwnd, &mut *state_ptr, true);
                        }
                    }

                    Ok((
                        hwnd,
                        ShellHostStartup {
                            hwnd_raw: hwnd.0 as isize,
                            hotkey_registered: reg_ok,
                            hotkey_error: reg_err,
                            win_arrow_patch_error: patch_err,
                        },
                    ))
                })();

                let hwnd = match init {
                    Ok((hwnd, startup)) => {
                        let _ = ready_tx.send(Ok(startup));
                        hwnd
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                let _ = hwnd;
                let mut msg = MSG::default();
                unsafe {
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })
            .context("spawn windeck-shell-host thread")?;

        let startup = ready_rx.recv().context("wait for shell host startup")??;

        Ok(Self {
            hwnd_raw: startup.hwnd_raw,
            hotkey_registered: startup.hotkey_registered,
            hotkey_error: startup.hotkey_error,
            win_arrow_patch_error: startup.win_arrow_patch_error,
            overlay,
            thread: Some(thread),
        })
    }

    pub fn hotkey_registered(&self) -> bool {
        self.hotkey_registered
    }

    pub fn hotkey_error(&self) -> Option<&str> {
        self.hotkey_error.as_deref()
    }

    pub fn win_arrow_patch_error(&self) -> Option<&str> {
        self.win_arrow_patch_error.as_deref()
    }

    /// Registers WinDeck's two overlay HWNDs so the keyboard patch only treats the overlay
    /// as blocking when WinDeck itself is actively foreground (keeping `Win + ←/→` working
    /// when `hide_on_blur = false` leaves the panel open behind another app).
    pub fn register_overlay_hwnds(&self, gpui_hwnd: HWND, host_hwnd: HWND) {
        self.overlay.register_hwnds(gpui_hwnd, host_hwnd);
    }

    /// Keeps `ShellHost` aware of overlay visibility and guarantees any temporary
    /// hold-modifier keyboard hook is immediately removed when hidden.
    pub fn set_overlay_visible(&self, visible: bool) {
        self.overlay.set_visible(visible);
        if !visible {
            self.disarm_hold_mode();
        }
    }

    /// Cancels an active hold-modifier navigation hook (e.g. when switching to mouse interaction).
    pub fn disarm_hold_mode(&self) {
        if self.hwnd_raw != 0 {
            let hwnd = HWND(self.hwnd_raw as *mut c_void);
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_DISARM_HOLD_HOOK, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Enables or disables the system `Win + Left/Right -> switch virtual desktop` patch live
    /// on the dedicated `windeck-kbd-hook` thread, installing or uninstalling `WH_KEYBOARD_LL`.
    pub fn set_win_arrow_patch_enabled(&self, enabled: bool) -> Result<()> {
        if self.hwnd_raw == 0 {
            return Err(anyhow!("ShellHost is not running"));
        }
        let hwnd = HWND(self.hwnd_raw as *mut c_void);
        let (tx, rx) = mpsc::channel();
        let req_ptr = Box::into_raw(Box::new(WinArrowPatchRequest { enabled, reply: tx }));
        let posted = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_SET_WIN_ARROW_PATCH,
                WPARAM(0),
                LPARAM(req_ptr as isize),
            )
        };
        if let Err(e) = posted {
            unsafe {
                drop(Box::from_raw(req_ptr));
            }
            return Err(anyhow!("PostMessageW(WM_SET_WIN_ARROW_PATCH) failed: {e}"));
        }
        rx.recv_timeout(Duration::from_millis(800))
            .map_err(|_| anyhow!("系统补丁更新超时"))?
    }

    /// Shows a non-intrusive Windows tray notification balloon (e.g. when an update is found while hidden).
    pub fn show_balloon(&self, title: impl Into<String>, body: impl Into<String>) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut c_void);
        let req_ptr = Box::into_raw(Box::new(BalloonRequest {
            title: title.into(),
            body: body.into(),
        }));
        if unsafe {
            PostMessageW(
                Some(hwnd),
                WM_SHOW_BALLOON,
                WPARAM(0),
                LPARAM(req_ptr as isize),
            )
        }
        .is_err()
        {
            unsafe {
                drop(Box::from_raw(req_ptr));
            }
        }
    }

    /// Synchronously rebinds the global hotkey on the shell thread with automatic rollback
    /// to the previous working hotkey if the requested combo is occupied by Windows or another app.
    pub fn rebind_hotkey(&self, hotkey: HotkeySpec) -> Result<()> {
        if self.hwnd_raw == 0 {
            return Err(anyhow!("ShellHost is not running"));
        }
        let hwnd = HWND(self.hwnd_raw as *mut c_void);
        let (tx, rx) = mpsc::channel();
        let req_ptr = Box::into_raw(Box::new(RebindRequest { hotkey, reply: tx }));
        let posted = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_REBIND_HOTKEY,
                WPARAM(0),
                LPARAM(req_ptr as isize),
            )
        };
        if let Err(e) = posted {
            unsafe {
                drop(Box::from_raw(req_ptr));
            }
            return Err(anyhow!("PostMessageW(WM_REBIND_HOTKEY) failed: {e}"));
        }
        rx.recv_timeout(Duration::from_millis(800))
            .map_err(|_| anyhow!("热键更新超时"))?
    }
}

impl Drop for ShellHost {
    fn drop(&mut self) {
        if self.hwnd_raw != 0 {
            let hwnd = HWND(self.hwnd_raw as *mut c_void);
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            self.hwnd_raw = 0;
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_spec_parses_primary_modifier_plus_letter() {
        let alt_q = HotkeySpec::parse("Alt+Q").unwrap();
        assert_eq!(alt_q.primary, PrimaryModifier::Alt);
        assert_eq!(alt_q.vk, b'Q' as u32);
        assert_ne!(alt_q.modifiers & MOD_ALT.0, 0);
        assert_eq!(alt_q.canonical_label(), "Alt+Q");

        let ctrl_e = HotkeySpec::parse("ctrl + e").unwrap();
        assert_eq!(ctrl_e.primary, PrimaryModifier::Ctrl);
        assert_eq!(ctrl_e.vk, b'E' as u32);
        assert_ne!(ctrl_e.modifiers & MOD_CONTROL.0, 0);
        assert_eq!(ctrl_e.canonical_label(), "Ctrl+E");

        let win_a = HotkeySpec::parse("Win+a").unwrap();
        assert_eq!(win_a.primary, PrimaryModifier::Win);
        assert_eq!(win_a.vk, b'A' as u32);
        assert_ne!(win_a.modifiers & MOD_WIN.0, 0);
        assert_eq!(win_a.canonical_label(), "Win+A");

        assert!(HotkeySpec::parse("Q").is_err(), "missing modifier should fail");
        assert!(HotkeySpec::parse("Alt+").is_err(), "missing letter should fail");
        assert!(
            HotkeySpec::parse("Shift+Q").is_err(),
            "shift is not a valid primary modifier"
        );
        assert!(
            HotkeySpec::parse("Ctrl+Alt+Q").is_err(),
            "multiple modifiers should fail"
        );
        assert!(
            HotkeySpec::parse("Alt+Space").is_err(),
            "non-letter secondary key should fail"
        );
        assert!(
            HotkeySpec::parse("Ctrl+1").is_err(),
            "digit secondary key should fail"
        );
    }

    #[test]
    fn physical_modifier_tracker_handles_fast_tap_race_and_stale_reconciliation() {
        let mut tracker = PhysicalModifierTracker::default();

        // Fast-tap race: Win DOWN arrived in FIFO stream, then Left DOWN arrives while hardware
        // Win UP has already cleared GetAsyncKeyState (async_bits == 0) before Win UP hits our hook.
        assert!(tracker.on_non_injected_key(VK_LWIN.0 as u32, true, false));
        let fast_tap = tracker.snapshot_for_arrow(0, 1_000, false);
        assert!(
            fast_tap.is_pure_win_only(),
            "FIFO tracked Win must survive transient async mismatch so fast Win+Arrow never leaks to Snap"
        );

        // Normal Win UP clears tracked Win immediately.
        assert!(tracker.on_non_injected_key(VK_LWIN.0 as u32, false, true));
        assert!(!tracker.any_held());
        assert!(!tracker.snapshot_for_arrow(0, 1_005, false).win);

        // Missed Win UP (e.g. UAC / Win+L Secure Desktop): reconciles cleanly after STALE_MOD_RECONCILE_MS.
        assert!(tracker.on_non_injected_key(VK_LWIN.0 as u32, true, false));
        assert!(tracker.snapshot_for_arrow(0, 2_000, false).win);
        assert!(
            tracker
                .snapshot_for_arrow(0, 2_000 + STALE_MOD_RECONCILE_MS - 1, false)
                .win
        );
        let after_stale = tracker.snapshot_for_arrow(0, 2_000 + STALE_MOD_RECONCILE_MS, false);
        assert!(
            !after_stale.win,
            "stale Win after missed keyup must clear after reconciliation window"
        );
        assert!(!tracker.any_held());

        // Missed Win DOWN (e.g. Win pressed while elevated window had focus): async fallback picks it up.
        let missed_down = tracker.snapshot_for_arrow(PHYS_MOD_RWIN, 3_000, false);
        assert!(missed_down.is_pure_win_only());
    }

    #[test]
    fn physical_modifier_tracker_ignores_synthetic_lctrl_pollution_during_rapid_switches() {
        let mut tracker = PhysicalModifierTracker::default();
        tracker.on_non_injected_key(VK_LWIN.0 as u32, true, false);

        // While our own SendInput(LCtrl+Arrow) is active, GetAsyncKeyState reports VK_LCONTROL down,
        // but physical Ctrl was NOT pressed -> must stay pure Win so rapid taps never leak to Snap.
        let rapid = tracker.snapshot_for_arrow(PHYS_MOD_LWIN | PHYS_MOD_LCTRL, 5_000, true);
        assert!(
            rapid.is_pure_win_only(),
            "synthetic LCtrl must not pollute rapid follow-up Win+Arrow taps"
        );

        // Physical RCtrl is never injected by us, so async RCtrl is still respected immediately.
        let rctrl = tracker.snapshot_for_arrow(PHYS_MOD_LWIN | PHYS_MOD_RCTRL, 5_010, true);
        assert!(rctrl.ctrl && !rctrl.is_pure_win_only());

        // Real physical LCtrl keydown (non-injected) overrides masking so native Ctrl+Win+Arrow passes through.
        tracker.on_non_injected_key(VK_LCONTROL.0 as u32, true, false);
        let real_ctrl = tracker.snapshot_for_arrow(PHYS_MOD_LWIN | PHYS_MOD_LCTRL, 5_020, true);
        assert!(real_ctrl.ctrl && !real_ctrl.is_pure_win_only());
    }

    #[test]
    fn win_arrow_patch_disables_builtin_snap_in_all_states_including_foreground_overlay() {
        let pure_win = ModifierSnapshot {
            win: true,
            ctrl: false,
            alt: false,
            shift: false,
        };
        let none = ModifierSnapshot {
            win: false,
            ctrl: false,
            alt: false,
            shift: false,
        };
        let shift_win = ModifierSnapshot {
            win: true,
            ctrl: false,
            alt: false,
            shift: true,
        };
        let ctrl_win = ModifierSnapshot {
            win: true,
            ctrl: true,
            alt: false,
            shift: false,
        };
        let alt_win = ModifierSnapshot {
            win: true,
            ctrl: false,
            alt: true,
            shift: false,
        };

        let mut state = WinArrowPatchState::default();
        // Disabled by default -> passes through
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, false, pure_win),
            WinArrowDecision::Pass
        );

        state.set_enabled(true);

        // When overlay IS foreground: built-in Win+Arrow snap is STILL disabled (swallowed + masked),
        // while bare arrow keys pass through normally for WinDeck card navigation.
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, true, pure_win),
            WinArrowDecision::SwallowAndMask
        );
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, true, pure_win),
            WinArrowDecision::SwallowOnly
        );
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, false, true, true, none),
            WinArrowDecision::SwallowOnly
        );
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, true, none),
            WinArrowDecision::Pass
        );

        // Exclusive modifier checks: Shift+Win, Ctrl+Win, Alt+Win pass through untouched
        assert_eq!(
            state.handle_key(VK_LEFT.0 as u32, true, false, false, shift_win),
            WinArrowDecision::Pass
        );
        assert_eq!(
            state.handle_key(VK_LEFT.0 as u32, true, false, false, ctrl_win),
            WinArrowDecision::Pass
        );
        assert_eq!(
            state.handle_key(VK_LEFT.0 as u32, true, false, false, alt_win),
            WinArrowDecision::Pass
        );

        // Initial Win+Right triggers once, OS auto-repeat swallows without re-triggering
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, false, pure_win),
            WinArrowDecision::SwallowAndSwitch(DesktopSwitchDir::Right)
        );
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, false, pure_win),
            WinArrowDecision::SwallowOnly
        );

        // Rollover: pressing Left before releasing Right triggers Left independently
        assert_eq!(
            state.handle_key(VK_LEFT.0 as u32, true, false, false, pure_win),
            WinArrowDecision::SwallowAndSwitch(DesktopSwitchDir::Left)
        );

        // Even if Win is released before arrows, matching KEYUPs are still swallowed
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, false, true, false, none),
            WinArrowDecision::SwallowOnly
        );
        assert_eq!(
            state.handle_key(VK_LEFT.0 as u32, false, true, false, none),
            WinArrowDecision::SwallowOnly
        );

        // Subsequent unmatched arrow KEYUP (arrow pressed without Win) is NOT swallowed
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, false, true, false, none),
            WinArrowDecision::Pass
        );

        // Re-tapping Right while still holding Win triggers again
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, false, pure_win),
            WinArrowDecision::SwallowAndSwitch(DesktopSwitchDir::Right)
        );
        // Orphan KEYUP is swallowed even if overlay popped open in the meantime
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, false, true, true, none),
            WinArrowDecision::SwallowOnly
        );
    }

    #[test]
    fn overlay_focus_state_only_suppresses_when_overlay_is_actually_foreground() {
        let overlay = OverlayFocusState::new();
        overlay.register_hwnds(HWND(0x1111 as *mut c_void), HWND(0x2222 as *mut c_void));

        assert!(!overlay.is_overlay_foreground_with(0x1111));

        // Regression for hide_on_blur = false: panel visible in background while another app is focused
        overlay.set_visible(true);
        assert!(!overlay.is_overlay_foreground_with(0x9999));
        assert!(overlay.is_overlay_foreground_with(0x1111));
        assert!(overlay.is_overlay_foreground_with(0x2222));

        overlay.set_visible(false);
        assert!(!overlay.is_overlay_foreground_with(0x1111));
    }

    #[test]
    fn native_desktop_switch_inputs_always_inject_lctrl_and_tag_extra_info() {
        let extract = |inputs: &[INPUT]| -> Vec<(VIRTUAL_KEY, KEYBD_EVENT_FLAGS, usize)> {
            inputs
                .iter()
                .map(|i| unsafe {
                    (
                        i.Anonymous.ki.wVk,
                        i.Anonymous.ki.dwFlags,
                        i.Anonymous.ki.dwExtraInfo,
                    )
                })
                .collect()
        };

        // Normal case: user physically holds Win -> always inject LCtrl + Arrow, keep Win held
        let normal = build_native_desktop_switch_inputs(DesktopSwitchDir::Right, true);
        assert_eq!(
            extract(&normal),
            vec![
                (VK_LCONTROL, KEYBD_EVENT_FLAGS(0), WINDECK_SYNTHETIC_EXTRA_INFO),
                (VK_RIGHT, KEYEVENTF_EXTENDEDKEY, WINDECK_SYNTHETIC_EXTRA_INFO),
                (
                    VK_RIGHT,
                    KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                    WINDECK_SYNTHETIC_EXTRA_INFO,
                ),
                (VK_LCONTROL, KEYEVENTF_KEYUP, WINDECK_SYNTHETIC_EXTRA_INFO),
            ]
        );

        // Ultra-fast release race: user already released Win -> inject complete Ctrl+Win+Left
        let full = build_native_desktop_switch_inputs(DesktopSwitchDir::Left, false);
        assert_eq!(
            extract(&full),
            vec![
                (VK_LCONTROL, KEYBD_EVENT_FLAGS(0), WINDECK_SYNTHETIC_EXTRA_INFO),
                (VK_LWIN, KEYEVENTF_EXTENDEDKEY, WINDECK_SYNTHETIC_EXTRA_INFO),
                (VK_LEFT, KEYEVENTF_EXTENDEDKEY, WINDECK_SYNTHETIC_EXTRA_INFO),
                (
                    VK_LEFT,
                    KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                    WINDECK_SYNTHETIC_EXTRA_INFO,
                ),
                (
                    VK_LWIN,
                    KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                    WINDECK_SYNTHETIC_EXTRA_INFO,
                ),
                (VK_LCONTROL, KEYEVENTF_KEYUP, WINDECK_SYNTHETIC_EXTRA_INFO),
            ]
        );

        let mask = build_menu_mask_inputs();
        assert_eq!(
            extract(&mask),
            vec![
                (VK_MENU_MASK, KEYBD_EVENT_FLAGS(0), WINDECK_SYNTHETIC_EXTRA_INFO),
                (VK_MENU_MASK, KEYEVENTF_KEYUP, WINDECK_SYNTHETIC_EXTRA_INFO),
            ]
        );
    }

    #[test]
    fn unified_keyboard_hook_installs_refreshes_and_unhooks_cleanly_on_win32() {
        let handle = thread::spawn(|| {
            let overlay = Arc::new(OverlayFocusState::new());
            let sync = Arc::new(SharedHookSync::new(overlay, false));
            // Starting disabled keeps zero WH_KEYBOARD_LL hooks installed
            init_keyboard_hook_context(HWND::default(), HWND::default(), sync.clone(), false)
                .unwrap();
            KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert!(
                    s.hook.is_none(),
                    "hook should stay uninstalled while idle/disabled"
                );
                assert!(!s.win_arrow_patch.enabled);
            });

            // Enabling patch installs exactly one real Win32 WH_KEYBOARD_LL hook
            set_win_arrow_patch_hook(true).unwrap();
            assert!(sync.patch_enabled.load(Ordering::Acquire));
            let hook_when_enabled = KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert!(s.win_arrow_patch.enabled);
                s.hook
                    .expect("WH_KEYBOARD_LL hook should be installed when enabled")
            });

            // Disarming hold mode while patch is enabled must keep the single hook alive
            disarm_hold_hook_local();
            KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert_eq!(s.hook, Some(hook_when_enabled));
            });

            // Force refresh replaces the hook cleanly and resets any stale modifier state
            KEYBOARD_HOOK.with(|slot| {
                slot.borrow_mut()
                    .modifiers
                    .on_non_injected_key(VK_LWIN.0 as u32, true, false);
            });
            refresh_keyboard_hook_local(true);
            KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert!(s.hook.is_some(), "hook should stay installed after refresh");
                assert!(s.win_arrow_patch.enabled);
            });

            // Disabling patch immediately unhooks WH_KEYBOARD_LL (back to zero idle hooks)
            set_win_arrow_patch_hook(false).unwrap();
            assert!(!sync.patch_enabled.load(Ordering::Acquire));
            KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert!(
                    s.hook.is_none(),
                    "hook should be uninstalled once disabled"
                );
                assert!(!s.win_arrow_patch.enabled);
            });

            teardown_keyboard_hook_local();
        });
        handle.join().expect("hook lifecycle test thread panicked");
    }

    #[test]
    fn dedicated_kbd_hook_worker_toggles_refreshes_and_shuts_down_cleanly() {
        let overlay = Arc::new(OverlayFocusState::new());
        let sync = Arc::new(SharedHookSync::new(overlay, false));
        let (mut worker, startup_err) =
            KbdHookWorker::start(HWND::default(), sync.clone(), false).unwrap();
        assert!(startup_err.is_none());
        assert!(!sync.patch_enabled.load(Ordering::Acquire));

        let (tx, rx) = mpsc::channel();
        worker.forward_patch_request(Box::into_raw(Box::new(WinArrowPatchRequest {
            enabled: true,
            reply: tx,
        })));
        rx.recv_timeout(Duration::from_millis(800))
            .expect("enable reply")
            .expect("enable hook succeeded");
        assert!(sync.patch_enabled.load(Ordering::Acquire));

        worker.refresh_hook(true);
        worker.reset_modifiers();
        worker.disarm_hold();

        let (tx2, rx2) = mpsc::channel();
        worker.forward_patch_request(Box::into_raw(Box::new(WinArrowPatchRequest {
            enabled: false,
            reply: tx2,
        })));
        rx2.recv_timeout(Duration::from_millis(800))
            .expect("disable reply")
            .expect("disable hook succeeded");
        assert!(!sync.patch_enabled.load(Ordering::Acquire));

        worker.shutdown();
    }

    #[test]
    fn broadcast_window_forwards_power_wts_and_explorer_events() {
        unsafe extern "system" fn sink_wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        let handle = thread::spawn(|| {
            let hinstance = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
            let sink_class = w!("WinDeckBroadcastTestSinkClass");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(sink_wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: sink_class,
                ..Default::default()
            };
            unsafe { RegisterClassW(&wc) };
            let sink_hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    sink_class,
                    w!("WinDeckBroadcastTestSink"),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    Some(hinstance.into()),
                    None,
                )
            }
            .expect("create sink HWND");

            let fake_taskbar_msg = WM_APP + 123;
            let broadcast_hwnd = create_broadcast_window(hinstance, sink_hwnd, fake_taskbar_msg)
                .expect("create broadcast window");

            unsafe {
                broadcast_wnd_proc(
                    broadcast_hwnd,
                    WM_POWERBROADCAST,
                    WPARAM(PBT_APMRESUMEAUTOMATIC_CODE),
                    LPARAM(0),
                );
                broadcast_wnd_proc(
                    broadcast_hwnd,
                    WM_WTSSESSION_CHANGE,
                    WPARAM(WTS_SESSION_LOCK as usize),
                    LPARAM(0),
                );
                broadcast_wnd_proc(
                    broadcast_hwnd,
                    WM_WTSSESSION_CHANGE,
                    WPARAM(WTS_SESSION_UNLOCK as usize),
                    LPARAM(0),
                );
                broadcast_wnd_proc(broadcast_hwnd, fake_taskbar_msg, WPARAM(0), LPARAM(0));
            }

            let mut received = Vec::new();
            let mut msg = MSG::default();
            for _ in 0..4 {
                assert!(
                    unsafe { GetMessageW(&mut msg, Some(sink_hwnd), WM_APP, WM_APP + 200) }
                        .as_bool(),
                    "expected forwarded broadcast message"
                );
                received.push(msg.message);
            }
            assert_eq!(
                received,
                vec![
                    WM_SYSTEM_RESUMED,
                    WM_SESSION_LOCKED,
                    WM_SESSION_UNLOCKED,
                    WM_EXPLORER_RESTARTED,
                ]
            );

            unsafe {
                let _ = DestroyWindow(broadcast_hwnd);
                let _ = DestroyWindow(sink_hwnd);
            }
        });
        handle
            .join()
            .expect("broadcast forwarding test thread panicked");
    }
}
