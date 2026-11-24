use std::process::Command;
use std::time::Instant;

use gpui::{
    Context, Div, MouseButton, ScrollDelta, ScrollWheelEvent, SharedString, Window, div,
    prelude::*, px, rgb, rgba,
};
use win_platform::{
    BackdropStyle, HotkeySpec, OverlayMonitorPolicy, PrimaryModifier,
    configure_borderless_immersive, is_autostart_enabled, set_autostart_enabled,
    show_and_focus_layered_overlay,
};

use crate::config::{
    AppConfig, ColumnsConfig, LabelPosition, LocalShortcut, PreviewFit, append_log, config_dir,
};
use crate::switcher_view::SwitcherView;
use crate::update_dialog::UpdateStatus;

const SHEET_W: f32 = 500.0;
const SHEET_CONTENT_EST_H: f32 = 880.0;

/// Minimalist 3-slider icon matching the hairline weight of the top Spaces bar.
fn sliders_icon() -> Div {
    let row = |dot_left: f32| {
        div()
            .relative()
            .w(px(14.0))
            .h(px(4.0))
            .flex()
            .items_center()
            .child(
                div()
                    .w(px(14.0))
                    .h(px(1.2))
                    .rounded_full()
                    .bg(rgba(0xFFFFFFB8)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(dot_left))
                    .size(px(3.8))
                    .rounded_full()
                    .bg(rgba(0xFFFFFFF0)),
            )
    };

    div()
        .w(px(14.0))
        .h(px(13.0))
        .flex()
        .flex_col()
        .items_center()
        .justify_between()
        .child(row(2.5))
        .child(row(8.0))
        .child(row(4.5))
}

fn row_divider() -> Div {
    div().w_full().h(px(1.0)).bg(rgba(0xFFFFFF0D))
}

fn setting_row(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .w_full()
        .min_h(px(42.0))
        .py(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgba(0xFFFFFFCC))
                .child(label),
        )
        .child(control)
}

fn section_header(title: &'static str) -> Div {
    div()
        .w_full()
        .pt(px(12.0))
        .pb(px(4.0))
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgba(0xFFFFFF66))
        .child(title)
}

fn setting_row_with_hint(
    label: &'static str,
    hint: &'static str,
    control: impl IntoElement,
) -> Div {
    div()
        .w_full()
        .min_h(px(52.0))
        .py(px(8.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgba(0xFFFFFFCC))
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgba(0xFFFFFF6E))
                        .child(hint),
                ),
        )
        .child(div().flex_shrink_0().child(control))
}

/// Quiet frosted pill container matching the top Spaces capsule bar (`rgba(0xFFFFFF0E)`).
fn pill_group() -> Div {
    div()
        .p(px(2.5))
        .rounded_full()
        .bg(rgba(0xFFFFFF0D))
        .border_1()
        .border_color(rgba(0xFFFFFF12))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
}

/// Frosted white active pill — identical visual language to `[ 全部 ] [ 桌面 1 ]` in the top bar.
fn glass_pill(
    id: &'static str,
    label: &'static str,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(px(22.0))
        .px(px(10.0))
        .rounded_full()
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .bg(if active {
            rgba(0xFFFFFF2E)
        } else {
            rgba(0x00000000)
        })
        .hover(|s| {
            s.bg(if active {
                rgba(0xFFFFFF38)
            } else {
                rgba(0xFFFFFF16)
            })
        })
        .on_click(on_click)
        .child(
            div()
                .text_xs()
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .text_color(if active {
                    rgba(0xFFFFFFFA)
                } else {
                    rgba(0xFFFFFF88)
                })
                .child(label),
        )
}

/// Ultra-quiet slim switch using the same frosted glass + subtle Apple blue accent as the Spaces dot.
fn minimal_switch(
    id: &'static str,
    checked: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .w(px(34.0))
        .h(px(19.0))
        .p(px(2.0))
        .rounded_full()
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_center()
        .bg(if checked {
            rgba(0x0A84FFCC)
        } else {
            rgba(0xFFFFFF1C)
        })
        .border_1()
        .border_color(if checked {
            rgba(0xFFFFFF28)
        } else {
            rgba(0xFFFFFF14)
        })
        .hover(|s| {
            s.bg(if checked {
                rgba(0x0A84FFE6)
            } else {
                rgba(0xFFFFFF2A)
            })
        })
        .on_click(on_click)
        .child(
            div()
                .ml(px(if checked { 14.0 } else { 0.0 }))
                .size(px(13.0))
                .rounded_full()
                .bg(if checked {
                    rgb(0xFFFFFF).into()
                } else {
                    rgba(0xFFFFFFCC)
                }),
        )
}

fn mini_step(
    id: &'static str,
    symbol: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .size(px(21.0))
        .rounded_full()
        .cursor_pointer()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0xFFFFFF10))
        .hover(|s| s.bg(rgba(0xFFFFFF26)))
        .on_click(on_click)
        .child(
            div()
                .text_xs()
                .text_color(rgba(0xFFFFFFDD))
                .child(symbol),
        )
}

impl SwitcherView {
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            self.show_and_refresh(window, cx);
        }
        self.disarm_hold_mode();
        self.clear_all_animations(false);
        self.armed_delete_desktop = None;
        // Clear live DWM thumbnails while in Settings so Layer 1 exposes pure, unobstructed Gaussian blur
        self.thumbs.borrow_mut().unregister_all();
        self.settings_open = true;
        self.recording_hotkey_letter = false;
        self.recording_force_quit_key = false;
        self.settings_error = None;
        self.win_arrow_patch_error = None;
        self.settings_scroll_y = 0.0;
        self.autostart_enabled = is_autostart_enabled();
        window.focus(&self.focus_handle);
        window.refresh();
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_open {
            return;
        }
        self.settings_open = false;
        self.recording_hotkey_letter = false;
        self.recording_force_quit_key = false;
        self.settings_error = None;
        self.win_arrow_patch_error = None;
        // Immediately restore live DWM thumbnails for the Mission Control grid
        self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
        self.clamp_scroll();
        window.refresh();
        cx.notify();
    }

    pub(crate) fn persist_config(&self) {
        if let Err(e) = self.config.save() {
            append_log(&format!("CONFIG SAVE ERROR: {e}"));
        }
    }

    pub(crate) fn apply_hotkey_change(
        &mut self,
        primary: PrimaryModifier,
        letter: char,
        cx: &mut Context<Self>,
    ) {
        let spec_str = format!("{}+{}", primary.display_name(), letter.to_ascii_uppercase());
        let Ok(spec) = HotkeySpec::parse(&spec_str) else {
            self.settings_error = Some(SharedString::from("快捷键须为 Ctrl / Alt / Win + 字母"));
            cx.notify();
            return;
        };

        let rebind_res = if let Some(ref host) = *self.shell_host {
            host.rebind_hotkey(spec)
        } else {
            Ok(())
        };

        match rebind_res {
            Ok(()) => {
                let canonical = spec.canonical_label();
                self.config.hotkey = canonical.clone();
                self.hotkey_warning = None;
                self.settings_error = None;
                self.recording_hotkey_letter = false;
                self.persist_config();
                append_log(&format!("HOTKEY UPDATED -> {canonical}"));
            }
            Err(e) => {
                self.recording_hotkey_letter = false;
                self.settings_error = Some(SharedString::from(format!("{e}")));
                append_log(&format!("HOTKEY REBIND FAILED -> {spec_str}: {e:#}"));
            }
        }
        cx.notify();
    }

    pub(crate) fn apply_force_quit_key_change(
        &mut self,
        shortcut: LocalShortcut,
        cx: &mut Context<Self>,
    ) {
        let label = shortcut.canonical_label();
        self.config.force_quit_key = label.clone();
        self.recording_force_quit_key = false;
        self.settings_error = None;
        self.persist_config();
        append_log(&format!("FORCE_QUIT_KEY UPDATED -> {label}"));
        cx.notify();
    }

    pub(crate) fn apply_win_arrow_patch_change(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let patch_res = if let Some(ref host) = *self.shell_host {
            host.set_win_arrow_patch_enabled(enabled)
        } else {
            Ok(())
        };

        match patch_res {
            Ok(()) => {
                self.config.win_arrow_switch_desktop = enabled;
                self.win_arrow_patch_error = None;
                self.persist_config();
                append_log(&format!("WIN_ARROW_PATCH UPDATED -> enabled={enabled}"));
                let toast = if enabled {
                    "已启用：关闭面板后按 Win + ←/→ 切换虚拟桌面"
                } else {
                    "已关闭：已恢复系统默认 Win + ←/→ 分屏"
                };
                self.transient_toast = Some((SharedString::from(toast), Instant::now()));
            }
            Err(e) => {
                self.win_arrow_patch_error = Some(SharedString::from(format!("{e}")));
                append_log(&format!(
                    "WIN_ARROW_PATCH UPDATE FAILED -> enabled={enabled}: {e:#}"
                ));
            }
        }
        cx.notify();
    }

    pub(crate) fn apply_visual_and_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.thumb_host.configure(
            self.config.immersive,
            self.config.background,
            self.config.blur_opacity,
        );
        self.persist_config();
        self.clamp_scroll();
        window.refresh();
        cx.notify();
    }

    pub(crate) fn apply_geometry_and_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        configure_borderless_immersive(self.gpui_hwnd, self.config.immersive);
        if let Ok((phys_rc, scale)) = show_and_focus_layered_overlay(
            self.gpui_hwnd,
            self.thumb_host.hwnd(),
            self.config.monitor,
            self.config.immersive,
        ) {
            self.overlay_phys_rect = phys_rc;
            self.overlay_scale = scale;
            self.last_overlay_rect = Some(phys_rc);
            let logical_w = ((phys_rc.right - phys_rc.left) as f32 / scale).max(640.0);
            let logical_h = ((phys_rc.bottom - phys_rc.top) as f32 / scale).max(420.0);
            self.viewport_width_px.set(logical_w);
            self.viewport_height_px.set((logical_h - 58.0).max(260.0));
        }
        self.thumb_host.configure(
            self.config.immersive,
            self.config.background,
            self.config.blur_opacity,
        );
        window.focus(&self.focus_handle);
        self.persist_config();
        self.clamp_scroll();
        window.refresh();
        cx.notify();
    }

    fn reset_all_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut defaults = AppConfig::default();
        let default_spec = defaults.parsed_hotkey();
        if let Some(ref host) = *self.shell_host {
            if let Err(e) = host.rebind_hotkey(default_spec) {
                defaults.hotkey = self.config.hotkey.clone();
                self.settings_error = Some(SharedString::from(format!("{e}")));
            } else {
                self.settings_error = None;
            }
            if let Err(e) = host.set_win_arrow_patch_enabled(defaults.win_arrow_switch_desktop) {
                defaults.win_arrow_switch_desktop = self.config.win_arrow_switch_desktop;
                self.win_arrow_patch_error = Some(SharedString::from(format!("{e}")));
            } else {
                self.win_arrow_patch_error = None;
            }
        } else {
            self.win_arrow_patch_error = None;
        }
        self.config = defaults;
        self.recording_hotkey_letter = false;
        self.recording_force_quit_key = false;
        self.apply_geometry_and_save(window, cx);
        self.transient_toast = Some((SharedString::from("已恢复默认"), Instant::now()));
        cx.notify();
    }

    /// Quiet circular frosted icon button in the bottom-right corner — matches top Spaces bar glass.
    pub(crate) fn render_settings_trigger(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        div()
            .id("windeck-settings-trigger")
            .absolute()
            .bottom(px(22.0))
            .right(px(24.0))
            .size(px(34.0))
            .rounded_full()
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0B0F1866))
            .border_1()
            .border_color(rgba(0xFFFFFF18))
            .hover(|s| s.bg(rgba(0xFFFFFF22)).border_color(rgba(0xFFFFFF30)))
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.open_settings(window, cx);
            }))
            .child(sliders_icon())
    }

    /// Centered minimalist translucent glass Settings view floating directly over real Acrylic blur.
    pub(crate) fn render_settings_view(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let hk = self.config.parsed_hotkey();
        let cur_primary = hk.primary;
        let cur_letter = char::from_u32(hk.vk).unwrap_or('Q');
        let recording = self.recording_hotkey_letter;
        let recording_fq = self.recording_force_quit_key;
        let fq_label = self.config.parsed_force_quit_key().canonical_label();
        let autostart = self.autostart_enabled;
        let label_pos = self.config.label_position;
        let preview_fit = self.config.preview_fit;
        let preview_w = self.config.preview_width.round() as i32;
        let preview_h = self.config.preview_height.round() as i32;
        let columns = self.config.columns;
        let background = self.config.background;
        let blur_pct = (self.config.blur_opacity * 100.0).round() as i32;
        let blur_ratio = ((self.config.blur_opacity - 0.05) / 0.80).clamp(0.0, 1.0);
        let immersive = self.config.immersive;
        let monitor = self.config.monitor;
        let cross_nav = self.config.cross_desktop_nav;
        let hide_on_blur = self.config.hide_on_blur;
        let auto_update = self.config.auto_update;
        let win_arrow_switch_desktop = self.config.win_arrow_switch_desktop;
        let cur_ver_tag = format!("v{}", self.effective_version);

        let (update_hint_text, update_hint_color, update_btn_label, update_has_new, update_checking) =
            match &self.update_status {
                UpdateStatus::Checking => (
                    "检查中...".to_string(),
                    rgba(0xFFFFFF88),
                    "检查中",
                    false,
                    true,
                ),
                UpdateStatus::UpToDate(tag) => (
                    format!("最新 ({tag})"),
                    rgba(0xFFFFFF78),
                    "检查更新",
                    false,
                    false,
                ),
                UpdateStatus::Available(rel) => (
                    format!("新版本 {}", rel.tag_name),
                    rgba(0x64D2FFFF),
                    "立即更新",
                    true,
                    false,
                ),
                UpdateStatus::Downloading { .. } => (
                    "正在更新...".to_string(),
                    rgba(0x64D2FFFF),
                    "查看进度",
                    true,
                    false,
                ),
                UpdateStatus::Error => (
                    "检查失败".to_string(),
                    rgba(0xFF8A80EE),
                    "重试",
                    false,
                    false,
                ),
                UpdateStatus::Idle => (cur_ver_tag, rgba(0xFFFFFF66), "检查更新", false, false),
            };

        let update_check_control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .text_xs()
                    .text_color(update_hint_color)
                    .child(update_hint_text),
            )
            .child(
                div()
                    .id("settings-check-update")
                    .h(px(26.0))
                    .px(px(12.0))
                    .rounded_full()
                    .when(!update_checking, |d| d.cursor_pointer())
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if update_has_new {
                        rgba(0x0A84FFD0)
                    } else {
                        rgba(0xFFFFFF18)
                    })
                    .border_1()
                    .border_color(if update_has_new {
                        rgba(0xFFFFFF30)
                    } else {
                        rgba(0xFFFFFF1C)
                    })
                    .when(!update_checking, |d| {
                        d.hover(|s| {
                            s.bg(if update_has_new {
                                rgba(0x0A84FFEC)
                            } else {
                                rgba(0xFFFFFF2A)
                            })
                        })
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if update_has_new && this.pending_update.is_some() {
                            this.open_update_dialog(cx);
                        } else {
                            this.trigger_update_check(true, cx);
                        }
                    }))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgba(0xFFFFFFF2))
                            .child(update_btn_label),
                    ),
            );

        let avail_h = (self.viewport_height_px.get() - 96.0).max(260.0);
        let max_scroll = (SHEET_CONTENT_EST_H - avail_h).max(0.0);
        let scroll_y = self.settings_scroll_y.clamp(0.0, max_scroll);

        // Hotkey row: [ Ctrl | Alt | Win ] + [ Q ]
        let hotkey_control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                pill_group()
                    .child(glass_pill(
                        "hk-mod-ctrl",
                        "Ctrl",
                        cur_primary == PrimaryModifier::Ctrl,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.apply_hotkey_change(PrimaryModifier::Ctrl, cur_letter, cx);
                        }),
                    ))
                    .child(glass_pill(
                        "hk-mod-alt",
                        "Alt",
                        cur_primary == PrimaryModifier::Alt,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.apply_hotkey_change(PrimaryModifier::Alt, cur_letter, cx);
                        }),
                    ))
                    .child(glass_pill(
                        "hk-mod-win",
                        "Win",
                        cur_primary == PrimaryModifier::Win,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.apply_hotkey_change(PrimaryModifier::Win, cur_letter, cx);
                        }),
                    )),
            )
            .child(
                div()
                    .id("hk-letter-record")
                    .h(px(27.0))
                    .min_w(px(if recording { 74.0 } else { 36.0 }))
                    .px(px(11.0))
                    .rounded_full()
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if recording {
                        rgba(0x0A84FFCC)
                    } else {
                        rgba(0xFFFFFF1C)
                    })
                    .border_1()
                    .border_color(if recording {
                        rgba(0xFFFFFF55)
                    } else {
                        rgba(0xFFFFFF1E)
                    })
                    .hover(|s| {
                        s.bg(if recording {
                            rgba(0x0A84FFE6)
                        } else {
                            rgba(0xFFFFFF2C)
                        })
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.recording_force_quit_key = false;
                        this.recording_hotkey_letter = !this.recording_hotkey_letter;
                        this.settings_error = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgba(0xFFFFFFFA))
                            .child(if recording {
                                "按字母".to_string()
                            } else {
                                cur_letter.to_string()
                            }),
                    ),
            );

        // Force-quit shortcut recorder pill (single key or modifier+key, e.g. Delete / Ctrl+W / Shift+Delete)
        let force_quit_control = div()
            .id("fq-key-record")
            .h(px(27.0))
            .min_w(px(if recording_fq { 92.0 } else { 64.0 }))
            .px(px(12.0))
            .rounded_full()
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .bg(if recording_fq {
                rgba(0x0A84FFCC)
            } else {
                rgba(0xFFFFFF1C)
            })
            .border_1()
            .border_color(if recording_fq {
                rgba(0xFFFFFF55)
            } else {
                rgba(0xFFFFFF1E)
            })
            .hover(|s| {
                s.bg(if recording_fq {
                    rgba(0x0A84FFE6)
                } else {
                    rgba(0xFFFFFF2C)
                })
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.recording_hotkey_letter = false;
                this.recording_force_quit_key = !this.recording_force_quit_key;
                this.settings_error = None;
                cx.notify();
            }))
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgba(0xFFFFFFFA))
                    .child(if recording_fq {
                        "请按键...".to_string()
                    } else {
                        fq_label
                    }),
            );

        // Label placement row
        let label_control = pill_group()
            .child(glass_pill(
                "lbl-br",
                "右下",
                label_pos == LabelPosition::BottomRight,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.label_position = LabelPosition::BottomRight;
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "lbl-bl",
                "左下",
                label_pos == LabelPosition::BottomLeft,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.label_position = LabelPosition::BottomLeft;
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "lbl-center",
                "居中",
                label_pos == LabelPosition::Center,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.label_position = LabelPosition::Center;
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "lbl-below",
                "下方",
                label_pos == LabelPosition::Below,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.label_position = LabelPosition::Below;
                    this.apply_visual_and_save(window, cx);
                }),
            ));

        // Fit mode row
        let fit_control = pill_group()
            .child(glass_pill(
                "fit-cover",
                "铺满",
                preview_fit == PreviewFit::Cover,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.preview_fit = PreviewFit::Cover;
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "fit-contain",
                "完整",
                preview_fit == PreviewFit::Contain,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.preview_fit = PreviewFit::Contain;
                    this.apply_visual_and_save(window, cx);
                }),
            ));

        // Preview size row: compact inline width × height steppers + presets
        let size_control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .child(mini_step(
                        "sz-w-dec",
                        "−",
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.config.preview_width =
                                (this.config.preview_width - 20.0).clamp(220.0, 680.0);
                            this.apply_visual_and_save(window, cx);
                        }),
                    ))
                    .child(
                        div()
                            .w(px(30.0))
                            .flex()
                            .justify_center()
                            .text_xs()
                            .text_color(rgba(0xFFFFFFDC))
                            .child(format!("{preview_w}")),
                    )
                    .child(mini_step(
                        "sz-w-inc",
                        "+",
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.config.preview_width =
                                (this.config.preview_width + 20.0).clamp(220.0, 680.0);
                            this.apply_visual_and_save(window, cx);
                        }),
                    )),
            )
            .child(div().text_xs().text_color(rgba(0xFFFFFF44)).child("×"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .child(mini_step(
                        "sz-h-dec",
                        "−",
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.config.preview_height =
                                (this.config.preview_height - 16.0).clamp(130.0, 440.0);
                            this.apply_visual_and_save(window, cx);
                        }),
                    ))
                    .child(
                        div()
                            .w(px(30.0))
                            .flex()
                            .justify_center()
                            .text_xs()
                            .text_color(rgba(0xFFFFFFDC))
                            .child(format!("{preview_h}")),
                    )
                    .child(mini_step(
                        "sz-h-inc",
                        "+",
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.config.preview_height =
                                (this.config.preview_height + 16.0).clamp(130.0, 440.0);
                            this.apply_visual_and_save(window, cx);
                        }),
                    )),
            );

        // Columns row
        let cols_control = pill_group()
            .child(glass_pill(
                "col-auto",
                "自动",
                columns == ColumnsConfig::Auto,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.columns = ColumnsConfig::Auto;
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "col-3",
                "3",
                columns == ColumnsConfig::Fixed(3),
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.columns = ColumnsConfig::Fixed(3);
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "col-4",
                "4",
                columns == ColumnsConfig::Fixed(4),
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.columns = ColumnsConfig::Fixed(4);
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "col-5",
                "5",
                columns == ColumnsConfig::Fixed(5),
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.columns = ColumnsConfig::Fixed(5);
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "col-6",
                "6",
                columns == ColumnsConfig::Fixed(6),
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.columns = ColumnsConfig::Fixed(6);
                    this.apply_visual_and_save(window, cx);
                }),
            ));

        // Backdrop style row
        let bg_control = pill_group()
            .child(glass_pill(
                "bg-blur",
                "毛玻璃",
                background == BackdropStyle::Blurred,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.background = BackdropStyle::Blurred;
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "bg-trans",
                "半透明",
                background == BackdropStyle::Transparent,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.background = BackdropStyle::Transparent;
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "bg-opaque",
                "暗幕",
                background == BackdropStyle::Opaque,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.background = BackdropStyle::Opaque;
                    this.apply_visual_and_save(window, cx);
                }),
            ));

        // Blur opacity ultra-thin track
        let blur_control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(mini_step(
                "blur-dec",
                "−",
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.blur_opacity =
                        (this.config.blur_opacity - 0.05).clamp(0.05, 0.85);
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(
                div()
                    .w(px(116.0))
                    .h(px(3.5))
                    .rounded_full()
                    .bg(rgba(0xFFFFFF1E))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(px(116.0 * blur_ratio))
                            .rounded_full()
                            .bg(rgba(0xFFFFFFCC)),
                    ),
            )
            .child(mini_step(
                "blur-inc",
                "+",
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.blur_opacity =
                        (this.config.blur_opacity + 0.05).clamp(0.05, 0.85);
                    this.apply_visual_and_save(window, cx);
                }),
            ))
            .child(
                div()
                    .w(px(32.0))
                    .flex()
                    .justify_end()
                    .text_xs()
                    .text_color(rgba(0xFFFFFF88))
                    .child(format!("{blur_pct}%")),
            );

        // Monitor policy row
        let monitor_control = pill_group()
            .child(glass_pill(
                "mon-cursor",
                "鼠标屏",
                monitor == OverlayMonitorPolicy::Cursor,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.monitor = OverlayMonitorPolicy::Cursor;
                    this.apply_geometry_and_save(window, cx);
                }),
            ))
            .child(glass_pill(
                "mon-primary",
                "主屏",
                monitor == OverlayMonitorPolicy::Primary,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.config.monitor = OverlayMonitorPolicy::Primary;
                    this.apply_geometry_and_save(window, cx);
                }),
            ));

        // Fullscreen transparent hit area over real Acrylic blur -> click anywhere outside returns to grid
        div()
            .id("windeck-settings-canvas")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(28.0))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_settings(window, cx);
                }),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.close_settings(window, cx);
            }))
            .child(
                // Single minimalist translucent glass sheet matching the top Spaces capsule material
                div()
                    .id("windeck-settings-sheet")
                    .w(px(SHEET_W))
                    .max_h_full()
                    .rounded(px(20.0))
                    .bg(rgba(0x0B0F186E))
                    .border_1()
                    .border_color(rgba(0xFFFFFF1C))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Right, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_scroll_wheel(cx.listener(
                        move |this, event: &ScrollWheelEvent, _, cx| {
                            cx.stop_propagation();
                            let dy = match event.delta {
                                ScrollDelta::Lines(pt) => -pt.y * 44.0,
                                ScrollDelta::Pixels(pt) => -f32::from(pt.y),
                            };
                            this.settings_scroll_y =
                                (this.settings_scroll_y + dy).clamp(0.0, max_scroll);
                            cx.notify();
                        },
                    ))
                    // Quiet minimalist header
                    .child(
                        div()
                            .h(px(50.0))
                            .flex_shrink_0()
                            .px(px(22.0))
                            .border_b_1()
                            .border_color(rgba(0xFFFFFF10))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgba(0xFFFFFFF2))
                                    .child("设置"),
                            )
                            .child(
                                div()
                                    .id("settings-close-x")
                                    .size(px(24.0))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(rgba(0xFFFFFF10))
                                    .hover(|s| s.bg(rgba(0xFFFFFF24)))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.close_settings(window, cx);
                                    }))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgba(0xFFFFFFCC))
                                            .child("×"),
                                    ),
                            ),
                    )
                    // Single clean scrollable list (no nested boxes, no loud chrome)
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_hidden()
                            .child(
                                div()
                                    .relative()
                                    .top(px(-scroll_y))
                                    .w_full()
                                    .px(px(22.0))
                                    .py(px(6.0))
                                    .flex()
                                    .flex_col()
                                    .child(setting_row("快捷键", hotkey_control))
                                    .when_some(self.settings_error.clone(), |el, err| {
                                        el.child(
                                            div()
                                                .pb(px(8.0))
                                                .text_xs()
                                                .text_color(rgb(0xFF8A80))
                                                .child(err),
                                        )
                                    })
                                    .child(row_divider())
                                    .child(setting_row("强制退出", force_quit_control))
                                    .child(row_divider())
                                    .child(setting_row("标签位置", label_control))
                                    .child(row_divider())
                                    .child(setting_row("画面适配", fit_control))
                                    .child(row_divider())
                                    .child(setting_row("预览尺寸", size_control))
                                    .child(row_divider())
                                    .child(setting_row("每行列数", cols_control))
                                    .child(row_divider())
                                    .child(setting_row("背景效果", bg_control))
                                    .child(row_divider())
                                    .child(setting_row("背景暗度", blur_control))
                                    .child(row_divider())
                                    .child(setting_row("弹出屏幕", monitor_control))
                                    .child(row_divider())
                                    .child(setting_row(
                                        "沉浸全屏",
                                        minimal_switch(
                                            "sw-immersive",
                                            immersive,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.config.immersive = !immersive;
                                                this.apply_geometry_and_save(window, cx);
                                            }),
                                        ),
                                    ))
                                    .child(row_divider())
                                    .child(setting_row(
                                        "跨桌面衔接",
                                        minimal_switch(
                                            "sw-cross-nav",
                                            cross_nav,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.config.cross_desktop_nav = !cross_nav;
                                                this.apply_visual_and_save(window, cx);
                                            }),
                                        ),
                                    ))
                                    .child(row_divider())
                                    .child(setting_row(
                                        "失焦隐藏",
                                        minimal_switch(
                                            "sw-hide-blur",
                                            hide_on_blur,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.config.hide_on_blur = !hide_on_blur;
                                                this.apply_visual_and_save(window, cx);
                                            }),
                                        ),
                                    ))
                                    .child(row_divider())
                                    .child(setting_row(
                                        "开机自启",
                                        minimal_switch(
                                            "sw-autostart",
                                            autostart,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                let next = !autostart;
                                                match set_autostart_enabled(next) {
                                                    Ok(()) => {
                                                        this.autostart_enabled = next;
                                                        this.settings_error = None;
                                                        append_log(&format!(
                                                            "AUTOSTART settings -> {next}"
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        this.settings_error = Some(
                                                            SharedString::from(format!(
                                                                "开机自启失败：{e}"
                                                            )),
                                                        );
                                                    }
                                                }
                                                cx.notify();
                                            }),
                                        ),
                                    ))
                                    .child(row_divider())
                                    .child(setting_row(
                                        "自动更新",
                                        minimal_switch(
                                            "sw-auto-update",
                                            auto_update,
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                let next = !auto_update;
                                                this.config.auto_update = next;
                                                this.apply_visual_and_save(window, cx);
                                                if next {
                                                    this.trigger_update_check(false, cx);
                                                }
                                            }),
                                        ),
                                    ))
                                    .child(row_divider())
                                    .child(setting_row("版本更新", update_check_control))
                                    .child(row_divider())
                                    .child(section_header("系统补丁"))
                                    .child(setting_row_with_hint(
                                        "Win + ←/→ 切虚拟桌面",
                                        "覆盖系统窗口分屏，改用原生动画切换虚拟桌面（管理员窗口不生效）",
                                        minimal_switch(
                                            "sw-win-arrow-desktop",
                                            win_arrow_switch_desktop,
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.apply_win_arrow_patch_change(
                                                    !win_arrow_switch_desktop,
                                                    cx,
                                                );
                                            }),
                                        ),
                                    ))
                                    .when_some(self.win_arrow_patch_error.clone(), |el, err| {
                                        el.child(
                                            div()
                                                .pb(px(8.0))
                                                .text_xs()
                                                .text_color(rgb(0xFF8A80))
                                                .child(err),
                                        )
                                    }),
                            ),
                    )
                    // Whisper-quiet footer links (no heavy buttons)
                    .child(
                        div()
                            .h(px(44.0))
                            .flex_shrink_0()
                            .px(px(22.0))
                            .border_t_1()
                            .border_color(rgba(0xFFFFFF10))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(14.0))
                                    .child(
                                        div()
                                            .id("settings-open-dir")
                                            .cursor_pointer()
                                            .text_xs()
                                            .text_color(rgba(0xFFFFFF6E))
                                            .hover(|s| s.text_color(rgba(0xFFFFFFDF)))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                cx.stop_propagation();
                                                let dir = config_dir();
                                                let _ = std::fs::create_dir_all(&dir);
                                                let _ = Command::new("explorer.exe").arg(dir).spawn();
                                                this.transient_toast = Some((
                                                    SharedString::from("已打开配置目录"),
                                                    Instant::now(),
                                                ));
                                                cx.notify();
                                            }))
                                            .child("配置文件"),
                                    )
                                    .child(
                                        div()
                                            .id("settings-reset")
                                            .cursor_pointer()
                                            .text_xs()
                                            .text_color(rgba(0xFFFFFF6E))
                                            .hover(|s| s.text_color(rgba(0xFFFFFFDF)))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.reset_all_defaults(window, cx);
                                            }))
                                            .child("恢复默认"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgba(0xFFFFFF44))
                                    .child("点击空白或按 Esc 返回"),
                            ),
                    ),
            )
    }
}
