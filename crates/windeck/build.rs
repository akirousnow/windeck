use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(output) = Command::new("where").arg("rc.exe").output() {
        if output.status.success() {
            if let Some(first) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
            {
                let p = PathBuf::from(first);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }

    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = std::fs::read_dir(kits_bin) {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("x64").join("rc.exe"))
            .filter(|p| p.exists())
            .collect();
        candidates.sort();
        if let Some(latest) = candidates.pop() {
            return Some(latest);
        }
    }

    None
}

fn main() {
    println!("cargo:rerun-if-changed=assets/windeck.rc");
    println!("cargo:rerun-if-changed=assets/windeck.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=Windows SDK rc.exe not found; skipping icon & version info embedding");
        return;
    };

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc_path = manifest_dir.join("assets").join("windeck.rc");
    let res_path = out_dir.join("windeck.res");

    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/c65001")
        .arg("/fo")
        .arg(&res_path)
        .arg(&rc_path)
        .status()
        .expect("invoke Windows SDK rc.exe");

    assert!(
        status.success(),
        "rc.exe failed compiling {} with status {:?}",
        rc_path.display(),
        status
    );

    println!("cargo:rustc-link-arg-bin=windeck={}", res_path.display());
}
