use pinyin::ToPinyinMulti;
use smallvec::SmallVec;
use win_platform::WindowInfo;

enum SearchUnit {
    Literal(char),
    Hanzi(SmallVec<[&'static str; 4]>),
}

fn push_unique_reading(out: &mut SmallVec<[&'static str; 4]>, reading: &'static str) {
    if !reading.is_empty() && !out.contains(&reading) {
        out.push(reading);
    }
}

/// Converts a plain pinyin syllable from `pinyin` crate into pure-ASCII keyboard-typeable
/// readings (handling `ü` -> both `v` and `u`, plus rare `ê` -> `e` with zero heap allocation).
fn push_normalized_readings(out: &mut SmallVec<[&'static str; 4]>, plain: &'static str) {
    if plain.is_ascii() {
        push_unique_reading(out, plain);
        return;
    }
    match plain {
        "lü" => {
            push_unique_reading(out, "lv");
            push_unique_reading(out, "lu");
        }
        "nü" => {
            push_unique_reading(out, "nv");
            push_unique_reading(out, "nu");
        }
        "lüe" => {
            push_unique_reading(out, "lve");
            push_unique_reading(out, "lue");
        }
        "nüe" => {
            push_unique_reading(out, "nve");
            push_unique_reading(out, "nue");
        }
        "lüan" => {
            push_unique_reading(out, "lvan");
            push_unique_reading(out, "luan");
        }
        "ê" => {
            push_unique_reading(out, "e");
        }
        _ => {}
    }
}

fn build_search_units(text: &str) -> (Vec<SearchUnit>, Vec<SearchUnit>) {
    let mut raw = Vec::with_capacity(text.chars().count());
    let mut compact = Vec::with_capacity(raw.capacity());

    for ch in text.chars() {
        if let Some(multi) = ch.to_pinyin_multi() {
            let mut readings: SmallVec<[&'static str; 4]> = SmallVec::new();
            for p in multi {
                push_normalized_readings(&mut readings, p.plain());
            }
            if !readings.is_empty() {
                compact.push(SearchUnit::Hanzi(readings.clone()));
                raw.push(SearchUnit::Hanzi(readings));
                continue;
            }
        }
        for lc in ch.to_lowercase() {
            raw.push(SearchUnit::Literal(lc));
            if lc.is_alphanumeric() {
                compact.push(SearchUnit::Literal(lc));
            }
        }
    }

    (raw, compact)
}

/// Contiguous substring match over `SearchUnit` sequences supporting:
/// - ASCII literals (English letters, digits)
/// - Hanzi pinyin initials (汉字首字母, e.g. `"wx"` -> `"微信"`, `"qywx"` -> `"企业微信"`)
/// - Hanzi digraph initials (`zh` / `ch` / `sh`, e.g. `"zhg"` -> `"中国"`, `"shz"` -> `"设置"`)
/// - Hanzi full pinyin & mixed full+initial prefixes (e.g. `"weixin"`, `"weix"`, `"we"`, `"qqyy"`)
/// - Polyphones / heteronyms (多音字, e.g. `"wyyyy"` and `"wyyyl"` both match `"网易云音乐"`)
///
/// Strictly O(N * M) worst-case via shared failure memoization.
fn matches_pinyin_units(units: &[SearchUnit], query_ascii: &[u8]) -> bool {
    let n = units.len();
    let m = query_ascii.len();
    if m == 0 {
        return true;
    }
    if n == 0 {
        return false;
    }

    let stride = m + 1;
    let mut failed = vec![false; n * stride];

    fn dfs(
        units: &[SearchUnit],
        q: &[u8],
        u: usize,
        pos: usize,
        stride: usize,
        failed: &mut [bool],
    ) -> bool {
        if pos == q.len() {
            return true;
        }
        if u == units.len() {
            return false;
        }
        let state_idx = u * stride + pos;
        if failed[state_idx] {
            return false;
        }

        let rem = &q[pos..];
        let matched = match &units[u] {
            SearchUnit::Literal(c) => {
                c.is_ascii()
                    && (*c as u8) == rem[0]
                    && dfs(units, q, u + 1, pos + 1, stride, failed)
            }
            SearchUnit::Hanzi(readings) => readings.iter().any(|&reading| {
                let rb = reading.as_bytes();
                if rb.is_empty() {
                    return false;
                }
                // 1. Full syllable (全拼)
                if rem.starts_with(rb) && dfs(units, q, u + 1, pos + rb.len(), stride, failed) {
                    return true;
                }
                // 2. Partial syllable prefix at end of query (live typing full pinyin, e.g. "we" -> "wei")
                if rb.starts_with(rem) {
                    return true;
                }
                // 3. Digraph initial ("zh" / "ch" / "sh")
                if rb.len() >= 2
                    && matches!(&rb[..2], b"zh" | b"ch" | b"sh")
                    && rem.starts_with(&rb[..2])
                    && dfs(units, q, u + 1, pos + 2, stride, failed)
                {
                    return true;
                }
                // 4. Single-letter initial (首字母)
                rem[0] == rb[0] && dfs(units, q, u + 1, pos + 1, stride, failed)
            }),
        };

        if !matched {
            failed[state_idx] = true;
        }
        matched
    }

    for start in 0..n {
        if dfs(units, query_ascii, start, 0, stride, &mut failed) {
            return true;
        }
    }
    false
}

/// Tests whether a single whitespace-free lowercase keyword matches `text`.
pub fn token_matches_text(text: &str, token_lower: &str) -> bool {
    if token_lower.is_empty() {
        return true;
    }
    if text.is_empty() {
        return false;
    }

    let text_lower = text.to_lowercase();
    if text_lower.contains(token_lower) {
        return true;
    }

    // Ignore Windows Chinese IME live composition syllable separators (e.g. "w'x", "wei'xin").
    let cleaned: String = token_lower
        .chars()
        .filter(|&c| c != '\'' && c != '’')
        .collect();
    if cleaned.is_empty() {
        return false;
    }
    if cleaned != token_lower && text_lower.contains(&cleaned) {
        return true;
    }

    // Compact alphanumeric/CJK substring (e.g. "vscode" matches "VS Code", "微信文件" matches "微信 - 文件").
    if cleaned.chars().all(char::is_alphanumeric) {
        let text_compact: String = text_lower
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect();
        if text_compact.contains(&cleaned) {
            return true;
        }
    }

    // Chinese Pinyin initials / full pinyin matching when token is ASCII alphanumeric and text has Hanzi.
    if cleaned.is_ascii()
        && cleaned.bytes().all(|b| b.is_ascii_alphanumeric())
        && text.chars().any(|c| c.to_pinyin_multi().is_some())
    {
        let (raw_units, compact_units) = build_search_units(text);
        let q_bytes = cleaned.as_bytes();
        return matches_pinyin_units(&raw_units, q_bytes)
            || matches_pinyin_units(&compact_units, q_bytes);
    }

    false
}

/// Tests whether `text` matches a full search query (space-separated LIKE tokens, AND semantics).
#[cfg(test)]
pub fn matches_text(text: &str, query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return true;
    }
    let query_lower = trimmed.to_lowercase();
    query_lower
        .split_whitespace()
        .all(|token| token_matches_text(text, token))
}

/// Tests whether `window` matches a search query across its window title, localized app display
/// name (`FileDescription` / `ProductName`), and executable process name (excluding the generic
/// UWP `ApplicationFrameHost.exe` wrapper).
pub fn matches_window(win: &WindowInfo, query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return true;
    }

    let query_lower = trimmed.to_lowercase();
    let searchable_process = if win
        .process_name
        .eq_ignore_ascii_case("ApplicationFrameHost.exe")
    {
        ""
    } else {
        win.process_name.as_str()
    };

    query_lower.split_whitespace().all(|token| {
        token_matches_text(&win.title, token)
            || token_matches_text(&win.app_name, token)
            || token_matches_text(searchable_process, token)
    })
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::RECT;

    use super::*;

    fn make_window(title: &str, app_name: &str, process_name: &str) -> WindowInfo {
        WindowInfo {
            hwnd: 1,
            pid: 100,
            process_name: process_name.to_string(),
            app_name: app_name.to_string(),
            exe_path: String::new(),
            class_name: "TestWnd".to_string(),
            title: title.to_string(),
            desktop_id: None,
            desktop_index: Some(0),
            is_pinned: false,
            is_minimized: false,
            is_maximized: false,
            restore_to_maximized: false,
            is_elevated: Some(false),
            cloaked_flags: 0,
            monitor_index: Some(0),
            rect: RECT::default(),
            visible_rect: RECT::default(),
            normal_rect: RECT::default(),
        }
    }

    #[test]
    fn empty_or_whitespace_query_matches_all_windows() {
        let win = make_window("微信", "微信", "WeChat.exe");
        assert!(matches_window(&win, ""));
        assert!(matches_window(&win, "   \t  "));
    }

    #[test]
    fn english_like_substring_search_is_case_insensitive_and_avoids_subsequence_noise() {
        let chrome = make_window(
            "GitHub - Google Chrome",
            "Google Chrome",
            "chrome.exe",
        );
        let explorer = make_window("Windows Explorer", "File Explorer", "explorer.exe");

        assert!(matches_window(&chrome, "chr"));
        assert!(matches_window(&chrome, "CHROME"));
        assert!(matches_window(&chrome, "git"));
        assert!(matches_window(&chrome, "goo"));
        assert!(matches_window(&chrome, "chrome github"));
        assert!(!matches_window(&chrome, "firefox"));
        // LIKE substring must NOT falsely match scattered subsequence 'w'...'x' in "Windows Explorer"
        assert!(!matches_window(&explorer, "wx"));
    }

    #[test]
    fn chinese_software_direct_and_pinyin_initial_search() {
        let wechat = make_window("微信", "微信", "WeChat.exe");
        let wechat_chat = make_window("Alice", "微信", "WeChat.exe");
        let wecom = make_window("企业微信", "企业微信", "WXWork.exe");
        let feishu = make_window("飞书 - 产品研发群", "飞书", "Feishu.exe");
        let notepad = make_window("无标题 - 记事本", "记事本", "notepad.exe");
        let taskmgr = make_window("任务管理器", "任务管理器", "Taskmgr.exe");

        // Direct Chinese LIKE
        assert!(matches_window(&wechat, "微信"));
        assert!(matches_window(&wecom, "微信"));
        assert!(matches_window(&feishu, "研发"));
        assert!(matches_window(&notepad, "记事本"));

        // Hanzi pinyin initials (首字母英文搜索)
        assert!(matches_window(&wechat, "w"));
        assert!(matches_window(&wechat, "wx"));
        assert!(matches_window(&wechat, "WX"));
        // Even when WeChat window title is English ("Alice"), localized app_name ("微信") matches "wx"!
        assert!(matches_window(&wechat_chat, "wx"));
        assert!(matches_window(&wechat_chat, "微信"));
        assert!(matches_window(&wechat_chat, "wx alice"));

        // Substring of initials (LIKE over initials)
        assert!(matches_window(&wecom, "qywx"));
        assert!(matches_window(&wecom, "wx"));
        assert!(matches_window(&wecom, "qy"));
        assert!(matches_window(&feishu, "fs"));
        assert!(matches_window(&feishu, "cpyfq"));
        assert!(matches_window(&notepad, "jsb"));
        assert!(matches_window(&notepad, "wbtjsb"));
        assert!(matches_window(&taskmgr, "rwglq"));
        assert!(matches_window(&taskmgr, "glq"));

        // Negative cases
        assert!(!matches_window(&feishu, "wx"));
        assert!(!matches_window(&notepad, "fs"));
        assert!(!matches_window(&taskmgr, "zz"));
    }

    #[test]
    fn full_pinyin_mixed_prefix_digraph_and_polyphone_matching() {
        let netease = make_window("网易云音乐", "网易云音乐", "cloudmusic.exe");
        let qq_music = make_window("QQ音乐", "QQ音乐", "QQMusic.exe");
        let settings = make_window("设置", "", "ApplicationFrameHost.exe");
        let green = make_window("绿色软件", "", "app.exe");

        // Polyphone '乐' (yue / le) both work
        assert!(matches_window(&netease, "wyyyy"));
        assert!(matches_window(&netease, "wyyyl"));
        assert!(matches_window(&netease, "yy"));
        assert!(matches_window(&netease, "yinyue"));
        assert!(matches_window(&netease, "wangyiyun"));

        // Mixed ASCII + Hanzi initials & full pinyin
        assert!(matches_window(&qq_music, "qqyy"));
        assert!(matches_window(&qq_music, "qqyl"));
        assert!(matches_window(&qq_music, "qqyinyue"));

        // Digraph initials ("sh" + "z") and single initials ("s" + "z")
        assert!(matches_window(&settings, "sz"));
        assert!(matches_window(&settings, "shz"));
        assert!(matches_window(&settings, "shezhi"));
        // Generic UWP host process_name should not pollute search
        assert!(!matches_window(&settings, "framehost"));
        assert!(!matches_window(&settings, "applicationframe"));

        // 'ü' normalized to both 'v' and 'u'
        assert!(matches_window(&green, "lsrj"));
        assert!(matches_window(&green, "lvse"));
        assert!(matches_window(&green, "luse"));
    }

    #[test]
    fn live_windows_ime_apostrophe_composing_and_compact_english_matching() {
        let wechat = make_window("微信 - 文件传输助手", "微信", "WeChat.exe");
        let vscode = make_window("main.rs - VS Code", "Visual Studio Code", "Code.exe");

        // Microsoft Pinyin IME puts apostrophes between syllables while composing
        assert!(matches_window(&wechat, "w'x"));
        assert!(matches_window(&wechat, "wei'xin"));
        assert!(matches_window(&wechat, "w'j'c's"));
        assert!(matches_window(&wechat, "wxwj"));

        // Compact substring ("vscode" matches "VS Code")
        assert!(matches_window(&vscode, "vscode"));
        assert!(matches_window(&vscode, "main code"));

        // Live incremental prefix typing for full pinyin ("w" -> "we" -> "wei" -> "weix" -> "weixi" -> "weixin")
        for prefix in ["w", "we", "wei", "weix", "weixi", "weixin"] {
            assert!(
                matches_text("微信", prefix),
                "expected '微信' to match live prefix '{prefix}'"
            );
        }
        // Must not match mid-syllable substrings like "ei"
        assert!(!matches_text("微信", "ei"));
        // Must not skip Hanzi characters ("文件系统" should not match "wx")
        assert!(!matches_text("文件系统", "wx"));
    }
}
