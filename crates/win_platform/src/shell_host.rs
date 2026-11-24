use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS,
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_WIN, MapVirtualKeyW, RegisterHotKey, SendInput, UnregisterHotKey,
    VIRTUAL_KEY, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT,
    VK_LWIN, VK_MENU, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_UP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, AppendMenuW, CallNextHookEx, CreateIconIndirect,
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowExW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW,
    HHOOK, HICON, HWND_MESSAGE, ICONINFO, IMAGE_ICON, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LR_DEFAULTCOLOR, LoadImageW, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG,
    PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowsHookExW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONUP,
    WM_NCCREATE, WM_NCDESTROY, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
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

struct ShellHostState {
    on_event: Box<dyn Fn(ShellEvent) + Send>,
    tray_icon: HICON,
    tooltip: String,
    hotkey: HotkeySpec,
    hotkey_label: String,
    taskbar_created_msg: u32,
    hotkey_registered: bool,
    overlay_visible: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopSwitchDir {
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

fn is_shift_physically_held() -> bool {
    PrimaryModifier::is_vk_down(VK_LSHIFT.0) || PrimaryModifier::is_vk_down(VK_RSHIFT.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierSnapshot {
    win: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl ModifierSnapshot {
    fn capture() -> Self {
        Self {
            win: PrimaryModifier::Win.is_physically_held(),
            ctrl: PrimaryModifier::Ctrl.is_physically_held(),
            alt: PrimaryModifier::Alt.is_physically_held(),
            shift: is_shift_physically_held(),
        }
    }

    fn is_pure_win_only(self) -> bool {
        self.win && !self.ctrl && !self.alt && !self.shift
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WinArrowDecision {
    Pass,
    SwallowOnly,
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

    fn handle_key(
        &mut self,
        vk: u32,
        is_keydown: bool,
        is_keyup: bool,
        overlay_visible: bool,
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

        if !self.enabled || overlay_visible {
            *swallowed = false;
            return WinArrowDecision::Pass;
        }

        if mods.is_pure_win_only() {
            if !*swallowed {
                *swallowed = true;
                WinArrowDecision::SwallowAndSwitch(dir)
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
                ..Default::default()
            },
        },
    }
}

fn build_native_desktop_switch_inputs(
    dir: DesktopSwitchDir,
    win_already_held: bool,
    ctrl_already_held: bool,
) -> Vec<INPUT> {
    let arrow_vk = dir.arrow_vk();
    let inject_ctrl = !ctrl_already_held;
    let inject_win = !win_already_held;
    let mut inputs = Vec::with_capacity(6);

    if inject_ctrl {
        inputs.push(keybd_input(VK_LCONTROL, KEYBD_EVENT_FLAGS(0)));
    }
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
    if inject_ctrl {
        inputs.push(keybd_input(VK_LCONTROL, KEYEVENTF_KEYUP));
    }
    inputs
}

/// Synthesizes Windows' native `Ctrl + Win + Left/Right` virtual desktop switch shortcut.
/// Keeps a physically held `Win` key untouched so users can keep holding `Win` and tap
/// arrows repeatedly across multiple desktops without triggering the Start Menu.
fn send_native_desktop_switch(dir: DesktopSwitchDir) {
    if PrimaryModifier::Alt.is_physically_held() || is_shift_physically_held() {
        return;
    }
    let win_held = PrimaryModifier::Win.is_physically_held();
    let ctrl_held = PrimaryModifier::Ctrl.is_physically_held();
    let inputs = build_native_desktop_switch_inputs(dir, win_held, ctrl_held);
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

struct HoldHookContext {
    primary: PrimaryModifier,
    navigated: bool,
}

#[derive(Default)]
struct KeyboardHookState {
    hook: Option<HHOOK>,
    shell_hwnd: HWND,
    overlay_visible: Option<Arc<AtomicBool>>,
    hold: Option<HoldHookContext>,
    win_arrow_patch: WinArrowPatchState,
}

thread_local! {
    static KEYBOARD_HOOK: RefCell<KeyboardHookState> = RefCell::new(KeyboardHookState::default());
}

fn sync_keyboard_hook(state: &mut KeyboardHookState) -> Result<()> {
    let needed = state.hold.is_some() || state.win_arrow_patch.enabled;
    if needed && state.hook.is_none() {
        let hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_ll_proc), None, 0) }
                .context("SetWindowsHookExW(WH_KEYBOARD_LL) failed")?;
        state.hook = Some(hook);
    } else if !needed {
        if let Some(hook) = state.hook.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        state.win_arrow_patch.reset_swallowed();
    }
    Ok(())
}

fn init_keyboard_hook_context(
    shell_hwnd: HWND,
    overlay_visible: Arc<AtomicBool>,
    win_arrow_patch_enabled: bool,
) -> Result<()> {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.shell_hwnd = shell_hwnd;
        state.overlay_visible = Some(overlay_visible);
        state.hold = None;
        state.win_arrow_patch.set_enabled(win_arrow_patch_enabled);
        let res = sync_keyboard_hook(&mut state);
        if res.is_err() {
            state.win_arrow_patch.set_enabled(false);
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
            return Err(anyhow!("安装键盘钩子失败（{e}）"));
        }
        Ok(())
    })
}

fn disarm_hold_hook() {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.hold = None;
        let _ = sync_keyboard_hook(&mut state);
    });
}

fn teardown_keyboard_hook() {
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.hold = None;
        state.win_arrow_patch.set_enabled(false);
        if let Some(hook) = state.hook.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
        state.overlay_visible = None;
        state.shell_hwnd = HWND::default();
    });
}

fn arm_hold_hook(shell_hwnd: HWND, primary: PrimaryModifier) -> bool {
    if !primary.is_physically_held() {
        disarm_hold_hook();
        return false;
    }
    KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
        state.shell_hwnd = shell_hwnd;
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

unsafe extern "system" fn keyboard_ll_proc(
    n_code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if n_code < 0 {
        return unsafe { CallNextHookEx(None, n_code, wparam, lparam) };
    }

    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    // Never react to synthetic SendInput events (including our own Ctrl+Win+Arrow injection).
    if (kb.flags.0 & LLKHF_INJECTED.0) != 0 {
        return unsafe { CallNextHookEx(None, n_code, wparam, lparam) };
    }

    let msg = wparam.0 as u32;
    let vk = kb.vkCode;
    let is_keydown = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let is_keyup = msg == WM_KEYUP || msg == WM_SYSKEYUP;

    enum Action {
        Pass,
        Swallow,
    }

    let action = KEYBOARD_HOOK.with(|slot| {
        let mut state = slot.borrow_mut();
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

        // Priority 2: Global `Win + Left/Right -> switch virtual desktop` patch.
        // Fast-path skip non-arrow keys with zero Win32 syscalls during normal typing.
        if vk != VK_LEFT.0 as u32 && vk != VK_RIGHT.0 as u32 {
            return Action::Pass;
        }

        let overlay_visible = state
            .overlay_visible
            .as_ref()
            .is_some_and(|v| v.load(Ordering::Acquire));
        let mods = if is_keydown && state.win_arrow_patch.enabled && !overlay_visible {
            ModifierSnapshot::capture()
        } else {
            ModifierSnapshot {
                win: false,
                ctrl: false,
                alt: false,
                shift: false,
            }
        };

        match state
            .win_arrow_patch
            .handle_key(vk, is_keydown, is_keyup, overlay_visible, mods)
        {
            WinArrowDecision::Pass => Action::Pass,
            WinArrowDecision::SwallowOnly => Action::Swallow,
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

    if msg == WM_WIN_ARROW_SWITCH {
        if let Some(dir) = DesktopSwitchDir::from_wparam(wparam.0) {
            send_native_desktop_switch(dir);
        }
        return LRESULT(0);
    }

    if msg == WM_SET_WIN_ARROW_PATCH {
        let req_ptr = lparam.0 as *mut WinArrowPatchRequest;
        if !req_ptr.is_null() {
            let req = unsafe { Box::from_raw(req_ptr) };
            let res = set_win_arrow_patch_hook(req.enabled);
            let _ = req.reply.send(res);
        }
        return LRESULT(0);
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
            (state.on_event)(ShellEvent::ExplorerRestarted);
            return LRESULT(0);
        }

        match msg {
            WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => {
                // Propagate WM_HOTKEY's foreground activation right to GPUI's UI thread.
                let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                let opening = !state.overlay_visible.load(Ordering::Acquire);
                let hold_armed = if opening {
                    arm_hold_hook(hwnd, state.hotkey.primary)
                } else {
                    disarm_hold_hook();
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
                disarm_hold_hook();
                return LRESULT(0);
            }
            WM_TRAY_ICON => {
                let mouse_msg = (lparam.0 & 0xFFFF) as u32;
                if mouse_msg == WM_LBUTTONUP {
                    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                    disarm_hold_hook();
                    (state.on_event)(ShellEvent::ToggleOverlay {
                        hotkey_triggered: false,
                        hold_armed: false,
                    });
                } else if mouse_msg == NIN_BALLOONUSERCLICK {
                    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                    disarm_hold_hook();
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
                disarm_hold_hook();

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
                disarm_hold_hook();
                (state.on_event)(ShellEvent::ShowOverlay);
                return LRESULT(0);
            }
            WM_CLOSE => {
                teardown_keyboard_hook();
                if state.hotkey_registered {
                    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID) };
                    state.hotkey_registered = false;
                }
                remove_tray_icon(hwnd);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            WM_DESTROY => {
                teardown_keyboard_hook();
                unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                teardown_keyboard_hook();
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                let boxed = unsafe { Box::from_raw(state_ptr) };
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

/// Manages global hotkey + Windows system tray icon on a dedicated zero-CPU message thread.
pub struct ShellHost {
    hwnd_raw: isize,
    hotkey_registered: bool,
    hotkey_error: Option<String>,
    win_arrow_patch_error: Option<String>,
    overlay_visible: Arc<AtomicBool>,
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
        let overlay_visible = Arc::new(AtomicBool::new(false));
        let overlay_visible_for_thread = overlay_visible.clone();
        let overlay_visible_for_hook = overlay_visible.clone();

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
                    let state = Box::new(ShellHostState {
                        on_event: Box::new(on_event),
                        tray_icon,
                        tooltip: tooltip.clone(),
                        hotkey,
                        hotkey_label,
                        taskbar_created_msg,
                        hotkey_registered: false,
                        overlay_visible: overlay_visible_for_thread,
                    });
                    let state_ptr = Box::into_raw(state);

                    let hwnd = unsafe {
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
                            Some(state_ptr as *const c_void),
                        )
                    };
                    let hwnd = match hwnd {
                        Ok(h) => h,
                        Err(e) => {
                            let boxed = unsafe { Box::from_raw(state_ptr) };
                            let _ = unsafe { DestroyIcon(boxed.tray_icon) };
                            return Err(e.into());
                        }
                    };

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

                    let patch_err = init_keyboard_hook_context(
                        hwnd,
                        overlay_visible_for_hook,
                        win_arrow_switch_desktop,
                    )
                    .err()
                    .map(|e| format!("{e:#}"));

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
            overlay_visible,
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

    /// Keeps `ShellHost` aware of overlay visibility and guarantees any temporary
    /// hold-modifier keyboard hook is immediately removed when hidden.
    pub fn set_overlay_visible(&self, visible: bool) {
        self.overlay_visible.store(visible, Ordering::Release);
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
    /// on the shell host thread, installing or uninstalling the unified `WH_KEYBOARD_LL` hook as needed.
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
        rx.recv_timeout(std::time::Duration::from_millis(800))
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
        rx.recv_timeout(std::time::Duration::from_millis(800))
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
        assert!(HotkeySpec::parse("Shift+Q").is_err(), "shift is not a valid primary modifier");
        assert!(HotkeySpec::parse("Ctrl+Alt+Q").is_err(), "multiple modifiers should fail");
        assert!(HotkeySpec::parse("Alt+Space").is_err(), "non-letter secondary key should fail");
        assert!(HotkeySpec::parse("Ctrl+1").is_err(), "digit secondary key should fail");
    }

    #[test]
    fn win_arrow_patch_handles_exclusive_win_repeats_rollover_and_overlay() {
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

        // Suppressed when overlay is visible
        assert_eq!(
            state.handle_key(VK_RIGHT.0 as u32, true, false, true, pure_win),
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
    fn native_desktop_switch_inputs_respect_held_modifiers() {
        let extract = |inputs: &[INPUT]| -> Vec<(VIRTUAL_KEY, KEYBD_EVENT_FLAGS)> {
            inputs
                .iter()
                .map(|i| unsafe { (i.Anonymous.ki.wVk, i.Anonymous.ki.dwFlags) })
                .collect()
        };

        // Normal case: user physically holds Win -> inject only LCtrl + Arrow, keep Win held
        let normal = build_native_desktop_switch_inputs(DesktopSwitchDir::Right, true, false);
        assert_eq!(
            extract(&normal),
            vec![
                (VK_LCONTROL, KEYBD_EVENT_FLAGS(0)),
                (VK_RIGHT, KEYEVENTF_EXTENDEDKEY),
                (VK_RIGHT, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
                (VK_LCONTROL, KEYEVENTF_KEYUP),
            ]
        );

        // Ultra-fast release race: user already released Win -> inject complete Ctrl+Win+Left
        let full = build_native_desktop_switch_inputs(DesktopSwitchDir::Left, false, false);
        assert_eq!(
            extract(&full),
            vec![
                (VK_LCONTROL, KEYBD_EVENT_FLAGS(0)),
                (VK_LWIN, KEYEVENTF_EXTENDEDKEY),
                (VK_LEFT, KEYEVENTF_EXTENDEDKEY),
                (VK_LEFT, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
                (VK_LWIN, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
                (VK_LCONTROL, KEYEVENTF_KEYUP),
            ]
        );

        // Defensive: if Ctrl is already held, never emit a synthetic Ctrl KEYUP
        let ctrl_held = build_native_desktop_switch_inputs(DesktopSwitchDir::Left, true, true);
        assert_eq!(
            extract(&ctrl_held),
            vec![
                (VK_LEFT, KEYEVENTF_EXTENDEDKEY),
                (VK_LEFT, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
            ]
        );
    }

    #[test]
    fn unified_keyboard_hook_installs_and_unhooks_cleanly_on_win32() {
        let handle = thread::spawn(|| {
            let overlay_visible = Arc::new(AtomicBool::new(false));
            // Starting disabled keeps zero WH_KEYBOARD_LL hooks installed
            init_keyboard_hook_context(HWND::default(), overlay_visible, false).unwrap();
            KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert!(s.hook.is_none(), "hook should stay uninstalled while idle/disabled");
                assert!(!s.win_arrow_patch.enabled);
            });

            // Enabling patch installs exactly one real Win32 WH_KEYBOARD_LL hook
            set_win_arrow_patch_hook(true).unwrap();
            let hook_when_enabled = KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert!(s.win_arrow_patch.enabled);
                s.hook.expect("WH_KEYBOARD_LL hook should be installed when enabled")
            });

            // Disarming hold mode while patch is enabled must keep the single hook alive
            disarm_hold_hook();
            KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert_eq!(s.hook, Some(hook_when_enabled));
            });

            // Disabling patch immediately unhooks WH_KEYBOARD_LL (back to zero idle hooks)
            set_win_arrow_patch_hook(false).unwrap();
            KEYBOARD_HOOK.with(|slot| {
                let s = slot.borrow();
                assert!(s.hook.is_none(), "hook should be uninstalled once disabled");
                assert!(!s.win_arrow_patch.enabled);
            });

            teardown_keyboard_hook();
        });
        handle.join().expect("hook lifecycle test thread panicked");
    }
}
