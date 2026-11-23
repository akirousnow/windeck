use std::ffi::c_void;
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
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey, VK_OEM_3, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, AppendMenuW, CreateIconIndirect, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW,
    FindWindowExW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, HICON,
    HWND_MESSAGE, ICONINFO, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, MSG, PostMessageW, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_DESTROY, WM_HOTKEY, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_RBUTTONUP,
    WNDCLASSW,
};
use windows_core::{HSTRING, PCWSTR, w};
use windows_registry::CURRENT_USER;

const HOTKEY_ID: i32 = 1;
const WM_TRAY_ICON: u32 = WM_APP + 1;
const WM_REBIND_HOTKEY: u32 = WM_APP + 2;
const WM_ACTIVATE_EXISTING: u32 = WM_APP + 3;

const MENU_CMD_TOGGLE: u32 = 1001;
const MENU_CMD_RELOAD: u32 = 1002;
const MENU_CMD_OPEN_CONFIG: u32 = 1003;
const MENU_CMD_AUTOSTART: u32 = 1004;
const MENU_CMD_QUIT: u32 = 1005;

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
pub struct HotkeySpec {
    pub modifiers: u32,
    pub vk: u32,
}

impl HotkeySpec {
    pub fn parse(spec: &str) -> Result<Self> {
        let mut modifiers = MOD_NOREPEAT.0;
        let mut vk: Option<u32> = None;

        for raw in spec.split('+') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            match token.to_ascii_lowercase().as_str() {
                "alt" => modifiers |= MOD_ALT.0,
                "ctrl" | "control" => modifiers |= MOD_CONTROL.0,
                "shift" => modifiers |= MOD_SHIFT.0,
                "win" | "super" | "meta" => modifiers |= MOD_WIN.0,
                "space" => vk = Some(VK_SPACE.0 as u32),
                "tab" => vk = Some(VK_TAB.0 as u32),
                "`" | "~" | "grave" | "backquote" => vk = Some(VK_OEM_3.0 as u32),
                other if other.len() == 1 => {
                    let ch = other.as_bytes()[0].to_ascii_uppercase();
                    if ch.is_ascii_alphanumeric() {
                        vk = Some(ch as u32);
                    } else {
                        return Err(anyhow!("Unsupported hotkey key: '{token}'"));
                    }
                }
                other if other.starts_with('f') => {
                    let num: u32 = other[1..]
                        .parse()
                        .map_err(|_| anyhow!("Unsupported hotkey key: '{token}'"))?;
                    if (1..=24).contains(&num) {
                        vk = Some(0x70 + (num - 1));
                    } else {
                        return Err(anyhow!("Function key out of range: '{token}'"));
                    }
                }
                _ => return Err(anyhow!("Unsupported hotkey token: '{token}'")),
            }
        }

        let vk = vk.ok_or_else(|| anyhow!("Hotkey '{spec}' is missing a primary key"))?;
        if (modifiers & !MOD_NOREPEAT.0) == 0 {
            return Err(anyhow!(
                "Hotkey '{spec}' must include at least one modifier (Alt/Ctrl/Shift/Win)"
            ));
        }

        Ok(Self { modifiers, vk })
    }
}

#[derive(Debug, Clone)]
pub enum ShellEvent {
    ToggleOverlay { hotkey_triggered: bool },
    ShowOverlay,
    ReloadConfig,
    OpenConfigDir,
    AutostartToggled(bool),
    ExplorerRestarted,
    Quit,
}

struct ShellHostState {
    on_event: Box<dyn Fn(ShellEvent) + Send>,
    tray_icon: HICON,
    tooltip: String,
    hotkey_label: String,
    taskbar_created_msg: u32,
    hotkey_registered: bool,
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
            }),
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
                (state.on_event)(ShellEvent::ToggleOverlay {
                    hotkey_triggered: true,
                });
                return LRESULT(0);
            }
            WM_TRAY_ICON => {
                let mouse_msg = (lparam.0 & 0xFFFF) as u32;
                if mouse_msg == WM_LBUTTONUP {
                    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                    (state.on_event)(ShellEvent::ToggleOverlay {
                        hotkey_triggered: false,
                    });
                } else if mouse_msg == WM_RBUTTONUP {
                    show_tray_menu(hwnd, state);
                }
                return LRESULT(0);
            }
            WM_REBIND_HOTKEY => {
                let new_mods = wparam.0 as u32;
                let new_vk = lparam.0 as u32;
                if state.hotkey_registered {
                    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID) };
                    state.hotkey_registered = false;
                }
                if unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID, HOT_KEY_MODIFIERS(new_mods), new_vk) }
                    .is_ok()
                {
                    state.hotkey_registered = true;
                }
                return LRESULT(0);
            }
            WM_ACTIVATE_EXISTING => {
                let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                (state.on_event)(ShellEvent::ShowOverlay);
                return LRESULT(0);
            }
            WM_CLOSE => {
                if state.hotkey_registered {
                    let _ = unsafe { UnregisterHotKey(Some(hwnd), HOTKEY_ID) };
                    state.hotkey_registered = false;
                }
                remove_tray_icon(hwnd);
                let _ = unsafe { DestroyWindow(hwnd) };
                return LRESULT(0);
            }
            WM_DESTROY => {
                unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
                return LRESULT(0);
            }
            WM_NCDESTROY => {
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
    thread: Option<thread::JoinHandle<()>>,
}

impl ShellHost {
    pub fn start<F>(hotkey: HotkeySpec, hotkey_label: String, on_event: F) -> Result<Self>
    where
        F: Fn(ShellEvent) + Send + 'static,
    {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(isize, bool, Option<String>)>>();

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
                        hotkey_label,
                        taskbar_created_msg,
                        hotkey_registered: false,
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
            thread: Some(thread),
        })
    }

    pub fn hotkey_registered(&self) -> bool {
        self.hotkey_registered
    }

    pub fn hotkey_error(&self) -> Option<&str> {
        self.hotkey_error.as_deref()
    }

    pub fn rebind_hotkey(&self, hotkey: HotkeySpec) {
        let hwnd = HWND(self.hwnd_raw as *mut c_void);
        unsafe {
            let _ = PostMessageW(
                Some(hwnd),
                WM_REBIND_HOTKEY,
                WPARAM(hotkey.modifiers as usize),
                LPARAM(hotkey.vk as isize),
            );
        }
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
    fn hotkey_spec_parses_common_combos() {
        let alt_q = HotkeySpec::parse("Alt+Q").unwrap();
        assert_eq!(alt_q.vk, b'Q' as u32);
        assert_ne!(alt_q.modifiers & MOD_ALT.0, 0);

        let ctrl_shift_space = HotkeySpec::parse("Ctrl+Shift+Space").unwrap();
        assert_eq!(ctrl_shift_space.vk, VK_SPACE.0 as u32);
        assert_ne!(ctrl_shift_space.modifiers & MOD_CONTROL.0, 0);
        assert_ne!(ctrl_shift_space.modifiers & MOD_SHIFT.0, 0);

        let alt_grave = HotkeySpec::parse("Alt+`").unwrap();
        assert_eq!(alt_grave.vk, VK_OEM_3.0 as u32);

        let f_key = HotkeySpec::parse("Ctrl+Alt+F12").unwrap();
        assert_eq!(f_key.vk, 0x7B);

        assert!(HotkeySpec::parse("Q").is_err(), "missing modifier should fail");
        assert!(HotkeySpec::parse("Alt+").is_err(), "missing key should fail");
    }
}
