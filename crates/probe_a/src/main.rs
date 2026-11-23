use std::collections::BTreeMap;
use std::ffi::c_void;
use std::time::Instant;

use anyhow::{Context, Result};
use win_platform::{
    OsVersion, VirtualDesktopClient, current_session_desktop_names, enumerate_monitors,
    enumerate_windows, switch_to_window,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowPlacement, IsIconic, IsZoomed,
    RegisterClassW, SW_MINIMIZE, SW_SHOW, SW_SHOWMAXIMIZED, ShowWindow, WINDOW_EX_STYLE,
    WINDOWPLACEMENT, WNDCLASSW, WPF_RESTORETOMAXIMIZED, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

fn run_self_test(vdm: &VirtualDesktopClient) -> Result<()> {
    println!("\n=== PROBE A SELF-TEST (IN-SESSION WIN32 ASSERTIONS) ===");
    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    let hinstance = unsafe { GetModuleHandleW(None) }?;
    let class_name = w!("WinDeckProbeATestWindow");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };

    let create = |title: PCWSTR, ex: WINDOW_EX_STYLE| -> Result<HWND> {
        unsafe {
            CreateWindowExW(
                ex,
                class_name,
                title,
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                640,
                400,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .context("CreateWindowExW test window")
    };

    let normal_hwnd = create(w!("ProbeA Normal Window"), WINDOW_EX_STYLE(0))?;
    let tool_hwnd = create(w!("ProbeA Tool Window (Must Drop)"), WS_EX_TOOLWINDOW)?;
    let max_min_hwnd = create(w!("ProbeA MaxThenMin Window"), WINDOW_EX_STYLE(0))?;

    struct Guard(Vec<HWND>);
    impl Drop for Guard {
        fn drop(&mut self) {
            for h in &self.0 {
                let _ = unsafe { DestroyWindow(*h) };
            }
        }
    }
    let _guard = Guard(vec![normal_hwnd, tool_hwnd, max_min_hwnd]);

    unsafe {
        let _ = ShowWindow(normal_hwnd, SW_SHOW);
        let _ = ShowWindow(tool_hwnd, SW_SHOW);
        let _ = ShowWindow(max_min_hwnd, SW_SHOWMAXIMIZED);
        let _ = ShowWindow(max_min_hwnd, SW_MINIMIZE);
    }

    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GetWindowPlacement(max_min_hwnd, &mut placement) };
    let has_restore_max = (placement.flags.0 & WPF_RESTORETOMAXIMIZED.0) != 0;
    println!(
        "MaxThenMin before switch: iconic={} zoomed={} WPF_RESTORETOMAXIMIZED={}",
        unsafe { IsIconic(max_min_hwnd) }.as_bool(),
        unsafe { IsZoomed(max_min_hwnd) }.as_bool(),
        has_restore_max
    );

    let desktops = vdm.get_desktops()?;
    let monitors = enumerate_monitors();
    let (kept, diags) = enumerate_windows(vdm, &desktops, &monitors, &[], true)?;

    let kept_normal = kept.iter().any(|w| w.hwnd == normal_hwnd.0 as isize);
    let kept_tool = kept.iter().any(|w| w.hwnd == tool_hwnd.0 as isize);
    let kept_max_min = kept.iter().any(|w| w.hwnd == max_min_hwnd.0 as isize);
    println!(
        "Filter assertions: normal_kept={} (expect true), tool_kept={} (expect false), max_min_kept={} (expect true), total_diags={}",
        kept_normal,
        kept_tool,
        kept_max_min,
        diags.len()
    );

    // Test switch_to_window on max_min_hwnd while hiding normal_hwnd as simulated switcher
    let report = switch_to_window(
        vdm,
        max_min_hwnd.0 as isize,
        Some(normal_hwnd.0 as isize),
    )?;
    println!("SwitchReport on MaxThenMin: {report:#?}");

    assert!(kept_normal, "Normal test window should be kept");
    assert!(!kept_tool, "WS_EX_TOOLWINDOW must be filtered out");
    assert!(kept_max_min, "Minimized normal window should be kept");
    assert_eq!(report.self_hidden_after, Some(true), "Switcher must be hidden");
    assert!(!report.is_iconic_after, "Target must not remain minimized");
    if has_restore_max {
        assert!(
            report.is_zoomed_after,
            "Maximized-then-minimized window must restore to SW_SHOWMAXIMIZED"
        );
    }

    // Verify COM reconnect works cleanly
    let status_after_reconnect = vdm.reconnect()?;
    println!("COM reconnect check: {:?}", status_after_reconnect);
    assert!(status_after_reconnect.official_vdm_ok);
    println!("SELF-TEST PASSED!");
    Ok(())
}

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let args: Vec<String> = std::env::args().collect();
    let dump_all = args.iter().any(|a| a == "--dump-all");
    let self_test = args.iter().any(|a| a == "--self-test");
    let test_create_desktop = args.iter().any(|a| a == "--test-create-desktop");
    let switch_target = args
        .windows(2)
        .find(|w| w[0] == "--switch")
        .map(|w| w[1].clone());

    let os = OsVersion::current()?;
    let (winsta, desk) = current_session_desktop_names();
    println!("=== OS & SESSION ===");
    println!("OS: {os}");
    println!("WindowStation: {winsta} | ThreadDesktop: {desk}");
    if !desk.eq_ignore_ascii_case("Default") {
        println!(
            "NOTE: Current process is on isolated desktop '{desk}' (not 'Default'), so EnumWindows only sees windows created inside this desktop."
        );
    }

    let t0 = Instant::now();
    let vdm = VirtualDesktopClient::new()?;
    let probe = vdm.probe_status()?;
    println!(
        "\n=== COM PROBE ({:.2} ms) ===",
        t0.elapsed().as_secs_f64() * 1000.0
    );
    println!("official IVirtualDesktopManager: {}", probe.official_vdm_ok);
    println!(
        "internal IVirtualDesktopManagerInternal: {:?}",
        probe.internal_profile
    );
    println!(
        "IApplicationViewCollection: {}",
        probe.app_view_collection_ok
    );
    println!("IVirtualDesktopPinnedApps: {}", probe.pinned_apps_ok);
    println!("COM GetCount(): {:?}", probe.com_desktop_count);
    if !probe.errors.is_empty() {
        println!("COM init errors:");
        for err in &probe.errors {
            println!("  - {err}");
        }
    }

    let t1 = Instant::now();
    let desktops = vdm.get_desktops()?;
    println!(
        "\n=== VIRTUAL DESKTOPS ({} found in {:.2} ms) ===",
        desktops.len(),
        t1.elapsed().as_secs_f64() * 1000.0
    );
    for d in &desktops {
        println!(
            "  [{}] {} | id={} | current={} | custom_name={:?} | src={}",
            d.index, d.display_name, d.id, d.is_current, d.custom_name, d.source
        );
    }

    let monitors = enumerate_monitors();
    println!("\n=== MONITORS ({}) ===", monitors.len());
    for m in &monitors {
        println!(
            "  [{}] {} primary={} bounds=({},{})-({},{}) {}x{} dpi={}x{} scale={:.2}x",
            m.index,
            m.device_name,
            m.is_primary,
            m.bounds.left,
            m.bounds.top,
            m.bounds.right,
            m.bounds.bottom,
            m.bounds.right - m.bounds.left,
            m.bounds.bottom - m.bounds.top,
            m.dpi_x,
            m.dpi_y,
            m.scale_factor
        );
    }

    if test_create_desktop {
        println!("\n=== TEST CREATE + REMOVE VIRTUAL DESKTOP ===");
        let before = vdm.get_desktops()?;
        let cur_id = before
            .iter()
            .find(|d| d.is_current)
            .map(|d| d.id)
            .unwrap_or(before[0].id);
        let new_id = vdm.create_desktop()?;
        let after_create = vdm.get_desktops()?;
        println!(
            "Created desktop {} | count {} -> {} (active unchanged: {})",
            new_id,
            before.len(),
            after_create.len(),
            after_create.iter().any(|d| d.is_current && d.id == cur_id)
        );
        assert_eq!(after_create.len(), before.len() + 1);
        assert!(after_create.iter().any(|d| d.id == new_id));
        vdm.remove_desktop(new_id, cur_id)?;
        let after_remove = vdm.get_desktops()?;
        println!(
            "Removed desktop {} | count {} -> {}",
            new_id,
            after_create.len(),
            after_remove.len()
        );
        assert_eq!(after_remove.len(), before.len());
        println!("CREATE+REMOVE DESKTOP ROUND-TRIP PASSED!");
        return Ok(());
    }

    if self_test {
        return run_self_test(&vdm);
    }

    if let Some(hex_str) = switch_target {
        let cleaned = hex_str.trim_start_matches("0x").trim_start_matches("0X");
        let hwnd_val = isize::from_str_radix(cleaned, 16)?;
        println!("\n=== SWITCH TEST -> HWND 0x{:X} ===", hwnd_val);
        let report = switch_to_window(&vdm, hwnd_val, None)?;
        println!("{report:#?}");
        return Ok(());
    }

    let t2 = Instant::now();
    let (windows, diagnostics) = enumerate_windows(&vdm, &desktops, &monitors, &[], dump_all)?;
    println!(
        "\n=== ALT-TAB WINDOWS ({} kept in {:.2} ms) ===",
        windows.len(),
        t2.elapsed().as_secs_f64() * 1000.0
    );

    let mut grouped: BTreeMap<Option<usize>, Vec<_>> = BTreeMap::new();
    for w in &windows {
        grouped.entry(w.desktop_index).or_default().push(w);
    }

    for (d_idx, list) in grouped {
        let header = match d_idx {
            Some(i) => {
                let d = &desktops[i];
                format!(
                    "Desktop {} ({}){}",
                    i + 1,
                    d.display_name,
                    if d.is_current { " [CURRENT]" } else { "" }
                )
            }
            None => "Unassigned Desktop".to_string(),
        };
        println!("\n--- {header} ({} windows) ---", list.len());
        for w in list {
            let mut flags = Vec::new();
            if w.is_pinned {
                flags.push("PINNED");
            }
            if w.is_minimized {
                flags.push("MIN");
            }
            if w.is_maximized {
                flags.push("MAX");
            }
            if w.restore_to_maximized {
                flags.push("RESTORE_MAX");
            }
            if w.is_elevated == Some(true) {
                flags.push("ADMIN");
            }
            if w.cloaked_flags != 0 {
                flags.push("CLOAKED");
            }
            println!(
                "  0x{:08X} | {:24} | flags=[{}] | mon={:?} | {}x{} | \"{}\"",
                w.hwnd,
                w.process_name,
                flags.join(","),
                w.monitor_index,
                w.rect.right - w.rect.left,
                w.rect.bottom - w.rect.top,
                w.title
            );
        }
    }

    if dump_all {
        println!(
            "\n=== RAW DIAGNOSTICS ({} visible top-level windows) ===",
            diagnostics.len()
        );
        for d in &diagnostics {
            println!(
                "  [{}] 0x{:08X} | proc={:<20} | class={:<28} | cloak=0x{:x} | off_did={:?} | view_did={:?} | appview={} | pin={} | reason={} | \"{}\"",
                if d.kept { "KEEP" } else { "DROP" },
                d.hwnd,
                d.process_name,
                d.class_name,
                d.cloaked,
                d.lookup.official_desktop_id.map(|g| format!("{}", g)),
                d.lookup.view_desktop_id.map(|g| format!("{}", g)),
                d.lookup.has_app_view,
                d.lookup.is_pinned,
                d.reason,
                d.title
            );
        }
    }

    let _ = std::ptr::null::<c_void>();
    Ok(())
}
