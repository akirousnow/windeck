use std::process::Command;
use std::time::{Duration, Instant};

use gpui::{Context, Div, MouseButton, SharedString, Window, div, prelude::*, px, rgb, rgba};
use win_platform::{
    GITHUB_RELEASES_PAGE, ReleaseInfo, UpdateCheckResult, check_latest_release,
    download_and_stage_update,
};

use crate::config::append_log;
use crate::switcher_view::SwitcherView;

const DIALOG_W: f32 = 430.0;
const AUTO_CHECK_MIN_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate(String),
    Available(ReleaseInfo),
    Downloading {
        downloaded: u64,
        total: Option<u64>,
    },
    Error,
}

enum DownloadEvent {
    Progress(u64, Option<u64>),
    Done(std::path::PathBuf),
    Failed(String),
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_release_notes_preview(raw: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cleaned = trimmed
            .trim_start_matches('#')
            .trim()
            .replace("**", "")
            .replace('`', "");
        if cleaned.is_empty() {
            continue;
        }
        lines.push(cleaned);
        if lines.len() >= 8 {
            break;
        }
    }
    lines
}

fn open_url_in_browser(url: &str) {
    let target = if url.starts_with("https://") {
        url
    } else {
        GITHUB_RELEASES_PAGE
    };
    let _ = Command::new("explorer.exe").arg(target).spawn();
}

impl SwitcherView {
    pub fn is_downloading_update(&self) -> bool {
        matches!(self.update_status, UpdateStatus::Downloading { .. })
    }

    pub fn open_update_dialog_from_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            self.show_and_refresh(window, cx);
        }
        if self.pending_update.is_some() {
            self.open_update_dialog(cx);
            window.focus(&self.focus_handle);
            window.refresh();
        } else {
            self.trigger_update_check(true, cx);
        }
    }

    pub fn check_update_from_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            self.open_settings(window, cx);
        }
        self.trigger_update_check(true, cx);
    }

    pub(crate) fn open_update_dialog(&mut self, cx: &mut Context<Self>) {
        if self.pending_update.is_none() {
            return;
        }
        self.disarm_hold_mode();
        self.armed_delete_desktop = None;
        self.recording_hotkey_letter = false;
        self.recording_force_quit_key = false;
        // Clear live DWM thumbnails so Layer 1 exposes clean Gaussian blur behind the modal dialog
        self.thumbs.borrow_mut().unregister_all();
        self.update_dialog_open = true;
        self.update_dialog_error = None;
        cx.notify();
    }

    pub(crate) fn dismiss_update_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.update_dialog_open || self.is_downloading_update() {
            return;
        }
        if let Some(ref rel) = self.pending_update {
            self.dismissed_update_tag = Some(rel.tag_name.clone());
        }
        self.update_dialog_open = false;
        self.update_dialog_error = None;
        if !self.settings_open {
            self.thumbs.borrow_mut().sync_targets(&self.flat_hwnds);
            self.clamp_scroll();
        }
        window.refresh();
        cx.notify();
    }

    /// Queries GitHub Releases in a background thread (zero UI blocking).
    /// When `manual == false`, sends `If-None-Match` ETag so `304 Not Modified` costs 0 API quota.
    pub fn trigger_update_check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.is_downloading_update() || self.update_check_in_flight {
            return;
        }
        if !manual {
            if !self.config.auto_update {
                return;
            }
            if self
                .last_update_check
                .is_some_and(|t| t.elapsed() < AUTO_CHECK_MIN_INTERVAL)
            {
                return;
            }
        } else {
            self.dismissed_update_tag = None;
        }

        self.update_check_in_flight = true;
        self.last_update_check = Some(Instant::now());
        if manual || !matches!(self.update_status, UpdateStatus::Available(_)) {
            self.update_status = UpdateStatus::Checking;
            cx.notify();
        }

        let current_ver = self.effective_version.clone();
        let cached_etag = if manual {
            None
        } else {
            self.update_etag.clone()
        };

        let (tx, rx) = async_channel::bounded(1);
        std::thread::Builder::new()
            .name("windeck-update-check".to_string())
            .spawn(move || {
                let res = check_latest_release(&current_ver, cached_etag.as_deref());
                let _ = tx.send_blocking(res);
            })
            .ok();

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else {
                let _ = this.update(cx, |v, cx| {
                    v.update_check_in_flight = false;
                    cx.notify();
                });
                return;
            };

            let _ = this.update(cx, |v, cx| {
                v.update_check_in_flight = false;
                match result {
                    Ok(UpdateCheckResult::UpdateAvailable { release, etag }) => {
                        if etag.is_some() {
                            v.update_etag = etag;
                        }
                        let tag = release.tag_name.clone();
                        v.pending_update = Some(release.clone());
                        v.update_status = UpdateStatus::Available(release);
                        append_log(&format!(
                            "UPDATE_CHECK available current={} latest={} manual={manual}",
                            v.effective_version, tag
                        ));

                        let should_prompt =
                            manual || v.dismissed_update_tag.as_deref() != Some(tag.as_str());
                        if should_prompt {
                            if v.visible && !(v.hold_active && v.hold_navigated) {
                                v.open_update_dialog(cx);
                            } else if !v.visible
                                && v.notified_balloon_tag.as_deref() != Some(tag.as_str())
                            {
                                v.notified_balloon_tag = Some(tag.clone());
                                if let Some(ref host) = *v.shell_host {
                                    host.show_balloon(
                                        "WinDeck 发现新版本",
                                        format!("{tag} 已发布，点击通知或打开 WinDeck 立即更新"),
                                    );
                                }
                            }
                        }
                    }
                    Ok(UpdateCheckResult::NotModified) => {
                        append_log("UPDATE_CHECK 304 Not Modified");
                        if let Some(release) = v.pending_update.clone() {
                            let tag = release.tag_name.clone();
                            v.update_status = UpdateStatus::Available(release);
                            let should_prompt =
                                manual || v.dismissed_update_tag.as_deref() != Some(tag.as_str());
                            if should_prompt && v.visible && !(v.hold_active && v.hold_navigated) {
                                v.open_update_dialog(cx);
                            }
                        } else {
                            v.update_status =
                                UpdateStatus::UpToDate(format!("v{}", v.effective_version));
                        }
                    }
                    Ok(UpdateCheckResult::UpToDate { latest_tag, etag }) => {
                        if etag.is_some() {
                            v.update_etag = etag;
                        }
                        v.pending_update = None;
                        v.update_status = UpdateStatus::UpToDate(latest_tag.clone());
                        append_log(&format!(
                            "UPDATE_CHECK up_to_date current={} latest={} manual={manual}",
                            v.effective_version, latest_tag
                        ));
                        if manual {
                            v.transient_toast = Some((
                                SharedString::from(format!("当前已是最新版本 ({latest_tag})")),
                                Instant::now(),
                            ));
                        }
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        append_log(&format!("UPDATE_CHECK ERROR manual={manual}: {e:#}"));
                        if manual {
                            v.update_status = UpdateStatus::Error;
                            v.transient_toast = Some((
                                SharedString::from(format!("检查更新失败：{msg}")),
                                Instant::now(),
                            ));
                        } else if matches!(v.update_status, UpdateStatus::Checking) {
                            v.update_status = UpdateStatus::Idle;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn start_self_update(&mut self, cx: &mut Context<Self>) {
        if self.is_downloading_update() {
            return;
        }
        let Some(release) = self.pending_update.clone() else {
            return;
        };

        if release.download_url.is_none() {
            open_url_in_browser(&release.html_url);
            self.transient_toast = Some((
                SharedString::from("已在浏览器打开 GitHub Release 页面"),
                Instant::now(),
            ));
            cx.notify();
            return;
        }

        self.update_dialog_error = None;
        self.update_status = UpdateStatus::Downloading {
            downloaded: 0,
            total: release.asset_size,
        };
        cx.notify();

        let (tx, rx) = async_channel::unbounded::<DownloadEvent>();
        let rel_for_thread = release.clone();
        std::thread::Builder::new()
            .name("windeck-updater-dl".to_string())
            .spawn(move || {
                let mut last_sent = Instant::now() - Duration::from_secs(1);
                let tx_prog = tx.clone();
                let res = download_and_stage_update(&rel_for_thread, |done, total| {
                    if last_sent.elapsed() >= Duration::from_millis(75)
                        || total.is_some_and(|t| done >= t)
                    {
                        last_sent = Instant::now();
                        let _ = tx_prog.send_blocking(DownloadEvent::Progress(done, total));
                    }
                });
                match res {
                    Ok(exe_path) => {
                        let _ = tx.send_blocking(DownloadEvent::Done(exe_path));
                    }
                    Err(e) => {
                        let _ = tx.send_blocking(DownloadEvent::Failed(format!("{e}")));
                    }
                }
            })
            .ok();

        cx.spawn(async move |this, cx| {
            while let Ok(ev) = rx.recv().await {
                let mut should_quit = false;
                let _ = this.update(cx, |v, cx| match ev {
                    DownloadEvent::Progress(downloaded, total) => {
                        v.update_status = UpdateStatus::Downloading { downloaded, total };
                        cx.notify();
                    }
                    DownloadEvent::Done(exe_path) => {
                        append_log(&format!(
                            "UPDATE_APPLIED version={} relaunch='{}'",
                            release.tag_name,
                            exe_path.display()
                        ));
                        v.hide(cx);
                        // Release single-instance lock so the updated WinDeck acquires it immediately
                        if let Ok(mut guard_slot) = v.single_instance_guard.lock() {
                            if let Some(ref mut guard) = *guard_slot {
                                guard.release();
                            }
                            *guard_slot = None;
                        }
                        let pid = std::process::id().to_string();
                        if let Err(e) = Command::new(&exe_path).args(["--wait-pid", &pid]).spawn() {
                            append_log(&format!("UPDATE_RELAUNCH ERROR: {e}"));
                            v.update_status = UpdateStatus::Available(release.clone());
                            v.update_dialog_open = true;
                            v.update_dialog_error = Some(SharedString::from(format!(
                                "更新已写入，但自动重启失败：{e}，请手动重新打开 WinDeck"
                            )));
                            cx.notify();
                        } else {
                            should_quit = true;
                        }
                    }
                    DownloadEvent::Failed(msg) => {
                        append_log(&format!("UPDATE_DOWNLOAD ERROR: {msg}"));
                        v.update_status = UpdateStatus::Available(release.clone());
                        v.update_dialog_error = Some(SharedString::from(msg));
                        cx.notify();
                    }
                });

                if should_quit {
                    let _ = cx.update(|cx| cx.quit());
                    break;
                }
            }
        })
        .detach();
    }

    /// Minimalist translucent frosted-glass update confirmation modal matching WinDeck's design language.
    pub(crate) fn render_update_dialog(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let Some(release) = self.pending_update.clone() else {
            return div().id("windeck-update-empty");
        };

        let cur_ver_label = format!("v{}", self.effective_version);
        let new_tag = release.tag_name.clone();
        let html_url = release.html_url.clone();
        let notes = format_release_notes_preview(&release.body);
        let size_text = release.asset_size.map(format_bytes);

        let (downloading, progress_ratio, progress_label) = match &self.update_status {
            UpdateStatus::Downloading {
                downloaded, total, ..
            } => {
                let (ratio, label) = if let Some(t) = *total {
                    if t > 0 {
                        let r = (*downloaded as f32 / t as f32).clamp(0.0, 1.0);
                        let pct = (r * 100.0).round() as u32;
                        (
                            r,
                            format!(
                                "正在下载更新... {pct}% ({} / {})",
                                format_bytes(*downloaded),
                                format_bytes(t)
                            ),
                        )
                    } else {
                        (0.15, format!("正在下载更新... {}", format_bytes(*downloaded)))
                    }
                } else {
                    (0.25, format!("正在下载更新... {}", format_bytes(*downloaded)))
                };
                (true, ratio, Some(label))
            }
            _ => (false, 0.0, None),
        };

        let mut notes_box = div().flex().flex_col().gap(px(5.0));
        if notes.is_empty() {
            notes_box = notes_box.child(
                div()
                    .text_xs()
                    .text_color(rgba(0xFFFFFFA6))
                    .child("GitHub Release 已发布新版本，点击「立即更新」自动下载并重启生效。"),
            );
        } else {
            for line in notes {
                notes_box = notes_box.child(
                    div()
                        .text_xs()
                        .text_color(rgba(0xFFFFFFB8))
                        .child(line),
                );
            }
        }

        div()
            .id("windeck-update-canvas")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(28.0))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.dismiss_update_dialog(window, cx);
                }),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.dismiss_update_dialog(window, cx);
            }))
            .child(
                div()
                    .id("windeck-update-sheet")
                    .w(px(DIALOG_W))
                    .rounded(px(20.0))
                    .bg(rgba(0x0B0F1878))
                    .border_1()
                    .border_color(rgba(0xFFFFFF20))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Right, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                    })
                    // Header
                    .child(
                        div()
                            .h(px(50.0))
                            .px(px(22.0))
                            .border_b_1()
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
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .size(px(7.0))
                                            .rounded_full()
                                            .bg(rgb(0x0A84FF)),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(rgba(0xFFFFFFF2))
                                            .child("发现新版本"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgba(0xFFFFFF88))
                                    .child(format!("{cur_ver_label}  →  {new_tag}")),
                            ),
                    )
                    // Body
                    .child(
                        div()
                            .w_full()
                            .px(px(22.0))
                            .py(px(16.0))
                            .flex()
                            .flex_col()
                            .gap(px(12.0))
                            .child(notes_box)
                            .when_some(size_text, |el, sz| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgba(0xFFFFFF5C))
                                        .child(format!("安装包：windeck.exe · {sz}")),
                                )
                            })
                            .when_some(progress_label, |el, label| {
                                el.child(
                                    div()
                                        .pt(px(2.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(6.0))
                                        .child(
                                            div()
                                                .w_full()
                                                .h(px(3.5))
                                                .rounded_full()
                                                .bg(rgba(0xFFFFFF1E))
                                                .overflow_hidden()
                                                .child(
                                                    div()
                                                        .h_full()
                                                        .w(px((DIALOG_W - 44.0) * progress_ratio))
                                                        .rounded_full()
                                                        .bg(rgb(0x0A84FF)),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgba(0xFFFFFFB8))
                                                .child(label),
                                        ),
                                )
                            })
                            .when_some(self.update_dialog_error.clone(), |el, err| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0xFF8A80))
                                        .child(err),
                                )
                            }),
                    )
                    // Footer actions
                    .child(
                        div()
                            .h(px(50.0))
                            .px(px(22.0))
                            .border_t_1()
                            .border_color(rgba(0xFFFFFF10))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .id("update-open-release")
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(rgba(0xFFFFFF6E))
                                    .hover(|s| s.text_color(rgba(0xFFFFFFDF)))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        open_url_in_browser(&html_url);
                                    })
                                    .child("更新日志"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .when(!downloading, |el| {
                                        el.child(
                                            div()
                                                .id("update-btn-later")
                                                .h(px(28.0))
                                                .px(px(14.0))
                                                .rounded_full()
                                                .cursor_pointer()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .bg(rgba(0xFFFFFF14))
                                                .border_1()
                                                .border_color(rgba(0xFFFFFF16))
                                                .hover(|s| s.bg(rgba(0xFFFFFF24)))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.dismiss_update_dialog(window, cx);
                                                }))
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .font_weight(gpui::FontWeight::MEDIUM)
                                                        .text_color(rgba(0xFFFFFFCC))
                                                        .child("稍后"),
                                                ),
                                        )
                                    })
                                    .child(
                                        div()
                                            .id("update-btn-install")
                                            .h(px(28.0))
                                            .px(px(16.0))
                                            .rounded_full()
                                            .when(!downloading, |d| d.cursor_pointer())
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .bg(if downloading {
                                                rgba(0x0A84FF77)
                                            } else {
                                                rgba(0x0A84FFD8)
                                            })
                                            .border_1()
                                            .border_color(rgba(0xFFFFFF28))
                                            .when(!downloading, |d| {
                                                d.hover(|s| s.bg(rgba(0x0A84FFF2)))
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.start_self_update(cx);
                                            }))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                                    .text_color(rgb(0xFFFFFF))
                                                    .child(if downloading {
                                                        "下载中..."
                                                    } else if self.update_dialog_error.is_some() {
                                                        "重试更新"
                                                    } else {
                                                        "立即更新"
                                                    }),
                                            ),
                                    ),
                            ),
                    ),
            )
    }
}
