#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod animation;
mod config;
mod genie;
mod nav;
mod settings_panel;
mod switcher_view;
mod thumb_manager;
mod update_dialog;
mod virtual_spaces;
mod window_order;

use std::ffi::c_void;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{
    App, Application, Bounds, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, prelude::*, px, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use win_platform::{
    CURRENT_VERSION, DesktopSwitchDir, ShellEvent, ShellHost, SingleInstanceGuard,
    ThumbnailHostWindow, UpdateCheckResult, VirtualDesktopClient, check_latest_release,
    configure_borderless_immersive, current_session_desktop_names, notify_existing_instance,
    wait_for_pid_and_cleanup_old_exe,
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

fn parse_arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn run_cli_update_check(effective_version: &str, verify_etag: bool) -> Result<()> {
    let t0 = Instant::now();
    let res = check_latest_release(effective_version, None)?;
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let etag = match &res {
        UpdateCheckResult::UpdateAvailable { release, etag } => {
            println!(
                "UPDATE_AVAILABLE current=v{} latest={} asset={:?} size={:?} url={:?} sha256={:?} elapsed={:.1}ms",
                effective_version,
                release.tag_name,
                release.asset_name,
                release.asset_size,
                release.download_url,
                release.sha256_url,
                ms
            );
            etag.clone()
        }
        UpdateCheckResult::UpToDate { latest_tag, etag } => {
            println!(
                "UP_TO_DATE current=v{} latest={} elapsed={:.1}ms",
                effective_version, latest_tag, ms
            );
            etag.clone()
        }
        UpdateCheckResult::NotModified => {
            println!("NOT_MODIFIED elapsed={:.1}ms", ms);
            None
        }
    };

    if verify_etag {
        if let Some(tag) = etag {
            let t1 = Instant::now();
            let res2 = check_latest_release(effective_version, Some(&tag))?;
            let ms2 = t1.elapsed().as_secs_f64() * 1000.0;
            println!(
                "ETAG_RECHECK etag={} result={:?} elapsed={:.1}ms",
                tag, res2, ms2
            );
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        append_log(&format!("PANIC at {loc}: {payload}"));
    }));

    let args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|a| a == "--hidden");
    let bench_mode = args.iter().any(|a| a == "--bench");
    let check_update_cli = args.iter().any(|a| a == "--check-update");
    let verify_etag = args.iter().any(|a| a == "--verify-etag");
    let wait_pid = parse_arg_value(&args, "--wait-pid").and_then(|s| s.parse::<u32>().ok());
    let effective_version =
        parse_arg_value(&args, "--simulate-version").unwrap_or_else(|| CURRENT_VERSION.to_string());

    if bench_mode || check_update_cli {
        attach_parent_console_if_needed();
    }

    if check_update_cli {
        return run_cli_update_check(&effective_version, verify_etag);
    }

    // If relaunched after an in-place self-update, wait for the exiting old process and clean up `.exe.old`.
    wait_for_pid_and_cleanup_old_exe(wait_pid);

    // Single-instance guard: prevent duplicate tray icons & hotkey conflicts.
    // When launched a second time interactively, wake the already-running instance and exit cleanly.
    let single_instance_guard = Arc::new(Mutex::new(if bench_mode {
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
    }));

    let mut config = AppConfig::load_or_create();
    let post_ota_restart = wait_pid.is_some();
    let (winsta, desk) = current_session_desktop_names();
    append_log(&format!(
        "STARTUP ver=v{} hotkey='{}' immersive={} label='{}' auto_update={} win_arrow_desktop={} desktop='{}\\{}' hidden={} bench={} wait_pid={:?}",
        effective_version,
        config.hotkey,
        config.immersive,
        config.label_position.as_config_str(),
        config.auto_update,
        config.win_arrow_switch_desktop,
        winsta,
        desk,
        start_hidden,
        bench_mode,
        wait_pid
    ));

    let vdm = Arc::new(VirtualDesktopClient::new()?);
    let (event_tx, event_rx) = async_channel::unbounded::<ShellEvent>();

    let hotkey_spec = config.parsed_hotkey();
    let shell_host = if bench_mode {
        None
    } else {
        let tx = event_tx.clone();
        match ShellHost::start(
            hotkey_spec,
            hotkey_spec.canonical_label(),
            config.win_arrow_switch_desktop,
            move |ev| {
                let _ = tx.send_blocking(ev);
            },
        ) {
            Ok(host) => Some(host),
            Err(e) => {
                append_log(&format!("ShellHost start failed: {e:#}"));
                None
            }
        }
    };

    if let Some(err) = shell_host.as_ref().and_then(|h| h.win_arrow_patch_error()) {
        append_log(&format!("WIN_ARROW_PATCH startup hook failed: {err}"));
        config.win_arrow_switch_desktop = false;
    }

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
    let initial_auto_update = config.auto_update;
    let guard_for_app = single_instance_guard.clone();
    let ver_for_app = effective_version.clone();

    Application::new().run(move |cx: &mut App| {
        let initial_bounds = Bounds::centered(None, size(px(1440.0), px(860.0)), cx);
        let vdm_clone = vdm.clone();
        let config_clone = config.clone();
        let shell_for_loop = shell_host.clone();
        let guard_for_view = guard_for_app.clone();
        let ver_for_view = ver_for_app.clone();

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
                let thumb_host = ThumbnailHostWindow::create(initial_immersive)
                    .expect("create bottom ThumbnailHostWindow");
                if let Some(ref host) = *shell_for_loop {
                    host.register_overlay_hwnds(gpui_hwnd, thumb_host.hwnd());
                }

                let shell_for_view = shell_for_loop.clone();
                let vdm_for_events = vdm_clone.clone();
                let view = cx.new(|cx| {
                    let v = SwitcherView::new(
                        gpui_hwnd,
                        thumb_host,
                        vdm_clone,
                        shell_for_view,
                        guard_for_view,
                        config_clone,
                        hotkey_warning,
                        ver_for_view,
                        bench_mode,
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
                            if post_ota_restart {
                                // Let the exiting process's DWM surfaces and GPUI's vsync thread settle
                                // cleanly before presenting the first post-OTA overlay.
                                cx.background_executor()
                                    .timer(Duration::from_millis(80))
                                    .await;
                            }
                            let _ = cx.update(|cx| {
                                let _ = window_handle.update(cx, |_, window, cx| {
                                    view_init.update(cx, |v, cx| {
                                        v.show_and_refresh(window, cx);
                                    });
                                });
                            });
                        })
                        .detach();
                    } else if initial_auto_update {
                        // Even when launched silently (--hidden at Windows startup), probe GitHub Releases once
                        let view_upd = view.clone();
                        cx.spawn(async move |cx| {
                            cx.background_executor()
                                .timer(Duration::from_secs(2))
                                .await;
                            let _ = cx.update(|cx| {
                                view_upd.update(cx, |v, cx| {
                                    v.trigger_update_check(false, cx);
                                });
                            });
                        })
                        .detach();
                    }

                    cx.spawn(async move |cx| {
                        while let Ok(ev) = event_rx.recv().await {
                            let _ = cx.update(|cx| match ev {
                                ShellEvent::ToggleOverlay { hold_armed, .. } => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.toggle_with_hold(hold_armed, window, cx);
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
                                ShellEvent::OpenSettings => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.open_settings(window, cx);
                                        });
                                    });
                                }
                                ShellEvent::CheckUpdate => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.check_update_from_shell(window, cx);
                                        });
                                    });
                                }
                                ShellEvent::OpenUpdateDialog => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.open_update_dialog_from_shell(window, cx);
                                        });
                                    });
                                }
                                ShellEvent::HoldNavigate(dir) => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.handle_hold_navigate(dir, window, cx);
                                        });
                                    });
                                }
                                ShellEvent::HoldModifierReleased { commit } => {
                                    let _ = window_handle.update(cx, |_, window, cx| {
                                        view_for_events.update(cx, |v, cx| {
                                            v.handle_hold_modifier_released(commit, window, cx);
                                        });
                                    });
                                }
                                ShellEvent::WinArrowComFallback(dir) => {
                                    let vdm_fallback = vdm_for_events.clone();
                                    cx.background_executor()
                                        .spawn(async move {
                                            if let Ok(desktops) = vdm_fallback.get_desktops()
                                                && let Some(cur_idx) =
                                                    desktops.iter().position(|d| d.is_current)
                                            {
                                                let target_idx = match dir {
                                                    DesktopSwitchDir::Left => {
                                                        cur_idx.checked_sub(1)
                                                    }
                                                    DesktopSwitchDir::Right => {
                                                        (cur_idx + 1 < desktops.len())
                                                            .then_some(cur_idx + 1)
                                                    }
                                                };
                                                if let Some(idx) = target_idx
                                                    && let Err(e) = vdm_fallback
                                                        .switch_to_desktop(desktops[idx].id)
                                                {
                                                    append_log(&format!(
                                                        "WIN_ARROW_PATCH COM fallback failed: {e:#}"
                                                    ));
                                                }
                                            }
                                        })
                                        .detach();
                                }
                                ShellEvent::AutostartToggled(enabled) => {
                                    append_log(&format!("AUTOSTART toggled -> enabled={enabled}"));
                                }
                                ShellEvent::ReloadConfig => {
                                    let mut new_cfg = AppConfig::load_or_create();
                                    if let Some(ref host) = *shell_for_loop {
                                        if let Err(e) = host.rebind_hotkey(new_cfg.parsed_hotkey())
                                        {
                                            append_log(&format!("RELOAD HOTKEY ERROR: {e:#}"));
                                        }
                                        if let Err(e) = host.set_win_arrow_patch_enabled(
                                            new_cfg.win_arrow_switch_desktop,
                                        ) {
                                            append_log(&format!(
                                                "RELOAD WIN_ARROW_PATCH ERROR: {e:#}"
                                            ));
                                            if new_cfg.win_arrow_switch_desktop {
                                                new_cfg.win_arrow_switch_desktop = false;
                                            }
                                        }
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
