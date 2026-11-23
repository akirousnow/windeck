use anyhow::{Context, Result};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows_registry::LOCAL_MACHINE;

#[derive(Debug, Clone)]
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    pub ubr: u32,
    pub display_version: String,
}

impl OsVersion {
    pub fn current() -> Result<Self> {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        let status = unsafe { RtlGetVersion(&mut info) };
        if !status.is_ok() {
            anyhow::bail!("RtlGetVersion failed with NTSTATUS 0x{:08X}", status.0);
        }

        let key = LOCAL_MACHINE
            .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
            .context("open CurrentVersion registry key")?;
        let ubr = key.get_u32("UBR").unwrap_or(0);
        let display_version = key
            .get_string("DisplayVersion")
            .unwrap_or_else(|_| "unknown".to_string());

        Ok(Self {
            major: info.dwMajorVersion,
            minor: info.dwMinorVersion,
            build: info.dwBuildNumber,
            ubr,
            display_version,
        })
    }
}

impl std::fmt::Display for OsVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Windows {}.{} Build {}.{} ({})",
            self.major, self.minor, self.build, self.ubr, self.display_version
        )
    }
}
