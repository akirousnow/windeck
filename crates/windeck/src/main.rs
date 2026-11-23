#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod nav;
mod switcher_view;
mod thumb_manager;

use std::ffi::c_void;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gpui::{
    App, Application, Bounds, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, prelude::*, px, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use win_platform::{
    ShellEvent, ShellHost, SingleInstanceGuard, ThumbnailHostWindow, VirtualDesktopClient,
    configure_borderless_immersive, current_session_desktop_names, notify_existing_instance,
};
use windows::Win32::Foundation::HWND;

use crate::config::{AppConfig, append_log, config_dir};
use crate::switcher_view::SwitcherView;

fn extract_hwnd(window: &Window) -> Result<HWND> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|e| anyhow::anyhow!("HasWindowHandle failed: {e}"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Ok(HWND(h.hwnd.get() as *mut c_void)),
        other => anyhow::bail!("Expected Win32 handle, got {:?}", other),
    }
}

fn attach_parent_console_if_needed() {
    #[cfg(not(debug_assertions))]
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|a| a == "--hidden");
    let bench_mode = args.iter().any(|a| a == "--bench");

    if bench_mode {
        attach_parent_console_if_needed();
    }

    // Single-instance guard: prevent duplicate tray icons & hotkey conflicts.
    // When launched a second time interactively, wake the already-running instance and exit cleanly.
    let _single_instance_guard = if bench_mode {
        None
    } else {
        match SingleInstanceGuard::try_acquire()? {
            Some(guard) => Some(guard),
            None => {
                if !start_hidden {
                    let woke = notify_existing_instance();
                    append_log(&format!(
                        "SECOND_INSTANCE detected -> notified existing instance (woke={woke}), exiting"
                    ));
                }
                return Ok(());
            }
        }
    };

    let config = AppConfig::load_or_create();
    let (winsta, desk) = current_session_desktop_names();
    append_log(&format!(
        "STARTUP hotkey='{}' immersive={} label='{}' desktop='{}\\{}' hidden={} bench={}",
        config.hotkey,
        config.immersive,
        config.label_position.as_config_str(),
        winsta,
        desk,
        start_hidden,
        bench_mode
    ));

    let vdm = Arc::new(VirtualDesktopClient::new()?);
    let (event_tx, event_rx) = async_channel::unbounded::<ShellEvent>();

    let hotkey_spec = config.parsed_hotkey();
    let shell_host = if bench_mode {
        None
    } else {
        let tx = event_tx.clone();
        match ShellHost::start(hotkey_spec, config.hotkey.clone(), move |ev| {
            let _ = tx.send_blocking(ev);
        }) {
            Ok(host) => Some(host),
            Err(e) => {
                append_log(&format!("ShellHost start failed: {e:#}"));
                None
            }
        }
    };

    let hotkey_warning = shell_host.as_ref().and_then(|h| {
        if h.hotkey_registered() {
            None
        } else {
            Some(format!(
                "热键 {} 注册失败（可能被其他程序占用，可在托盘修改配置）",
                config.hotkey
            ))
        }
    });

    if let Some(ref warn) = hotkey_warning {
        append_log(warn);
    }

    let shell_host = Arc::new(shell_host);
    let initial_immersive = config.immersive;
    let initial_backdrop = config.background;
    let initial_blur_opacity = config.blur_opacity;

    Application::new().run(move |cx: &mut App| {
        let initial_bounds = Bounds::centered(None, size(px(1440.0), px(860.0)), cx);
        let vdm_clone = vdm.clone();
        let config_clone = config.clone();
        let shell_for_loop = shell_host.clone();

        // Two-layer compositing:
        // - Top layer (this GPUI window): per-pixel transparent DirectComposition interactive UI + embedded badges
        // - Bottom layer (ThumbnailHostWindow): frosted Win11 Acrylic backdrop + live DWM thumbnails
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(initial_bounds)),
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                focus: false,
                show: false,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |window, cx| {
                let gpui_hwnd = extract_hwnd(window).expect("extract GPUI Win32 HWND");
                configure_borderless_immersive(gpui_hwnd, initial_immersive);
                let thumb_host = ThumbnailHostWindow::create(
                    initial_immersive,
                    initial_backdrop,
                    initial_blur_opacity,
                )
                .expect("create bottom ThumbnailHostWindow");

                let view = cx.new(|cx| {
                    let v = SwitcherView::new(
                        gpui_hwnd,
                        thumb_host,
                        vdm_clone,
                        config_clone,
                        hotkey_warning,
                        window,
                        cx,
                    );
                    cx.observe_window_activation(window, |this, window, cx| {
                        this.handle_window_activation_changed(window, cx);
                    })
                    .detach();
                    v
                });

                let window_handle = Window::window_handle(window);
                let view_for_events = view.clone();

                if bench_mode {
                    let view_bench = view.clone();
                    cx.spawn(async move |cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(120))
                            .await;
                        let _ = cx.update(|cx| {
                            let _ = window_handle.update(cx, |_, window, cx| {
                                view_bench.update(cx, |v, cx| {
                                    let report = v.run_self_benchmark(window, cx);
                                    println!("{report}");
                                    append_log(&report);
                                });
                            });
                            cx.quit();
                        });
                    })
                    .detach();
                } else {
                    if !start_hidden {
                        let view_init = view.clone();
                        cx.spawn(async move |cx| {
                            let _ = cx.update(|cx| {
                                let _ = window_handle.update(cx, |_, window, cx| {
                                    view_init.update(cx, |v, cx| {
                                        v.show_and_refresh(window, cx);
                                    });
                                });
                            });
                        })
                        .detach();
                    }

                    cx.spawn(async move |cx| {
                        while let Ok(ev) = event_rx.recv().await {
                            let _ = cx.update(|cx| match ev {
                                ShellEvent::ToggleOverlay { .. } => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.toggle(window, cx);
                                        });
                                    });
                                }
                                ShellEvent::ShowOverlay => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.show_and_refresh(window, cx);
                                        });
                                    });
                                }
                                ShellEvent::AutostartToggled(enabled) => {
                                    append_log(&format!("AUTOSTART toggled -> enabled={enabled}"));
                                }
                                ShellEvent::ReloadConfig => {
                                    let new_cfg = AppConfig::load_or_create();
                                    if let Some(ref host) = *shell_for_loop {
                                        host.rebind_hotkey(new_cfg.parsed_hotkey());
                                    }
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.update_config(new_cfg, window, cx);
                                        });
                                    });
                                }
                                ShellEvent::OpenConfigDir => {
                                    let dir = config_dir();
                                    let _ = std::fs::create_dir_all(&dir);
                                    let _ = Command::new("explorer.exe").arg(dir).spawn();
                                }
                                ShellEvent::ExplorerRestarted => {
                                    view_for_events.update(cx, |v, _| {
                                        v.handle_explorer_restarted();
                                    });
                                }
                                ShellEvent::Quit => {
                                    view_for_events.update(cx, |v, cx| {
                                        v.hide(cx);
                                    });
                                    cx.quit();
                                }
                            });
                        }
                    })
                    .detach();
                }

                view
            },
        );

        if let Err(e) = opened {
            append_log(&format!("Failed to open WinDeck GPUI window: {e:#}"));
            cx.quit();
        }
    });

    Ok(())
}
