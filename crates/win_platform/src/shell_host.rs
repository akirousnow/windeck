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
    GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_WIN,
    RegisterHotKey, UnregisterHotKey, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LCONTROL, VK_LEFT,
    VK_LMENU, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RWIN, VK_UP,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
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

const MENU_CMD_TOGGLE: u32 = 1001;
const MENU_CMD_RELOAD: u32 = 1002;
const MENU_CMD_OPEN_CONFIG: u32 = 1003;
const MENU_CMD_AUTOSTART: u32 = 1004;
const MENU_CMD_QUIT: u32 = 1005;
const MENU_CMD_SETTINGS: u32 = 1006;

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
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
            self.handle = HANDLE::default();
        }
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

struct HoldHookContext {
    hook: HHOOK,
    shell_hwnd: HWND,
    primary: PrimaryModifier,
    navigated: bool,
}

thread_local! {
    static HOLD_HOOK: RefCell<Option<HoldHookContext>> = const { RefCell::new(None) };
}

fn disarm_hold_hook() {
    HOLD_HOOK.with(|slot| {
        if let Some(ctx) = slot.borrow_mut().take() {
            unsafe {
                let _ = UnhookWindowsHookEx(ctx.hook);
            }
        }
    });
}

fn arm_hold_hook(shell_hwnd: HWND, primary: PrimaryModifier) -> bool {
    disarm_hold_hook();
    if !primary.is_physically_held() {
        return false;
    }
    let Ok(hook) = (unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hold_keyboard_ll_proc), None, 0) })
    else {
        return false;
    };
    HOLD_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(HoldHookContext {
            hook,
            shell_hwnd,
            primary,
            navigated: false,
        });
    });
    true
}

unsafe extern "system" fn hold_keyboard_ll_proc(
    n_code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if n_code < 0 {
        return unsafe { CallNextHookEx(None, n_code, wparam, lparam) };
    }

    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    // Never react to synthetic SendInput events
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

    let action = HOLD_HOOK.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(ctx) = borrow.as_mut() else {
            return Action::Pass;
        };

        // 1. Primary modifier physically released -> unhook immediately & notify shell_hwnd
        if is_keyup && ctx.primary.matches_vk(vk) {
            if !ctx.primary.sibling_still_held(vk) {
                let finished = borrow.take().unwrap();
                unsafe {
                    let _ = UnhookWindowsHookEx(finished.hook);
                    let _ = PostMessageW(
                        Some(finished.shell_hwnd),
                        WM_HOLD_RELEASED,
                        WPARAM(finished.navigated as usize),
                        LPARAM(0),
                    );
                }
            }
            // Always pass modifier KEYUP through so Windows never sees a stuck modifier!
            return Action::Pass;
        }

        // 2. Escape or Enter cancels hold-to-jump and passes through to GPUI immediately
        if is_keydown && (vk == VK_ESCAPE.0 as u32 || vk == VK_RETURN.0 as u32) {
            let finished = borrow.take().unwrap();
            unsafe {
                let _ = UnhookWindowsHookEx(finished.hook);
                let _ = PostMessageW(
                    Some(finished.shell_hwnd),
                    WM_HOLD_RELEASED,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            return Action::Pass;
        }

        // 3. Arrow keys while primary modifier is held -> navigate WinDeck & swallow key
        //    (prevents Win+Arrow Aero Snap, Alt+Arrow system ding, and Ctrl+Arrow double-fire)
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
                let finished = borrow.take().unwrap();
                unsafe {
                    let _ = UnhookWindowsHookEx(finished.hook);
                    let _ = PostMessageW(
                        Some(finished.shell_hwnd),
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
                        Some(ctx.shell_hwnd),
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

        Action::Pass
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

fn show_tray_menu(hwnd: HWND, state: &ShellHostState) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let toggle_label = HSTRING::from(format!("显示窗口切换器 ({})", state.hotkey_label));
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
            MENU_CMD_TOGGLE => (state.on_event)(ShellEvent::ToggleOverlay {
                hotkey_triggered: false,
                hold_armed: false,
            }),
            MENU_CMD_SETTINGS => {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                (state.on_event)(ShellEvent::OpenSettings);
            }
            MENU_CMD_RELOAD => (state.on_event)(ShellEvent::ReloadConfig),
            MENU_CMD_OPEN_CONFIG => (state.on_event)(ShellEvent::OpenConfigDir),
            MENU_CMD_AUTOSTART => {
                let target = !autostart_on;
                if set_autostart_enabled(target).is_ok() {
                    (state.on_event)(ShellEvent::AutostartToggled(target));
                }
            }
            MENU_CMD_QUIT => (state.on_event)(ShellEvent::Quit),
            _ => {}
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

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ShellHostState;
    if !state_ptr.is_null() {
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
                } else if mouse_msg == WM_RBUTTONUP {
                    show_tray_menu(hwnd, state);
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
                disarm_hold_hook();
                if state.hotkey_registered {
                    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID) };
                    state.hotkey_registered = false;
                }
                remove_tray_icon(hwnd);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            WM_DESTROY => {
                disarm_hold_hook();
                unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
                return LRESULT(0);
            }
            WM_NCDESTROY => {
                disarm_hold_hook();
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
    overlay_visible: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ShellHost {
    pub fn start<F>(hotkey: HotkeySpec, hotkey_label: String, on_event: F) -> Result<Self>
    where
        F: Fn(ShellEvent) + Send + 'static,
    {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(isize, bool, Option<String>)>>();
        let overlay_visible = Arc::new(AtomicBool::new(false));
        let overlay_visible_for_thread = overlay_visible.clone();

        let thread = thread::Builder::new()
            .name("windeck-shell-host".to_string())
            .spawn(move || {
                let init = (|| -> Result<(HWND, bool, Option<String>)> {
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

                    Ok((hwnd, reg_ok, reg_err))
                })();

                let hwnd = match init {
                    Ok((hwnd, reg_ok, reg_err)) => {
                        let _ = ready_tx.send(Ok((hwnd.0 as isize, reg_ok, reg_err)));
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

        let (hwnd_raw, hotkey_registered, hotkey_error) =
            ready_rx.recv().context("wait for shell host startup")??;

        Ok(Self {
            hwnd_raw,
            hotkey_registered,
            hotkey_error,
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
}
