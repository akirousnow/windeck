use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, Focusable, KeyDownEvent, Pixels, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, canvas, div, prelude::*,
    px, rgb, rgba, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use win_platform::{
    DwmThumbnail, VirtualDesktopClient, capture_process_resources, current_session_desktop_names,
    enumerate_monitors, enumerate_windows,
};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWL_EXSTYLE, GWL_STYLE, GetClientRect,
    GetWindowLongW, IsWindowVisible, RegisterClassW, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE,
    ShowWindow, WINDOW_EX_STYLE, WNDCLASSW, WM_PAINT, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::w;

/// Helper synthetic source window (used in --self-test or when no external windows exist).
struct DummySourceWindow {
    hwnd: HWND,
}

impl DummySourceWindow {
    fn create() -> Result<Self> {
        unsafe extern "system" fn proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if msg == WM_PAINT {
                unsafe {
                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    let bg = CreateSolidBrush(COLORREF(0x00CC8811));
                    let _ = FillRect(hdc, &ps.rcPaint, bg);
                    let _ = DeleteObject(bg.into());
                    let stripe = RECT {
                        left: 20,
                        top: 20,
                        right: 360,
                        bottom: 120,
                    };
                    let fg = CreateSolidBrush(COLORREF(0x0022EE66));
                    let _ = FillRect(hdc, &stripe, fg);
                    let _ = DeleteObject(fg.into());
                    let _ = EndPaint(hwnd, &ps);
                }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        let hinstance = unsafe { GetModuleHandleW(None) }?;
        let class_name = w!("WinDeckProbeBDummySource");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        unsafe { RegisterClassW(&wc) };

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("ProbeB Dummy Source Window"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                40,
                40,
                480,
                300,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .context("create dummy source window")?;
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(Self { hwnd })
    }
}

impl Drop for DummySourceWindow {
    fn drop(&mut self) {
        if !self.hwnd.is_invalid() {
            let _ = unsafe { DestroyWindow(self.hwnd) };
        }
    }
}

fn extract_hwnd(window: &Window) -> Result<HWND> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|e| anyhow::anyhow!("HasWindowHandle failed: {e}"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as *mut c_void)),
        other => anyhow::bail!("Expected Win32 handle, got {:?}", other),
    }
}

struct TargetEntry {
    hwnd: HWND,
    label: String,
    desktop_label: String,
}

struct ThumbnailSlots {
    gpui_hwnd: HWND,
    targets: Vec<TargetEntry>,
    thumbs: Vec<Option<DwmThumbnail>>,
    last_errors: Vec<Option<String>>,
    last_dest_rects: Vec<Option<RECT>>,
}

impl ThumbnailSlots {
    fn new(gpui_hwnd: HWND, targets: Vec<TargetEntry>) -> Self {
        let len = targets.len();
        Self {
            gpui_hwnd,
            targets,
            thumbs: (0..len).map(|_| None).collect(),
            last_errors: vec![None; len],
            last_dest_rects: vec![None; len],
        }
    }

    fn register_all(&mut self) {
        self.unregister_all();
        for (idx, target) in self.targets.iter().enumerate() {
            match DwmThumbnail::register(self.gpui_hwnd, target.hwnd) {
                Ok(t) => {
                    let src_sz = t.query_source_size();
                    println!(
                        "  [Slot {}] DwmRegisterThumbnail(src=0x{:X} '{}') => OK, source_size={:?}",
                        idx,
                        target.hwnd.0 as isize,
                        target.label,
                        src_sz
                    );
                    self.thumbs[idx] = Some(t);
                    self.last_errors[idx] = None;
                }
                Err(e) => {
                    println!(
                        "  [Slot {}] DwmRegisterThumbnail(src=0x{:X}) => ERR: {e:#}",
                        idx,
                        target.hwnd.0 as isize
                    );
                    self.last_errors[idx] = Some(format!("{e}"));
                }
            }
        }
    }

    fn unregister_all(&mut self) {
        for slot in &mut self.thumbs {
            *slot = None;
        }
    }

    fn sync_slot(
        &mut self,
        idx: usize,
        bounds: Bounds<Pixels>,
        scale: f32,
        clip_viewport: Option<Bounds<Pixels>>,
    ) {
        let Some(Some(thumb)) = self.thumbs.get(idx) else {
            return;
        };
        let Ok((src_w, src_h)) = thumb.query_source_size() else {
            return;
        };
        if src_w <= 0 || src_h <= 0 {
            return;
        }

        // Convert logical GPUI bounds to physical client pixels.
        let mut dst_left = (f32::from(bounds.origin.x) * scale).round() as i32;
        let mut dst_top = (f32::from(bounds.origin.y) * scale).round() as i32;
        let mut dst_right = (f32::from(bounds.origin.x + bounds.size.width) * scale).round() as i32;
        let mut dst_bottom =
            (f32::from(bounds.origin.y + bounds.size.height) * scale).round() as i32;

        // Aspect-fit source inside destination box.
        let box_w = (dst_right - dst_left).max(1) as f32;
        let box_h = (dst_bottom - dst_top).max(1) as f32;
        let aspect_src = src_w as f32 / src_h as f32;
        let aspect_box = box_w / box_h;
        if aspect_src > aspect_box {
            let fitted_h = (box_w / aspect_src).round() as i32;
            let pad = ((dst_bottom - dst_top) - fitted_h) / 2;
            dst_top += pad;
            dst_bottom = dst_top + fitted_h;
        } else {
            let fitted_w = (box_h * aspect_src).round() as i32;
            let pad = ((dst_right - dst_left) - fitted_w) / 2;
            dst_left += pad;
            dst_right = dst_left + fitted_w;
        }

        let mut src_rect = RECT {
            left: 0,
            top: 0,
            right: src_w,
            bottom: src_h,
        };
        let mut visible = true;

        // Apply viewport scroll clipping (proportionally crops rcSource & clamps rcDestination).
        if let Some(clip) = clip_viewport {
            let clip_top = (f32::from(clip.origin.y) * scale).round() as i32;
            let clip_bottom = (f32::from(clip.origin.y + clip.size.height) * scale).round() as i32;
            let orig_h = (dst_bottom - dst_top).max(1) as f32;

            if dst_bottom <= clip_top || dst_top >= clip_bottom {
                visible = false;
            } else {
                if dst_top < clip_top {
                    let cut = (clip_top - dst_top) as f32 / orig_h;
                    src_rect.top = (cut * src_h as f32).round() as i32;
                    dst_top = clip_top;
                }
                if dst_bottom > clip_bottom {
                    let cut = (dst_bottom - clip_bottom) as f32 / orig_h;
                    src_rect.bottom = src_h - (cut * src_h as f32).round() as i32;
                    dst_bottom = clip_bottom;
                }
            }
        }

        let dest_rect = RECT {
            left: dst_left,
            top: dst_top,
            right: dst_right.max(dst_left + 1),
            bottom: dst_bottom.max(dst_top + 1),
        };
        self.last_dest_rects[idx] = Some(dest_rect);

        let _ = thumb.update(dest_rect, Some(src_rect), 255, visible, false);
    }
}

struct ProbeBView {
    focus_handle: FocusHandle,
    gpui_hwnd: HWND,
    slots: Rc<RefCell<ThumbnailSlots>>,
    _dummy: Option<DummySourceWindow>,
    bg_mode_name: &'static str,
    scroll_offset_px: f32,
    status_line: String,
    dcomp_disabled: bool,
}

impl Focusable for ProbeBView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ProbeBView {
    fn run_50_cycle_stress(&mut self) -> String {
        // 10 warmup cycles to let lazy OS/DWM caches stabilize before measuring steady-state deltas.
        for _ in 0..10 {
            unsafe {
                let _ = ShowWindow(self.gpui_hwnd, SW_SHOW);
            }
            {
                let mut slots = self.slots.borrow_mut();
                slots.unregister_all();
                for idx in 0..slots.targets.len() {
                    let src = slots.targets[idx].hwnd;
                    if let Ok(thumb) = DwmThumbnail::register(self.gpui_hwnd, src) {
                        slots.thumbs[idx] = Some(thumb);
                    }
                }
            }
            self.slots.borrow_mut().unregister_all();
            unsafe {
                let _ = ShowWindow(self.gpui_hwnd, SW_HIDE);
            }
        }

        let before = capture_process_resources();
        let mut latencies_ms = Vec::with_capacity(50);

        for _ in 0..50 {
            let t = Instant::now();
            unsafe {
                let _ = ShowWindow(self.gpui_hwnd, SW_SHOW);
            }
            {
                let mut slots = self.slots.borrow_mut();
                slots.unregister_all();
                for idx in 0..slots.targets.len() {
                    let src = slots.targets[idx].hwnd;
                    if let Ok(thumb) = DwmThumbnail::register(self.gpui_hwnd, src) {
                        let _ = thumb.update(
                            RECT {
                                left: 40 + (idx as i32) * 320,
                                top: 140,
                                right: 340 + (idx as i32) * 320,
                                bottom: 340,
                            },
                            None,
                            255,
                            true,
                            false,
                        );
                        slots.thumbs[idx] = Some(thumb);
                    }
                }
            }
            latencies_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            self.slots.borrow_mut().unregister_all();
            unsafe {
                let _ = ShowWindow(self.gpui_hwnd, SW_HIDE);
            }
        }

        let after = capture_process_resources();
        latencies_ms.sort_by(|a, b| a.total_cmp(b));
        let p50 = latencies_ms[latencies_ms.len() / 2];
        let p95 = latencies_ms[(latencies_ms.len() * 95) / 100];
        let max = *latencies_ms.last().unwrap_or(&0.0);

        let summary = format!(
            "50-cycle hide/show+DWM: p50={:.2}ms p95={:.2}ms max={:.2}ms | GDI {}->{} (d={:+}) USER {}->{} (d={:+}) Handles {}->{} (d={:+})",
            p50,
            p95,
            max,
            before.gdi_objects,
            after.gdi_objects,
            after.gdi_objects as i64 - before.gdi_objects as i64,
            before.user_objects,
            after.user_objects,
            after.user_objects as i64 - before.user_objects as i64,
            before.kernel_handles,
            after.kernel_handles,
            after.kernel_handles as i64 - before.kernel_handles as i64,
        );
        println!("[STRESS] {summary}");
        summary
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" | "q" => {
                self.slots.borrow_mut().unregister_all();
                cx.quit();
            }
            "1" => {
                window.set_background_appearance(WindowBackgroundAppearance::Opaque);
                self.bg_mode_name = "Opaque";
                self.status_line = "Switched background to Opaque".into();
                cx.notify();
            }
            "2" => {
                window.set_background_appearance(WindowBackgroundAppearance::Transparent);
                self.bg_mode_name = "Transparent";
                self.status_line = "Switched background to Transparent".into();
                cx.notify();
            }
            "3" => {
                window.set_background_appearance(WindowBackgroundAppearance::Blurred);
                self.bg_mode_name = "Blurred (Acrylic)";
                self.status_line = "Switched background to Blurred (Acrylic)".into();
                cx.notify();
            }
            "up" => {
                self.scroll_offset_px = (self.scroll_offset_px - 30.0).max(-180.0);
                self.status_line = format!("Card 3 scroll offset = {:.0}px", self.scroll_offset_px);
                cx.notify();
            }
            "down" => {
                self.scroll_offset_px = (self.scroll_offset_px + 30.0).min(180.0);
                self.status_line = format!("Card 3 scroll offset = {:.0}px", self.scroll_offset_px);
                cx.notify();
            }
            "h" => {
                println!("[HIDE TEST] Hiding GPUI window & unregistering DWM thumbnails for 1.5s...");
                self.slots.borrow_mut().unregister_all();
                unsafe {
                    let _ = ShowWindow(self.gpui_hwnd, SW_HIDE);
                }
                let t_hidden = Instant::now();
                let res_before = capture_process_resources();
                thread::sleep(Duration::from_millis(1500));
                let res_after = capture_process_resources();
                let wall_ms = t_hidden.elapsed().as_secs_f64() * 1000.0;
                let cpu_delta_ms = res_after.cpu_time_ms - res_before.cpu_time_ms;

                let t_show = Instant::now();
                unsafe {
                    let _ = ShowWindow(self.gpui_hwnd, SW_SHOW);
                }
                self.slots.borrow_mut().register_all();
                let show_ms = t_show.elapsed().as_secs_f64() * 1000.0;
                self.status_line = format!(
                    "Hide 1.5s idle CPU={:.2}ms ({:.2}%) | Re-show+register={:.2}ms",
                    cpu_delta_ms,
                    (cpu_delta_ms / wall_ms) * 100.0,
                    show_ms
                );
                println!("[HIDE TEST] {}", self.status_line);
                cx.notify();
            }
            "l" => {
                let summary = self.run_50_cycle_stress();
                // Restore visible after stress test
                unsafe {
                    let _ = ShowWindow(self.gpui_hwnd, SW_SHOW);
                }
                self.slots.borrow_mut().register_all();
                self.status_line = summary;
                cx.notify();
            }
            _ => {}
        }
    }
}

impl Render for ProbeBView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scale = window.scale_factor();
        let viewport = window.viewport_size();
        let slots_ref = self.slots.clone();
        let scroll_offset = self.scroll_offset_px;

        let card_labels: Vec<(String, String)> = {
            let s = self.slots.borrow();
            s.targets
                .iter()
                .map(|t| (t.label.clone(), t.desktop_label.clone()))
                .collect()
        };

        let slot0_info = card_labels
            .first()
            .cloned()
            .unwrap_or_else(|| ("(none)".into(), "-".into()));
        let slot1_info = card_labels
            .get(1)
            .or_else(|| card_labels.first())
            .cloned()
            .unwrap_or_else(|| ("(none)".into(), "-".into()));
        let slot2_info = card_labels
            .get(2)
            .or_else(|| card_labels.first())
            .cloned()
            .unwrap_or_else(|| ("(none)".into(), "-".into()));

        let slot1_idx = if card_labels.len() > 1 { 1 } else { 0 };
        let slot2_idx = if card_labels.len() > 2 { 2 } else { 0 };

        let slots_for_0 = slots_ref.clone();
        let slots_for_1 = slots_ref.clone();
        let slots_for_2 = slots_ref.clone();

        div()
            .id("probe-b-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_col()
            .p_5()
            .gap_4()
            .bg(rgba(0x141720D8))
            .border_2()
            .border_color(rgb(0x3B82F6))
            .text_color(rgb(0xF1F5F9))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .child(format!(
                                "WinDeck Probe B | GPUI HWND=0x{:X} | scale={:.2}x | viewport={:.0}x{:.0} | DComp={} | Bg={}",
                                self.gpui_hwnd.0 as isize,
                                scale,
                                f32::from(viewport.width),
                                f32::from(viewport.height),
                                if self.dcomp_disabled { "DISABLED" } else { "ENABLED" },
                                self.bg_mode_name,
                            )),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x94A3B8))
                            .child("Keys: [1]Opaque [2]Transparent [3]Blurred | [Up/Down]Clip Card 3 | [H]Hide 1.5s+Measure | [L]50-Cycle Leak Test | [Esc/Q]Quit"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x38BDF8))
                            .child(self.status_line.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .flex_1()
                    // CARD 1: Opaque Magenta behind DWM thumbnail -> tests Z-order!
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x1E293B))
                            .border_2()
                            .border_color(rgb(0xF43F5E))
                            .child(div().text_sm().child("Slot 1: Z-Order Test (Opaque Magenta #FF00FF)"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x94A3B8))
                                    .child(format!("{} | {}", slot0_info.1, slot0_info.0)),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .bg(rgb(0xFF00FF))
                                    .border_2()
                                    .border_color(rgb(0x22C55E))
                                    .child(
                                        canvas(
                                            move |bounds, window, _| {
                                                let sf = window.scale_factor();
                                                slots_for_0.borrow_mut().sync_slot(0, bounds, sf, None);
                                            },
                                            |_, _, _, _| {},
                                        )
                                        .size_full(),
                                    ),
                            ),
                    )
                    // CARD 2: Transparent Cutout + Overlapping GPUI Badge -> tests if GPUI overlays above DWM
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x1E293B))
                            .border_2()
                            .border_color(rgb(0xF59E0B))
                            .child(div().text_sm().child("Slot 2: Transparent Cutout + GPUI Badge"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x94A3B8))
                                    .child(format!("{} | {}", slot1_info.1, slot1_info.0)),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .bg(rgba(0x00000000))
                                    .border_2()
                                    .border_color(rgb(0xFACC15))
                                    .rounded_xl()
                                    .child(
                                        canvas(
                                            move |bounds, window, _| {
                                                let sf = window.scale_factor();
                                                slots_for_1
                                                    .borrow_mut()
                                                    .sync_slot(slot1_idx, bounds, sf, None);
                                            },
                                            |_, _, _, _| {},
                                        )
                                        .size_full(),
                                    )
                                    .child(
                                        div()
                                            .absolute()
                                            .top_2()
                                            .left_2()
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .bg(rgb(0xEAB308))
                                            .text_color(rgb(0x0F172A))
                                            .text_xs()
                                            .child("GPUI BADGE OVER THUMB?"),
                                    ),
                            ),
                    )
                    // CARD 3: Scroll-clipping test using rcSource + rcDestination clamping
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x1E293B))
                            .border_2()
                            .border_color(rgb(0x10B981))
                            .child(div().text_sm().child(format!(
                                "Slot 3: Clip Test (offset={:.0}px, Up/Down)",
                                scroll_offset
                            )))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x94A3B8))
                                    .child(format!("{} | {}", slot2_info.1, slot2_info.0)),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .overflow_hidden()
                                    .border_2()
                                    .border_color(rgb(0x06B6D4))
                                    .bg(rgba(0x00000000))
                                    .child(
                                        canvas(
                                            move |clip_bounds, window, _| {
                                                let sf = window.scale_factor();
                                                let shifted = Bounds {
                                                    origin: gpui::point(
                                                        clip_bounds.origin.x,
                                                        clip_bounds.origin.y + px(scroll_offset),
                                                    ),
                                                    size: clip_bounds.size,
                                                };
                                                slots_for_2.borrow_mut().sync_slot(
                                                    slot2_idx,
                                                    shifted,
                                                    sf,
                                                    Some(clip_bounds),
                                                );
                                            },
                                            |_, _, _, _| {},
                                        )
                                        .size_full(),
                                    ),
                            ),
                    ),
            )
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let self_test = args.iter().any(|a| a == "--self-test");
    let dcomp_disabled = std::env::var("GPUI_DISABLE_DIRECT_COMPOSITION")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let (winsta, desk) = current_session_desktop_names();
    println!("=== PROBE B START ===");
    println!("WindowStation={winsta} ThreadDesktop={desk} self_test={self_test} dcomp_disabled={dcomp_disabled}");

    let vdm = VirtualDesktopClient::new()?;
    let desktops = vdm.get_desktops().unwrap_or_default();
    let monitors = enumerate_monitors();

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.0), px(680.0)), cx);
        let window_res = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                focus: !self_test,
                show: true,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            move |window, cx| {
                let gpui_hwnd = extract_hwnd(window).expect("extract GPUI Win32 HWND");
                let style = unsafe { GetWindowLongW(gpui_hwnd, GWL_STYLE) } as u32;
                let ex_style = unsafe { GetWindowLongW(gpui_hwnd, GWL_EXSTYLE) } as u32;
                let dpi = unsafe { GetDpiForWindow(gpui_hwnd) };
                let mut client_rc = RECT::default();
                let _ = unsafe { GetClientRect(gpui_hwnd, &mut client_rc) };

                println!("=== GPUI WINDOW FACTS ===");
                println!("HWND: 0x{:X}", gpui_hwnd.0 as isize);
                println!(
                    "GWL_STYLE=0x{:08X} GWL_EXSTYLE=0x{:08X} | WS_EX_TOOLWINDOW={} WS_EX_NOREDIRECTIONBITMAP={}",
                    style,
                    ex_style,
                    (ex_style & WS_EX_TOOLWINDOW.0) != 0,
                    (ex_style & WS_EX_NOREDIRECTIONBITMAP.0) != 0
                );
                println!(
                    "GetDpiForWindow={} | GPUI scale_factor={:.2} | ClientRect={}x{} | GPUI viewport={:.0}x{:.0}",
                    dpi,
                    window.scale_factor(),
                    client_rc.right - client_rc.left,
                    client_rc.bottom - client_rc.top,
                    f32::from(window.viewport_size().width),
                    f32::from(window.viewport_size().height),
                );

                // Pick up to 3 real windows (preferring windows from different virtual desktops!).
                let (real_windows, _) =
                    enumerate_windows(&vdm, &desktops, &monitors, Some(gpui_hwnd.0 as isize), false)
                        .unwrap_or_default();
                let mut targets: Vec<TargetEntry> = Vec::new();
                // Try picking 1 per desktop first so cross-desktop thumbnails are visibly tested!
                for d in &desktops {
                    if let Some(w) = real_windows.iter().find(|w| w.desktop_id == Some(d.id)) {
                        targets.push(TargetEntry {
                            hwnd: HWND(w.hwnd as *mut c_void),
                            label: format!("{} - {}", w.process_name, w.title),
                            desktop_label: format!(
                                "{}{}",
                                d.display_name,
                                if d.is_current { " [CUR]" } else { " [OTHER]" }
                            ),
                        });
                    }
                    if targets.len() >= 3 {
                        break;
                    }
                }
                for w in &real_windows {
                    if targets.len() >= 3 {
                        break;
                    }
                    if !targets.iter().any(|t| t.hwnd.0 as isize == w.hwnd) {
                        let dname = w
                            .desktop_index
                            .and_then(|i| desktops.get(i))
                            .map(|d| d.display_name.clone())
                            .unwrap_or_else(|| "?".into());
                        targets.push(TargetEntry {
                            hwnd: HWND(w.hwnd as *mut c_void),
                            label: format!("{} - {}", w.process_name, w.title),
                            desktop_label: dname,
                        });
                    }
                }

                let dummy = if targets.is_empty() {
                    let d = DummySourceWindow::create().expect("create dummy window");
                    targets.push(TargetEntry {
                        hwnd: d.hwnd,
                        label: "Synthetic Dummy Source Window".into(),
                        desktop_label: "Local Session".into(),
                    });
                    Some(d)
                } else {
                    None
                };

                println!("=== REGISTERING DWM THUMBNAILS ({} targets) ===", targets.len());
                let mut slot_mgr = ThumbnailSlots::new(gpui_hwnd, targets);
                slot_mgr.register_all();

                let focus_handle = cx.focus_handle();
                window.focus(&focus_handle);

                let view = cx.new(|_| ProbeBView {
                    focus_handle,
                    gpui_hwnd,
                    slots: Rc::new(RefCell::new(slot_mgr)),
                    _dummy: dummy,
                    bg_mode_name: "Blurred (Acrylic)",
                    scroll_offset_px: 0.0,
                    status_line: "Ready. Observe Slot 1 (magenta vs live) & Slot 2 (badge vs live).".into(),
                    dcomp_disabled,
                });

                if self_test {
                    let view_clone = view.clone();
                    cx.spawn(async move |cx| {
                        // Let initial frame render
                        cx.background_executor().timer(Duration::from_millis(250)).await;
                        let _ = cx.update(|cx| {
                            view_clone.update(cx, |v, cx| {
                                println!("\n=== PROBE B AUTOMATED SELF-TEST ===");
                                let _ = v.run_50_cycle_stress();

                                // Measure hidden idle CPU over 2.0 seconds
                                let visible_after_hide =
                                    unsafe { IsWindowVisible(v.gpui_hwnd) }.as_bool();
                                println!("IsWindowVisible after SW_HIDE: {visible_after_hide} (expect false)");
                                let r0 = capture_process_resources();
                                let t0 = Instant::now();
                                thread::sleep(Duration::from_millis(2000));
                                let r1 = capture_process_resources();
                                let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
                                let cpu_delta = r1.cpu_time_ms - r0.cpu_time_ms;
                                println!(
                                    "[HIDDEN IDLE 2s] wall={:.1}ms cpu_delta={:.2}ms ({:.2}% of 1 core) | GDI={} USER={} Handles={}",
                                    wall_ms,
                                    cpu_delta,
                                    (cpu_delta / wall_ms) * 100.0,
                                    r1.gdi_objects,
                                    r1.user_objects,
                                    r1.kernel_handles
                                );
                                v.slots.borrow_mut().unregister_all();
                                cx.quit();
                            });
                        });
                    })
                    .detach();
                }

                view
            },
        );

        if let Err(e) = window_res {
            eprintln!("Failed to open GPUI window: {e:#}");
            cx.quit();
        }
    });

    Ok(())
}
