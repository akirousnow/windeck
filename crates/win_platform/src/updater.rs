use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_ETAG,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_FLAG_NUMBER64, WINHTTP_QUERY_LOCATION,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
use windows_core::{HSTRING, PCWSTR, w};

pub const GITHUB_OWNER: &str = "akirousnow";
pub const GITHUB_REPO: &str = "windeck";
pub const GITHUB_RELEASES_PAGE: &str = "https://github.com/akirousnow/windeck/releases";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const MAX_API_BODY_BYTES: usize = 2 * 1024 * 1024;
const MIN_VALID_EXE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub version: String,
    pub name: String,
    pub body: String,
    pub html_url: String,
    pub asset_name: Option<String>,
    pub download_url: Option<String>,
    pub asset_size: Option<u64>,
    pub sha256_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheckResult {
    UpdateAvailable {
        release: ReleaseInfo,
        etag: Option<String>,
    },
    UpToDate {
        latest_tag: String,
        etag: Option<String>,
    },
    NotModified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Option<String>,
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.prerelease, &other.prerelease) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(_), None) => std::cmp::Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_semver(raw: &str) -> Option<SemVer> {
    let s = raw.trim().trim_start_matches(['v', 'V']);
    if s.is_empty() {
        return None;
    }
    let without_build = s.split('+').next()?;
    let (core, pre) = match without_build.split_once('-') {
        Some((c, p)) => (c, (!p.is_empty()).then(|| p.to_string())),
        None => (without_build, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse::<u64>().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(SemVer {
        major,
        minor,
        patch,
        prerelease: pre,
    })
}

pub fn normalize_version_str(raw: &str) -> String {
    raw.trim().trim_start_matches(['v', 'V']).to_string()
}

pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => false,
    }
}

struct WinHttpHandle(*mut c_void);

impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
            self.0 = std::ptr::null_mut();
        }
    }
}

fn parse_https_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .trim()
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("仅支持 HTTPS 地址: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(anyhow!("无效的 URL 主机名: {url}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            let port = p
                .parse::<u16>()
                .with_context(|| format!("无效端口号: {url}"))?;
            (h.to_string(), port)
        }
        _ => (authority.to_string(), 443u16),
    };
    Ok((host, port, path.to_string()))
}

fn open_winhttp_session() -> Result<WinHttpHandle> {
    let ua = HSTRING::from(format!("WinDeck/{CURRENT_VERSION}"));
    let mut handle = unsafe {
        WinHttpOpen(
            PCWSTR(ua.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    };
    if handle.is_null() {
        handle = unsafe {
            WinHttpOpen(
                PCWSTR(ua.as_ptr()),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        };
    }
    if handle.is_null() {
        return Err(anyhow!(
            "WinHttpOpen 失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(WinHttpHandle(handle))
}

fn query_status_code(hrequest: *mut c_void) -> Result<u32> {
    let mut status: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    let mut index: u32 = 0;
    unsafe {
        WinHttpQueryHeaders(
            hrequest,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast::<c_void>()),
            &mut len,
            &mut index,
        )
    }
    .context("WinHttpQueryHeaders(STATUS_CODE)")?;
    Ok(status)
}

fn query_content_length(hrequest: *mut c_void) -> Option<u64> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>() as u32;
    let mut index: u32 = 0;
    unsafe {
        WinHttpQueryHeaders(
            hrequest,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER64,
            PCWSTR::null(),
            Some((&mut value as *mut u64).cast::<c_void>()),
            &mut len,
            &mut index,
        )
    }
    .ok()
    .map(|_| value)
}

fn query_header_string(hrequest: *mut c_void, info_level: u32) -> Option<String> {
    let mut byte_len: u32 = 0;
    let mut index: u32 = 0;
    let _ = unsafe {
        WinHttpQueryHeaders(
            hrequest,
            info_level,
            PCWSTR::null(),
            None,
            &mut byte_len,
            &mut index,
        )
    };
    if byte_len == 0 {
        return None;
    }
    let mut buf = vec![0u16; (byte_len as usize).div_ceil(2)];
    let mut actual_len = (buf.len() * 2) as u32;
    index = 0;
    unsafe {
        WinHttpQueryHeaders(
            hrequest,
            info_level,
            PCWSTR::null(),
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &mut actual_len,
            &mut index,
        )
    }
    .ok()?;
    let u16_len = (actual_len as usize / 2).min(buf.len());
    let trimmed = &buf[..u16_len];
    let trimmed = trimmed
        .iter()
        .position(|&c| c == 0)
        .map_or(trimmed, |pos| &trimmed[..pos]);
    let s = String::from_utf16_lossy(trimmed).trim().to_string();
    (!s.is_empty()).then_some(s)
}

struct HttpResponse {
    status: u32,
    etag: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
}

fn http_get_bytes(
    url: &str,
    extra_headers: &str,
    follow_redirects: bool,
    receive_timeout_ms: i32,
) -> Result<HttpResponse> {
    let (host, port, path) = parse_https_url(url)?;
    let session = open_winhttp_session()?;
    unsafe {
        let _ = WinHttpSetTimeouts(session.0, 5_000, 8_000, 10_000, receive_timeout_ms);
    }

    let host_w = HSTRING::from(host.as_str());
    let connect_raw = unsafe { WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), port, 0) };
    if connect_raw.is_null() {
        return Err(anyhow!(
            "连接 {host} 失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    let connect = WinHttpHandle(connect_raw);

    let path_w = HSTRING::from(path.as_str());
    let request_raw = unsafe {
        WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    };
    if request_raw.is_null() {
        return Err(anyhow!(
            "WinHttpOpenRequest 失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    let request = WinHttpHandle(request_raw);

    let redirect_policy = if follow_redirects {
        WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP
    } else {
        WINHTTP_OPTION_REDIRECT_POLICY_NEVER
    };
    unsafe {
        let _ = WinHttpSetOption(
            Some(request.0.cast_const()),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&redirect_policy.to_ne_bytes()),
        );
    }

    let hdr_u16: Vec<u16> = extra_headers.encode_utf16().collect();
    let hdr_opt = (!hdr_u16.is_empty()).then_some(hdr_u16.as_slice());

    unsafe { WinHttpSendRequest(request.0, hdr_opt, None, 0, 0, 0) }
        .context("发送 HTTPS 请求失败（请检查网络或代理连接）")?;
    unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
        .context("接收 HTTPS 响应失败")?;

    let status = query_status_code(request.0)?;
    let etag = query_header_string(request.0, WINHTTP_QUERY_ETAG);
    let location = query_header_string(request.0, WINHTTP_QUERY_LOCATION);

    let mut body = Vec::new();
    if status != 304 && follow_redirects {
        loop {
            let mut avail: u32 = 0;
            unsafe { WinHttpQueryDataAvailable(request.0, &mut avail) }
                .context("WinHttpQueryDataAvailable")?;
            if avail == 0 {
                break;
            }
            let chunk_len = (avail as usize).min(64 * 1024);
            if body.len() + chunk_len > MAX_API_BODY_BYTES {
                return Err(anyhow!("响应体积超出限制"));
            }
            let start = body.len();
            body.resize(start + chunk_len, 0);
            let mut read: u32 = 0;
            unsafe {
                WinHttpReadData(
                    request.0,
                    body[start..].as_mut_ptr().cast::<c_void>(),
                    chunk_len as u32,
                    &mut read,
                )
            }
            .context("WinHttpReadData")?;
            body.truncate(start + read as usize);
            if read == 0 {
                break;
            }
        }
    }

    Ok(HttpResponse {
        status,
        etag,
        location,
        body,
    })
}

fn parse_github_release_json(body: &[u8]) -> Result<ReleaseInfo> {
    let val: serde_json::Value =
        serde_json::from_slice(body).context("解析 GitHub Release JSON 失败")?;

    let tag_name = val
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("GitHub Release 缺少 tag_name 字段"))?
        .to_string();

    let version = normalize_version_str(&tag_name);
    let name = val
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&tag_name)
        .to_string();
    let release_body = val
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let html_url = val
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            format!("https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/tag/{tag_name}")
        });

    let mut best_exe: Option<(i32, String, String, Option<u64>)> = None;
    let mut sha256_url: Option<String> = None;

    if let Some(assets) = val.get("assets").and_then(|v| v.as_array()) {
        for asset in assets {
            let Some(a_name) = asset.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(a_url) = asset.get("browser_download_url").and_then(|v| v.as_str()) else {
                continue;
            };
            let a_size = asset.get("size").and_then(|v| v.as_u64());
            let lower = a_name.to_ascii_lowercase();

            if lower == "windeck.exe.sha256"
                || lower == "sha256sums.txt"
                || (sha256_url.is_none() && lower.ends_with(".sha256"))
            {
                sha256_url = Some(a_url.to_string());
                continue;
            }

            if lower.ends_with(".exe") {
                let score = if lower == "windeck.exe" {
                    100
                } else if lower.contains("windeck") && (lower.contains("x64") || lower.contains("win"))
                {
                    80
                } else if lower.contains("windeck") {
                    60
                } else {
                    20
                };
                if best_exe.as_ref().is_none_or(|(best_s, ..)| score > *best_s) {
                    best_exe = Some((score, a_name.to_string(), a_url.to_string(), a_size));
                }
            }
        }
    }

    let (asset_name, download_url, asset_size) = match best_exe {
        Some((_, n, u, sz)) => (Some(n), Some(u), sz),
        None => (None, None, None),
    };

    Ok(ReleaseInfo {
        tag_name,
        version,
        name,
        body: release_body,
        html_url,
        asset_name,
        download_url,
        asset_size,
        sha256_url,
    })
}

fn check_latest_release_via_redirect(current_version: &str) -> Result<UpdateCheckResult> {
    let url = format!("https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest");
    let resp = http_get_bytes(&url, "", false, 12_000)?;
    if !(300..400).contains(&resp.status) {
        return Err(anyhow!("GitHub Releases 返回状态码 HTTP {}", resp.status));
    }
    let loc = resp
        .location
        .ok_or_else(|| anyhow!("GitHub Releases 重定向缺少 Location 头"))?;
    let tag = loc
        .rsplit("/releases/tag/")
        .next()
        .filter(|t| *t != loc.as_str() && !t.is_empty())
        .map(|t| t.split(['?', '#']).next().unwrap_or(t).to_string())
        .ok_or_else(|| anyhow!("无法从重定向地址解析版本号: {loc}"))?;

    if !is_newer_version(current_version, &tag) {
        return Ok(UpdateCheckResult::UpToDate {
            latest_tag: tag,
            etag: None,
        });
    }

    let version = normalize_version_str(&tag);
    Ok(UpdateCheckResult::UpdateAvailable {
        release: ReleaseInfo {
            tag_name: tag.clone(),
            version,
            name: format!("WinDeck {tag}"),
            body: String::new(),
            html_url: format!(
                "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/tag/{tag}"
            ),
            asset_name: Some("windeck.exe".to_string()),
            download_url: Some(format!(
                "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/download/{tag}/windeck.exe"
            )),
            asset_size: None,
            sha256_url: Some(format!(
                "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/download/{tag}/windeck.exe.sha256"
            )),
        },
        etag: None,
    })
}

/// Checks GitHub Releases for a newer version than `current_version`.
/// Supports ETag conditional requests (`304 Not Modified` costs 0 GitHub API quota) and falls back
/// to GitHub's non-rate-limited `/releases/latest` redirect endpoint when API quota is exhausted.
pub fn check_latest_release(
    current_version: &str,
    cached_etag: Option<&str>,
) -> Result<UpdateCheckResult> {
    let api_url =
        format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest");
    let mut headers = String::from(
        "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n",
    );
    if let Some(etag) = cached_etag {
        if !etag.trim().is_empty() {
            headers.push_str(&format!("If-None-Match: {}\r\n", etag.trim()));
        }
    }

    let resp = http_get_bytes(&api_url, &headers, true, 12_000)?;
    match resp.status {
        304 => Ok(UpdateCheckResult::NotModified),
        200 => {
            let release = parse_github_release_json(&resp.body)?;
            if is_newer_version(current_version, &release.tag_name) {
                Ok(UpdateCheckResult::UpdateAvailable {
                    release,
                    etag: resp.etag,
                })
            } else {
                Ok(UpdateCheckResult::UpToDate {
                    latest_tag: release.tag_name,
                    etag: resp.etag,
                })
            }
        }
        403 | 429 => check_latest_release_via_redirect(current_version),
        404 => Err(anyhow!("GitHub 仓库尚未发布 Release")),
        other => Err(anyhow!("GitHub API 返回异常状态码 HTTP {other}")),
    }
}

fn http_download_to_file(
    url: &str,
    dest_path: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(u64, [u8; 32])> {
    let (host, port, path) = parse_https_url(url)?;
    let session = open_winhttp_session()?;
    unsafe {
        let _ = WinHttpSetTimeouts(session.0, 8_000, 12_000, 15_000, 60_000);
    }

    let host_w = HSTRING::from(host.as_str());
    let connect_raw = unsafe { WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), port, 0) };
    if connect_raw.is_null() {
        return Err(anyhow!(
            "下载服务器连接失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    let connect = WinHttpHandle(connect_raw);

    let path_w = HSTRING::from(path.as_str());
    let request_raw = unsafe {
        WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    };
    if request_raw.is_null() {
        return Err(anyhow!(
            "WinHttpOpenRequest 失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    let request = WinHttpHandle(request_raw);

    let policy = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP;
    unsafe {
        let _ = WinHttpSetOption(
            Some(request.0.cast_const()),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&policy.to_ne_bytes()),
        );
    }

    unsafe { WinHttpSendRequest(request.0, None, None, 0, 0, 0) }.context("发送下载请求失败")?;
    unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
        .context("接收下载响应失败")?;

    let status = query_status_code(request.0)?;
    if status != 200 {
        return Err(anyhow!("下载安装包失败（HTTP {status}）"));
    }

    let total_len = query_content_length(request.0);
    let mut file = File::create(dest_path)
        .with_context(|| format!("无法在当前目录写入临时更新文件：{}", dest_path.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    on_progress(0, total_len);

    loop {
        let mut avail: u32 = 0;
        if let Err(e) = unsafe { WinHttpQueryDataAvailable(request.0, &mut avail) } {
            drop(file);
            let _ = fs::remove_file(dest_path);
            return Err(anyhow!("下载连接中断: {e}"));
        }
        if avail == 0 {
            break;
        }
        let to_read = (avail as usize).min(buf.len()) as u32;
        let mut read: u32 = 0;
        if let Err(e) = unsafe {
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast::<c_void>(),
                to_read,
                &mut read,
            )
        } {
            drop(file);
            let _ = fs::remove_file(dest_path);
            return Err(anyhow!("读取下载数据失败: {e}"));
        }
        if read == 0 {
            break;
        }
        let slice = &buf[..read as usize];
        if let Err(e) = file.write_all(slice) {
            drop(file);
            let _ = fs::remove_file(dest_path);
            return Err(anyhow!("写入更新文件失败: {e}"));
        }
        hasher.update(slice);
        downloaded += read as u64;
        on_progress(downloaded, total_len);
    }

    file.flush().context("刷新更新文件缓存失败")?;
    Ok((downloaded, hasher.finalize()))
}

fn parse_expected_sha256(text: &str, target_filename: &str) -> Option<String> {
    let target_lower = target_filename.to_ascii_lowercase();
    let mut fallback: Option<String> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let first = parts.next()?;
        if first.len() == 64 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let hex_lower = first.to_ascii_lowercase();
            let file_part = parts
                .next()
                .map(|p| p.trim_start_matches('*').to_ascii_lowercase());
            if let Some(ref f) = file_part {
                if f == &target_lower || f.ends_with(&format!("/{target_lower}")) {
                    return Some(hex_lower);
                }
            }
            if fallback.is_none() {
                fallback = Some(hex_lower);
            }
        }
    }
    fallback
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

/// Downloads the release's `windeck.exe`, verifies its PE header and optional SHA-256 checksum,
/// and atomically swaps it into place (renaming the currently running executable to `.exe.old`).
/// Returns the executable path to relaunch.
pub fn download_and_stage_update(
    release: &ReleaseInfo,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    let download_url = release
        .download_url
        .as_deref()
        .ok_or_else(|| anyhow!("该 Release 未附带可下载的 windeck.exe 文件"))?;

    let current_exe = std::env::current_exe().context("无法获取当前程序路径")?;
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("无法确定程序所在目录"))?;
    let exe_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("windeck.exe");

    let new_path = exe_dir.join(format!("{exe_name}.new"));
    let old_path = exe_dir.join(format!("{exe_name}.old"));

    let _ = fs::remove_file(&new_path);

    let (bytes_written, digest) = http_download_to_file(download_url, &new_path, |done, total| {
        on_progress(done, total.or(release.asset_size));
    })?;

    if bytes_written < MIN_VALID_EXE_BYTES {
        let _ = fs::remove_file(&new_path);
        return Err(anyhow!(
            "下载的文件体积异常（仅 {bytes_written} 字节），已取消替换"
        ));
    }

    let mut magic = [0u8; 2];
    let magic_ok = File::open(&new_path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == *b"MZ";
    if !magic_ok {
        let _ = fs::remove_file(&new_path);
        return Err(anyhow!("下载的文件不是有效的 Windows 可执行程序，已取消替换"));
    }

    if let Some(ref sha_url) = release.sha256_url {
        if let Ok(resp) = http_get_bytes(sha_url, "", true, 10_000) {
            if resp.status == 200 {
                let text = String::from_utf8_lossy(&resp.body);
                let target_asset = release.asset_name.as_deref().unwrap_or("windeck.exe");
                if let Some(expected) = parse_expected_sha256(&text, target_asset) {
                    let actual = hex_encode(&digest);
                    if actual != expected {
                        let _ = fs::remove_file(&new_path);
                        return Err(anyhow!(
                            "安装包 SHA-256 校验失败（文件可能损坏），已取消更新"
                        ));
                    }
                }
            }
        }
    }

    let _ = fs::remove_file(&old_path);
    if let Err(e) = fs::rename(&current_exe, &old_path) {
        let _ = fs::remove_file(&new_path);
        return Err(anyhow!(
            "无法替换当前程序（目录可能无写权限：{e}），请手动下载更新"
        ));
    }

    if let Err(e) = fs::rename(&new_path, &current_exe) {
        // Roll back immediately so the user is never left without windeck.exe
        let _ = fs::rename(&old_path, &current_exe);
        let _ = fs::remove_file(&new_path);
        return Err(anyhow!("应用更新失败并已自动回滚：{e}"));
    }

    Ok(current_exe)
}

/// Spawns the newly staged WinDeck executable detached in its own process group and hands off
/// Windows foreground activation rights while the exiting process still owns the foreground.
pub fn relaunch_updated_exe(exe_path: &Path, wait_pid: u32) -> Result<u32> {
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    let pid_str = wait_pid.to_string();
    let child = Command::new(exe_path)
        .args(["--wait-pid", &pid_str])
        .creation_flags(CREATE_NEW_PROCESS_GROUP.0)
        .spawn()
        .with_context(|| format!("重启新版程序失败：{}", exe_path.display()))?;
    let child_pid = child.id();
    unsafe {
        let _ = AllowSetForegroundWindow(child_pid);
    }
    Ok(child_pid)
}

/// Waits briefly for a previous WinDeck PID (when restarted after self-update) and cleans up
/// any leftover `windeck.exe.old` file beside the current executable.
pub fn wait_for_pid_and_cleanup_old_exe(wait_pid: Option<u32>) {
    if let Some(pid) = wait_pid {
        if pid != 0 && pid != std::process::id() {
            unsafe {
                if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                    let _ = WaitForSingleObject(handle, 5_000);
                    let _ = CloseHandle(handle);
                }
            }
        }
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if let (Some(dir), Some(name)) = (
            current_exe.parent(),
            current_exe.file_name().and_then(|n| n.to_str()),
        ) {
            let old_path = dir.join(format!("{name}.old"));
            if old_path.exists() {
                for _ in 0..8 {
                    if fs::remove_file(&old_path).is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
            }
        }
    }
}

/// Minimal dependency-free SHA-256 implementation (FIPS 180-4) for release checksum verification.
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_bits: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0; 64],
            buf_len: 0,
            total_bits: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bits = self.total_bits.wrapping_add((data.len() as u64) * 8);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bits = self.total_bits;
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;
        if self.buf_len > 56 {
            self.buf[self.buf_len..].fill(0);
            let block = self.buf;
            self.compress(&block);
            self.buf.fill(0);
        } else {
            self.buf[self.buf_len..56].fill(0);
        }
        self.buf[56..64].copy_from_slice(&bits.to_be_bytes());
        let block = self.buf;
        self.compress(&block);

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_handles_prefixes_and_prereleases() {
        assert!(is_newer_version("0.2.0", "v0.2.1"));
        assert!(is_newer_version("0.2.0", "v0.3.0"));
        assert!(is_newer_version("0.2.0", "1.0.0"));
        assert!(is_newer_version("0.2.0-beta.1", "0.2.0"));
        assert!(!is_newer_version("0.2.0", "v0.2.0"));
        assert!(!is_newer_version("0.2.1", "v0.2.0"));
        assert!(!is_newer_version("0.2.0", "v0.2.0-rc.1"));
        assert!(!is_newer_version("0.2.0", "not-a-version"));
    }

    #[test]
    fn https_url_parser_rejects_http_and_parses_ports() {
        assert!(parse_https_url("http://example.com/a").is_err());
        let (h, p, path) =
            parse_https_url("https://api.github.com/repos/akirousnow/windeck/releases/latest")
                .unwrap();
        assert_eq!(h, "api.github.com");
        assert_eq!(p, 443);
        assert_eq!(path, "/repos/akirousnow/windeck/releases/latest");

        let (h2, p2, path2) = parse_https_url("https://example.com:8443/dl?a=1").unwrap();
        assert_eq!(h2, "example.com");
        assert_eq!(p2, 8443);
        assert_eq!(path2, "/dl?a=1");
    }

    #[test]
    fn github_release_json_picks_windeck_exe_and_sha256() {
        let json = br#"{
            "tag_name": "v0.3.0",
            "name": "WinDeck v0.3.0",
            "body": "- Added auto-update\n- Faster thumbnails",
            "html_url": "https://github.com/akirousnow/windeck/releases/tag/v0.3.0",
            "assets": [
                {
                    "name": "windeck.exe.sha256",
                    "size": 78,
                    "browser_download_url": "https://github.com/akirousnow/windeck/releases/download/v0.3.0/windeck.exe.sha256"
                },
                {
                    "name": "windeck.exe",
                    "size": 8021504,
                    "browser_download_url": "https://github.com/akirousnow/windeck/releases/download/v0.3.0/windeck.exe"
                }
            ]
        }"#;
        let rel = parse_github_release_json(json).unwrap();
        assert_eq!(rel.tag_name, "v0.3.0");
        assert_eq!(rel.version, "0.3.0");
        assert_eq!(rel.asset_name.as_deref(), Some("windeck.exe"));
        assert_eq!(rel.asset_size, Some(8021504));
        assert!(rel.download_url.unwrap().ends_with("/windeck.exe"));
        assert!(rel.sha256_url.unwrap().ends_with("/windeck.exe.sha256"));
    }

    #[test]
    fn sha256_matches_nist_vectors_and_checksum_parser() {
        let h_empty = {
            let s = Sha256::new();
            hex_encode(&s.finalize())
        };
        assert_eq!(
            h_empty,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let h_abc = {
            let mut s = Sha256::new();
            s.update(b"abc");
            hex_encode(&s.finalize())
        };
        assert_eq!(
            h_abc,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let sum_text = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  windeck.exe\n";
        assert_eq!(
            parse_expected_sha256(sum_text, "windeck.exe").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn relaunch_updated_exe_handles_missing_and_valid_binaries() {
        let missing = Path::new(r"C:\__windeck_nonexistent_test_dir__\missing_windeck.exe");
        assert!(relaunch_updated_exe(missing, 999_999).is_err());

        // Spawn the test harness binary itself with `--wait-pid`, which rejects the unknown flag
        // and exits immediately without opening UI or side effects, proving detached spawn works.
        let self_exe = std::env::current_exe().expect("current_exe");
        let child_pid = relaunch_updated_exe(&self_exe, std::process::id())
            .expect("relaunch_updated_exe should spawn valid executable");
        assert_ne!(child_pid, 0);
        assert_ne!(child_pid, std::process::id());

        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, child_pid) {
                let _ = WaitForSingleObject(handle, 3_000);
                let _ = CloseHandle(handle);
            }
        }
    }
}
