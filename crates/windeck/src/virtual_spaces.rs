use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use win_platform::WindowInfo;

use crate::config::{append_log, config_dir, virtual_spaces_file_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpaceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuleId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchScope {
    /// Matches any window of the same application (used for single-window apps).
    App,
    /// Matches a specific window instance by app + stable title signature (used for multi-window & UWP apps).
    AppAndTitle {
        signature: String,
        last_title: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRule {
    pub id: RuleId,
    pub exe_path: String,
    pub process_name: String,
    pub class_name: String,
    pub scope: MatchScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LiveBinding {
    hwnd: isize,
    pid: u32,
    rule_id: RuleId,
    order_seq: u64,
}

#[derive(Debug, Clone)]
pub struct VirtualSpace {
    pub id: SpaceId,
    pub name: String,
    pub rules: Vec<WindowRule>,
    live_bindings: Vec<LiveBinding>,
    /// Live windows explicitly removed by the user during this session so `App` rules
    /// do not immediately pull them back before the window closes.
    excluded_live: HashSet<(isize, u32)>,
}

impl VirtualSpace {
    fn new(id: SpaceId, name: String) -> Self {
        Self {
            id,
            name,
            rules: Vec::new(),
            live_bindings: Vec::new(),
            excluded_live: HashSet::new(),
        }
    }

    pub fn contains_hwnd(&self, hwnd: isize) -> bool {
        self.live_bindings.iter().any(|b| b.hwnd == hwnd)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddWindowOutcome {
    Added,
    AlreadyPresent,
}

/// Manages WinDeck-internal logical virtual spaces (projection views over existing OS windows).
///
/// Windows added to a `VirtualSpace` are NEVER moved off their native Windows virtual desktop.
/// Runtime membership tracks exact `(hwnd, pid)` bindings so title changes never drop an active
/// window, while persisted rules allow windows to auto-home after closing or restarting WinDeck.
#[derive(Debug, Clone)]
pub struct VirtualSpaceStore {
    next_space_id: u64,
    next_rule_id: u64,
    next_order_seq: u64,
    spaces: Vec<VirtualSpace>,
}

impl Default for VirtualSpaceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualSpaceStore {
    pub fn new() -> Self {
        Self {
            next_space_id: 1,
            next_rule_id: 1,
            next_order_seq: 1,
            spaces: Vec::new(),
        }
    }

    pub fn load_or_default() -> Self {
        let path = virtual_spaces_file_path();
        match fs::read_to_string(&path) {
            Ok(raw) => match Self::from_json(&raw) {
                Ok(store) => store,
                Err(err) => {
                    append_log(&format!("VIRTUAL_SPACES parse error: {err}"));
                    Self::new()
                }
            },
            Err(_) => Self::new(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to_path(&virtual_spaces_file_path())
    }

    pub fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        } else {
            fs::create_dir_all(config_dir())?;
        }
        let json = self.to_json();
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &json)?;
        if fs::rename(&tmp_path, path).is_err() {
            fs::write(path, &json)?;
            let _ = fs::remove_file(&tmp_path);
        }
        Ok(())
    }

    pub fn spaces(&self) -> &[VirtualSpace] {
        &self.spaces
    }

    pub fn get(&self, id: SpaceId) -> Option<&VirtualSpace> {
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn index_of(&self, id: SpaceId) -> Option<usize> {
        self.spaces.iter().position(|s| s.id == id)
    }

    pub fn live_count(&self, id: SpaceId, live_windows: &[WindowInfo]) -> usize {
        let Some(space) = self.get(id) else {
            return 0;
        };
        let live_set: HashSet<isize> = live_windows.iter().map(|w| w.hwnd).collect();
        space
            .live_bindings
            .iter()
            .filter(|b| live_set.contains(&b.hwnd))
            .count()
    }

    pub fn ordered_windows_for_space(
        &self,
        id: SpaceId,
        live_windows: &[WindowInfo],
    ) -> Vec<WindowInfo> {
        let Some(space) = self.get(id) else {
            return Vec::new();
        };
        let by_hwnd: HashMap<isize, &WindowInfo> =
            live_windows.iter().map(|w| (w.hwnd, w)).collect();
        let mut sorted = space.live_bindings.clone();
        sorted.sort_by_key(|b| b.order_seq);

        let mut out = Vec::with_capacity(sorted.len());
        let mut seen = HashSet::with_capacity(sorted.len());
        for binding in sorted {
            if seen.insert(binding.hwnd) {
                if let Some(win) = by_hwnd.get(&binding.hwnd) {
                    out.push((*win).clone());
                }
            }
        }
        out
    }

    pub fn next_default_name(&self) -> String {
        let existing: HashSet<&str> = self.spaces.iter().map(|s| s.name.as_str()).collect();
        for n in 1usize..=9999 {
            let candidate = format!("空间 {n}");
            if !existing.contains(candidate.as_str()) {
                return candidate;
            }
        }
        format!("空间 {}", self.next_space_id)
    }

    pub fn create_space(&mut self, custom_name: Option<String>) -> SpaceId {
        let id = SpaceId(self.next_space_id);
        self.next_space_id = self.next_space_id.saturating_add(1);
        let name = custom_name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.next_default_name());
        self.spaces.push(VirtualSpace::new(id, name));
        id
    }

    pub fn rename_space(&mut self, id: SpaceId, new_name: &str) -> bool {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Some(space) = self.spaces.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        if space.name == trimmed {
            return false;
        }
        space.name = trimmed.to_string();
        true
    }

    pub fn delete_space(&mut self, id: SpaceId) -> Option<String> {
        let idx = self.spaces.iter().position(|s| s.id == id)?;
        let removed = self.spaces.remove(idx);
        Some(removed.name)
    }

    /// Reorders `id` to `target_index` (clamped to valid range). Returns `true` if order changed.
    pub fn move_space(&mut self, id: SpaceId, target_index: usize) -> bool {
        let Some(from_index) = self.spaces.iter().position(|s| s.id == id) else {
            return false;
        };
        if self.spaces.len() <= 1 {
            return false;
        }
        let clamped_target = target_index.min(self.spaces.len() - 1);
        if from_index == clamped_target {
            return false;
        }
        let space = self.spaces.remove(from_index);
        self.spaces.insert(clamped_target, space);
        true
    }

    fn alloc_rule_id(&mut self) -> RuleId {
        let id = RuleId(self.next_rule_id);
        self.next_rule_id = self.next_rule_id.saturating_add(1);
        id
    }

    fn alloc_order_seq(&mut self) -> u64 {
        let seq = self.next_order_seq;
        self.next_order_seq = self.next_order_seq.saturating_add(1);
        seq
    }

    /// Adds a live window into `space_id` and records a smart auto-homing rule.
    pub fn add_window(
        &mut self,
        space_id: SpaceId,
        win: &WindowInfo,
        all_windows: &[WindowInfo],
    ) -> AddWindowOutcome {
        let Some(space_idx) = self.spaces.iter().position(|s| s.id == space_id) else {
            return AddWindowOutcome::AlreadyPresent;
        };
        if self.spaces[space_idx].contains_hwnd(win.hwnd) {
            return AddWindowOutcome::AlreadyPresent;
        }

        let scope = choose_match_scope(win, all_windows);
        let rule_id = self.alloc_rule_id();
        let order_seq = self.alloc_order_seq();

        let rule = WindowRule {
            id: rule_id,
            exe_path: normalize_exe_path(&win.exe_path),
            process_name: normalize_process_name(&win.process_name),
            class_name: win.class_name.clone(),
            scope,
        };

        let space = &mut self.spaces[space_idx];
        space.excluded_live.remove(&(win.hwnd, win.pid));
        // Avoid piling duplicate `App` rules if an existing `App` rule for this app already exists
        let effective_rule_id = if matches!(rule.scope, MatchScope::App) {
            if let Some(existing) = space
                .rules
                .iter()
                .find(|r| matches!(r.scope, MatchScope::App) && app_matches(r, win))
            {
                existing.id
            } else {
                space.rules.push(rule);
                rule_id
            }
        } else {
            space.rules.push(rule);
            rule_id
        };

        space.live_bindings.push(LiveBinding {
            hwnd: win.hwnd,
            pid: win.pid,
            rule_id: effective_rule_id,
            order_seq,
        });
        AddWindowOutcome::Added
    }

    /// Removes a live window from `space_id` (and prunes its auto-homing rule unless still shared
    /// by another live window in the same space). Does NOT touch the real OS window or desktop.
    pub fn remove_window(&mut self, space_id: SpaceId, hwnd: isize, pid: u32) -> bool {
        let Some(space) = self.spaces.iter_mut().find(|s| s.id == space_id) else {
            return false;
        };
        let Some(pos) = space.live_bindings.iter().position(|b| b.hwnd == hwnd) else {
            return false;
        };
        let removed = space.live_bindings.remove(pos);
        space.excluded_live.insert((hwnd, if pid != 0 { pid } else { removed.pid }));

        let still_used_by_other = space
            .live_bindings
            .iter()
            .any(|b| b.rule_id == removed.rule_id);
        if !still_used_by_other {
            space.rules.retain(|r| r.id != removed.rule_id);
        }
        true
    }

    /// Synchronizes all virtual spaces against the latest enumerated window snapshot:
    /// 1. Keeps existing live `(hwnd, pid)` bindings unconditionally (even if window title changes
    ///    or the window is temporarily hidden while still alive).
    /// 2. Updates `last_title` on bound `AppAndTitle` rules so session-restored browsers/apps
    ///    re-match cleanly after restart.
    /// 3. Auto-homes newly seen windows matching persisted rules (`App` matches all non-excluded
    ///    windows of that app; each `AppAndTitle` rule claims at most one highest-scoring live window).
    ///
    /// Returns `true` if any persisted rule metadata (`last_title`) changed.
    pub fn sync<F>(&mut self, live_windows: &[WindowInfo], mut is_alive: F) -> bool
    where
        F: FnMut(isize, u32) -> bool,
    {
        let live_by_hwnd: HashMap<isize, &WindowInfo> =
            live_windows.iter().map(|w| (w.hwnd, w)).collect();

        let mut persisted_dirty = false;
        let mut next_seq = self.next_order_seq;

        for space in &mut self.spaces {
            // 1. Prune destroyed / recycled HWNDs from live_bindings and excluded_live
            space.live_bindings.retain(|b| match live_by_hwnd.get(&b.hwnd) {
                Some(win) => b.pid == win.pid || b.pid == 0 || win.pid == 0,
                None => is_alive(b.hwnd, b.pid),
            });
            space.excluded_live.retain(|&(hwnd, pid)| match live_by_hwnd.get(&hwnd) {
                Some(win) => pid == win.pid || pid == 0 || win.pid == 0,
                None => is_alive(hwnd, pid),
            });

            // 2. Refresh pid and latest title metadata for currently bound live windows
            for binding in &mut space.live_bindings {
                if let Some(win) = live_by_hwnd.get(&binding.hwnd) {
                    if binding.pid == 0 && win.pid != 0 {
                        binding.pid = win.pid;
                    }
                    if let Some(rule) = space.rules.iter_mut().find(|r| r.id == binding.rule_id) {
                        if let MatchScope::AppAndTitle { last_title, .. } = &mut rule.scope {
                            let cleaned = clean_title(&win.title);
                            if !cleaned.is_empty() && *last_title != cleaned {
                                *last_title = cleaned;
                                persisted_dirty = true;
                            }
                        }
                    }
                }
            }

            // 3. Auto-home unbound live windows according to persisted rules (in rule order)
            let mut bound_hwnds: HashSet<isize> =
                space.live_bindings.iter().map(|b| b.hwnd).collect();
            let mut bound_rules: HashSet<RuleId> =
                space.live_bindings.iter().map(|b| b.rule_id).collect();

            // 3a. First match `AppAndTitle` rules (more specific), each claiming at most 1 best live window
            for rule in &space.rules {
                if !matches!(rule.scope, MatchScope::AppAndTitle { .. }) {
                    continue;
                }
                if bound_rules.contains(&rule.id) {
                    continue;
                }
                let best = live_windows
                    .iter()
                    .filter(|w| {
                        !bound_hwnds.contains(&w.hwnd)
                            && !space.excluded_live.contains(&(w.hwnd, w.pid))
                    })
                    .filter_map(|w| score_title_rule_match(rule, w).map(|score| (score, w)))
                    .max_by_key(|&(score, _)| score);

                if let Some((_, win)) = best {
                    let seq = next_seq;
                    next_seq = next_seq.saturating_add(1);
                    space.live_bindings.push(LiveBinding {
                        hwnd: win.hwnd,
                        pid: win.pid,
                        rule_id: rule.id,
                        order_seq: seq,
                    });
                    bound_hwnds.insert(win.hwnd);
                    bound_rules.insert(rule.id);
                }
            }

            // 3b. Then match `App` rules against any remaining unbound non-excluded windows
            for rule in &space.rules {
                if !matches!(rule.scope, MatchScope::App) {
                    continue;
                }
                for win in live_windows {
                    if bound_hwnds.contains(&win.hwnd)
                        || space.excluded_live.contains(&(win.hwnd, win.pid))
                    {
                        continue;
                    }
                    if app_matches(rule, win) {
                        let seq = next_seq;
                        next_seq = next_seq.saturating_add(1);
                        space.live_bindings.push(LiveBinding {
                            hwnd: win.hwnd,
                            pid: win.pid,
                            rule_id: rule.id,
                            order_seq: seq,
                        });
                        bound_hwnds.insert(win.hwnd);
                    }
                }
            }
        }

        self.next_order_seq = next_seq;
        persisted_dirty
    }

    pub fn to_json(&self) -> String {
        let spaces_json: Vec<Value> = self
            .spaces
            .iter()
            .map(|space| {
                let rules_json: Vec<Value> = space
                    .rules
                    .iter()
                    .map(|rule| match &rule.scope {
                        MatchScope::App => json!({
                            "id": rule.id.0,
                            "exe_path": rule.exe_path,
                            "process_name": rule.process_name,
                            "class_name": rule.class_name,
                            "scope": "app",
                        }),
                        MatchScope::AppAndTitle {
                            signature,
                            last_title,
                        } => json!({
                            "id": rule.id.0,
                            "exe_path": rule.exe_path,
                            "process_name": rule.process_name,
                            "class_name": rule.class_name,
                            "scope": "app_and_title",
                            "signature": signature,
                            "last_title": last_title,
                        }),
                    })
                    .collect();
                json!({
                    "id": space.id.0,
                    "name": space.name,
                    "rules": rules_json,
                })
            })
            .collect();

        let root = json!({
            "version": 1,
            "next_space_id": self.next_space_id,
            "next_rule_id": self.next_rule_id,
            "spaces": spaces_json,
        });
        serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_json(raw: &str) -> Result<Self, String> {
        let root: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        let mut store = Self::new();

        let mut max_space_id = 0u64;
        let mut max_rule_id = 0u64;

        if let Some(spaces_arr) = root.get("spaces").and_then(|v| v.as_array()) {
            let mut seen_space_ids = HashSet::new();
            for item in spaces_arr {
                let Some(id_val) = item.get("id").and_then(|v| v.as_u64()) else {
                    continue;
                };
                if id_val == 0 || !seen_space_ids.insert(id_val) {
                    continue;
                }
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("空间 {id_val}"));
                max_space_id = max_space_id.max(id_val);

                let mut space = VirtualSpace::new(SpaceId(id_val), name);
                if let Some(rules_arr) = item.get("rules").and_then(|v| v.as_array()) {
                    for r_item in rules_arr {
                        let r_id = r_item
                            .get("id")
                            .and_then(|v| v.as_u64())
                            .filter(|&v| v > 0)
                            .unwrap_or_else(|| {
                                max_rule_id = max_rule_id.saturating_add(1);
                                max_rule_id
                            });
                        max_rule_id = max_rule_id.max(r_id);

                        let exe_path = r_item
                            .get("exe_path")
                            .and_then(|v| v.as_str())
                            .map(normalize_exe_path)
                            .unwrap_or_default();
                        let process_name = r_item
                            .get("process_name")
                            .and_then(|v| v.as_str())
                            .map(normalize_process_name)
                            .unwrap_or_default();
                        let class_name = r_item
                            .get("class_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if exe_path.is_empty() && process_name.is_empty() {
                            continue;
                        }

                        let scope_str = r_item
                            .get("scope")
                            .and_then(|v| v.as_str())
                            .unwrap_or("app");
                        let scope = if scope_str == "app_and_title"
                            || is_uwp_host(&process_name)
                        {
                            let signature = r_item
                                .get("signature")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .trim()
                                .to_string();
                            let last_title = r_item
                                .get("last_title")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .trim()
                                .to_string();
                            if signature.is_empty() && last_title.is_empty() {
                                continue;
                            }
                            let resolved_sig = if signature.is_empty() {
                                last_title.clone()
                            } else {
                                signature
                            };
                            let resolved_last = if last_title.is_empty() {
                                resolved_sig.clone()
                            } else {
                                last_title
                            };
                            MatchScope::AppAndTitle {
                                signature: resolved_sig,
                                last_title: resolved_last,
                            }
                        } else {
                            MatchScope::App
                        };

                        space.rules.push(WindowRule {
                            id: RuleId(r_id),
                            exe_path,
                            process_name,
                            class_name,
                            scope,
                        });
                    }
                }
                store.spaces.push(space);
            }
        }

        store.next_space_id = root
            .get("next_space_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(max_space_id.saturating_add(1));
        store.next_rule_id = root
            .get("next_rule_id")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(max_rule_id.saturating_add(1));

        Ok(store)
    }
}

fn normalize_exe_path(path: &str) -> String {
    path.trim().replace('/', "\\").to_ascii_lowercase()
}

fn normalize_process_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn is_uwp_host(process_lower: &str) -> bool {
    process_lower == "applicationframehost.exe"
}

fn is_generic_runtime(process_lower: &str) -> bool {
    matches!(
        process_lower,
        "java.exe" | "javaw.exe" | "python.exe" | "pythonw.exe" | "node.exe" | "electron.exe"
    )
}

fn is_known_multi_window_app(process_lower: &str) -> bool {
    matches!(
        process_lower,
        "chrome.exe"
            | "msedge.exe"
            | "firefox.exe"
            | "brave.exe"
            | "arc.exe"
            | "vivaldi.exe"
            | "opera.exe"
            | "code.exe"
            | "code - insiders.exe"
            | "cursor.exe"
            | "windsurf.exe"
            | "antigravity.exe"
            | "zed.exe"
            | "devenv.exe"
            | "idea64.exe"
            | "webstorm64.exe"
            | "pycharm64.exe"
            | "goland64.exe"
            | "clion64.exe"
            | "rider64.exe"
            | "rustrover64.exe"
            | "windowsterminal.exe"
            | "wt.exe"
            | "wezterm-gui.exe"
            | "alacritty.exe"
            | "powershell.exe"
            | "pwsh.exe"
            | "cmd.exe"
            | "explorer.exe"
            | "notepad.exe"
            | "notepad++.exe"
            | "obsidian.exe"
            | "applicationframehost.exe"
    )
}

fn is_vscode_family(process_lower: &str) -> bool {
    matches!(
        process_lower,
        "code.exe"
            | "code - insiders.exe"
            | "cursor.exe"
            | "windsurf.exe"
            | "antigravity.exe"
            | "obsidian.exe"
    )
}

fn is_jetbrains_family(process_lower: &str) -> bool {
    matches!(
        process_lower,
        "idea64.exe"
            | "webstorm64.exe"
            | "pycharm64.exe"
            | "goland64.exe"
            | "clion64.exe"
            | "rider64.exe"
            | "rustrover64.exe"
    )
}

fn is_browser_family(process_lower: &str) -> bool {
    matches!(
        process_lower,
        "chrome.exe"
            | "msedge.exe"
            | "firefox.exe"
            | "brave.exe"
            | "arc.exe"
            | "vivaldi.exe"
            | "opera.exe"
    )
}

fn same_app_identity(a: &WindowInfo, b: &WindowInfo) -> bool {
    let pa = normalize_process_name(&a.process_name);
    let pb = normalize_process_name(&b.process_name);
    if !pa.is_empty() && !pb.is_empty() {
        if pa != pb {
            return false;
        }
        if is_generic_runtime(&pa) {
            let ea = normalize_exe_path(&a.exe_path);
            let eb = normalize_exe_path(&b.exe_path);
            if !ea.is_empty() && !eb.is_empty() {
                return ea == eb;
            }
        }
        return true;
    }
    let ea = normalize_exe_path(&a.exe_path);
    let eb = normalize_exe_path(&b.exe_path);
    !ea.is_empty() && ea == eb
}

fn app_matches(rule: &WindowRule, win: &WindowInfo) -> bool {
    let win_proc = normalize_process_name(&win.process_name);
    let win_exe = normalize_exe_path(&win.exe_path);

    if !rule.process_name.is_empty() && !win_proc.is_empty() {
        if rule.process_name != win_proc {
            return false;
        }
        if is_generic_runtime(&rule.process_name)
            && !rule.exe_path.is_empty()
            && !win_exe.is_empty()
            && rule.exe_path != win_exe
        {
            return false;
        }
        if rule.process_name == "explorer.exe"
            && !rule.class_name.is_empty()
            && !win.class_name.is_empty()
            && !rule.class_name.eq_ignore_ascii_case(&win.class_name)
        {
            return false;
        }
        return true;
    }

    !rule.exe_path.is_empty() && rule.exe_path == win_exe
}

/// Strips leading unread/modified badges (e.g. `(3) `, `[2] `, `● `, `* `) and collapses whitespace.
fn clean_title(raw: &str) -> String {
    let mut s = raw.trim();

    loop {
        let prev = s;
        // Strip leading "(12) " or "[3] "
        if let Some(rest) = s.strip_prefix('(') {
            if let Some(end) = rest.find(')') {
                let inner = &rest[..end];
                if !inner.is_empty()
                    && inner.chars().all(|c| c.is_ascii_digit() || c == '+')
                {
                    s = rest[end + 1..].trim_start();
                }
            }
        } else if let Some(rest) = s.strip_prefix('[') {
            if let Some(end) = rest.find(']') {
                let inner = &rest[..end];
                if !inner.is_empty()
                    && inner.chars().all(|c| c.is_ascii_digit() || c == '+')
                {
                    s = rest[end + 1..].trim_start();
                }
            }
        }
        for prefix in ["● ", "• ", "* ", "🔴 ", "🔔 "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start();
            }
        }
        if s == prev {
            break;
        }
    }

    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_title_segments(title: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = title;
    loop {
        let next_sep = [" - ", " — ", " – "]
            .iter()
            .filter_map(|sep| rest.find(sep).map(|idx| (idx, sep.len())))
            .min_by_key(|&(idx, _)| idx);
        match next_sep {
            Some((idx, len)) => {
                let piece = rest[..idx].trim();
                if !piece.is_empty() {
                    parts.push(piece);
                }
                rest = &rest[idx + len..];
            }
            None => {
                let piece = rest.trim();
                if !piece.is_empty() {
                    parts.push(piece);
                }
                break;
            }
        }
    }
    parts
}

/// Extracts a stable window signature from `(process_name, title)` so multi-window apps
/// (like VS Code workspaces, JetBrains projects, browser windows, Explorer folders)
/// stay distinguishable even as open files or notification badges change.
pub fn extract_title_signature(process_name: &str, raw_title: &str) -> String {
    let proc_lower = normalize_process_name(process_name);
    let cleaned = clean_title(raw_title);
    if cleaned.is_empty() {
        return String::new();
    }

    let segments = split_title_segments(&cleaned);

    if is_vscode_family(&proc_lower) {
        // "main.rs - windeck - Visual Studio Code" -> "windeck"
        // "windeck - Visual Studio Code" -> "windeck"
        if segments.len() >= 3 {
            return segments[segments.len() - 2].to_string();
        }
        if segments.len() == 2 {
            return segments[0].to_string();
        }
    } else if is_jetbrains_family(&proc_lower) {
        // "windeck – switcher_view.rs" -> "windeck"
        if let Some(&first) = segments.first() {
            return first.to_string();
        }
    } else if is_browser_family(&proc_lower) {
        // Strip trailing browser suffix ("Google Chrome", "Microsoft Edge", "Mozilla Firefox", ...)
        if segments.len() >= 2 {
            return segments[..segments.len() - 1].join(" - ");
        }
    } else if proc_lower == "explorer.exe" || proc_lower == "devenv.exe" {
        if segments.len() >= 2 {
            return segments[..segments.len() - 1].join(" - ");
        }
    }

    cleaned
}

fn choose_match_scope(win: &WindowInfo, all_windows: &[WindowInfo]) -> MatchScope {
    let proc_lower = normalize_process_name(&win.process_name);
    let sibling_count = all_windows
        .iter()
        .filter(|w| same_app_identity(w, win))
        .count();

    let need_title = is_uwp_host(&proc_lower)
        || is_known_multi_window_app(&proc_lower)
        || sibling_count > 1;

    if need_title {
        let cleaned = clean_title(&win.title);
        let signature = extract_title_signature(&win.process_name, &win.title);
        if !signature.is_empty() || !cleaned.is_empty() {
            return MatchScope::AppAndTitle {
                signature: if signature.is_empty() {
                    cleaned.clone()
                } else {
                    signature
                },
                last_title: cleaned,
            };
        }
    }

    MatchScope::App
}

fn score_title_rule_match(rule: &WindowRule, win: &WindowInfo) -> Option<u32> {
    if !app_matches(rule, win) {
        return None;
    }
    let MatchScope::AppAndTitle {
        signature,
        last_title,
    } = &rule.scope
    else {
        return None;
    };

    let win_cleaned = clean_title(&win.title);
    let win_sig = extract_title_signature(&win.process_name, &win.title);
    if win_cleaned.is_empty() && win_sig.is_empty() {
        return None;
    }

    let rule_sig_low = signature.to_lowercase();
    let rule_last_low = last_title.to_lowercase();
    let win_sig_low = win_sig.to_lowercase();
    let win_clean_low = win_cleaned.to_lowercase();

    // 1. Exact stable signature match (e.g. VS Code workspace "windeck" even when active file changed)
    if !rule_sig_low.is_empty() && win_sig_low == rule_sig_low {
        return Some(100);
    }
    // 2. Exact latest cleaned title match (e.g. browser session restore reopening last active tab)
    if !rule_last_low.is_empty() && win_clean_low == rule_last_low {
        return Some(90);
    }
    // 3. Exact match between saved last_title's extracted signature and current signature
    if !rule_last_low.is_empty() {
        let last_sig_low = extract_title_signature(&rule.process_name, last_title).to_lowercase();
        if !last_sig_low.is_empty() && last_sig_low == win_sig_low {
            return Some(85);
        }
    }
    // 4. Meaningful token/substring containment for longer signatures (>= 4 chars)
    if rule_sig_low.chars().count() >= 4
        && (win_clean_low.contains(&rule_sig_low) || win_sig_low.contains(&rule_sig_low))
    {
        return Some(60);
    }

    None
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::RECT;

    use super::*;

    fn make_win(
        hwnd: isize,
        pid: u32,
        process_name: &str,
        exe_path: &str,
        title: &str,
    ) -> WindowInfo {
        WindowInfo {
            hwnd,
            pid,
            process_name: process_name.to_string(),
            app_name: String::new(),
            exe_path: exe_path.to_string(),
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
    fn single_window_app_auto_homes_after_relaunch_and_persists() {
        let mut store = VirtualSpaceStore::new();
        let work = store.create_space(Some("沟通".to_string()));

        let wechat_1 = make_win(100, 1000, "WeChat.exe", r"C:\Apps\WeChat\WeChat.exe", "微信");
        let all_1 = vec![wechat_1.clone()];
        assert_eq!(
            store.add_window(work, &wechat_1, &all_1),
            AddWindowOutcome::Added
        );
        assert_eq!(
            store.add_window(work, &wechat_1, &all_1),
            AddWindowOutcome::AlreadyPresent
        );

        // Simulate saving & restarting WinDeck, and WeChat reopening with a brand new HWND & PID
        let json = store.to_json();
        let mut reloaded = VirtualSpaceStore::from_json(&json).expect("valid json");
        let wechat_2 = make_win(
            999,
            5555,
            "WeChat.exe",
            r"C:\Apps\WeChat\WeChat.exe",
            "(3) 微信",
        );
        reloaded.sync(&[wechat_2.clone()], |_, _| false);

        let matched = reloaded.ordered_windows_for_space(work, &[wechat_2]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].hwnd, 999);
    }

    #[test]
    fn multi_window_vscode_distinguishes_workspaces_across_file_switches_and_restart() {
        let mut store = VirtualSpaceStore::new();
        let dev = store.create_space(Some("开发".to_string()));

        let vscode_a = make_win(
            10,
            2000,
            "Code.exe",
            r"C:\VSCode\Code.exe",
            "● main.rs - windeck - Visual Studio Code",
        );
        let vscode_b = make_win(
            20,
            2000,
            "Code.exe",
            r"C:\VSCode\Code.exe",
            "index.ts - other-app - Visual Studio Code",
        );
        let all_1 = vec![vscode_a.clone(), vscode_b.clone()];
        store.add_window(dev, &vscode_a, &all_1);

        // Save, restart WinDeck, and reopen both VS Code windows on different files
        let mut reloaded = VirtualSpaceStore::from_json(&store.to_json()).unwrap();
        let vscode_a_new = make_win(
            110,
            3000,
            "Code.exe",
            r"C:\VSCode\Code.exe",
            "switcher_view.rs - windeck - Visual Studio Code",
        );
        let vscode_b_new = make_win(
            120,
            3000,
            "Code.exe",
            r"C:\VSCode\Code.exe",
            "package.json - other-app - Visual Studio Code",
        );
        let all_2 = vec![vscode_a_new, vscode_b_new];
        reloaded.sync(&all_2, |_, _| false);

        let matched = reloaded.ordered_windows_for_space(dev, &all_2);
        let hwnds: Vec<isize> = matched.iter().map(|w| w.hwnd).collect();
        assert_eq!(hwnds, vec![110]);
    }

    #[test]
    fn live_browser_stays_bound_when_switching_tabs_and_remembers_latest_tab_on_reopen() {
        let mut store = VirtualSpaceStore::new();
        let research = store.create_space(Some("资料".to_string()));

        let chrome_1 = make_win(
            30,
            4000,
            "chrome.exe",
            r"C:\Chrome\chrome.exe",
            "Rust Docs - Google Chrome",
        );
        let chrome_2 = make_win(
            40,
            4000,
            "chrome.exe",
            r"C:\Chrome\chrome.exe",
            "YouTube - Google Chrome",
        );
        store.add_window(research, &chrome_1, &[chrome_1.clone(), chrome_2.clone()]);

        // User switches tab in chrome_1 to a totally different page while running -> MUST stay in space!
        let chrome_1_navigated = make_win(
            30,
            4000,
            "chrome.exe",
            r"C:\Chrome\chrome.exe",
            "GPUI Drag & Drop Guide - Google Chrome",
        );
        let snap_live = vec![chrome_1_navigated, chrome_2.clone()];
        let dirty = store.sync(&snap_live, |_, _| true);
        assert!(dirty);
        assert_eq!(
            store
                .ordered_windows_for_space(research, &snap_live)
                .iter()
                .map(|w| w.hwnd)
                .collect::<Vec<_>>(),
            vec![30]
        );

        // User restarts Chrome with session restore -> chrome_1 reopens on "GPUI Drag & Drop Guide"
        let mut reloaded = VirtualSpaceStore::from_json(&store.to_json()).unwrap();
        let chrome_1_reborn = make_win(
            300,
            9000,
            "chrome.exe",
            r"C:\Chrome\chrome.exe",
            "GPUI Drag & Drop Guide - Google Chrome",
        );
        let chrome_2_reborn = make_win(
            400,
            9000,
            "chrome.exe",
            r"C:\Chrome\chrome.exe",
            "YouTube - Google Chrome",
        );
        let snap_reborn = vec![chrome_1_reborn, chrome_2_reborn];
        reloaded.sync(&snap_reborn, |_, _| false);
        assert_eq!(
            reloaded
                .ordered_windows_for_space(research, &snap_reborn)
                .iter()
                .map(|w| w.hwnd)
                .collect::<Vec<_>>(),
            vec![300]
        );
    }

    #[test]
    fn uwp_application_frame_host_never_collides_across_different_uwp_apps() {
        let mut store = VirtualSpaceStore::new();
        let tools = store.create_space(Some("工具".to_string()));

        let calc = make_win(
            50,
            500,
            "ApplicationFrameHost.exe",
            r"C:\Windows\System32\ApplicationFrameHost.exe",
            "计算器",
        );
        store.add_window(tools, &calc, &[calc.clone()]);

        let mut reloaded = VirtualSpaceStore::from_json(&store.to_json()).unwrap();
        let settings = make_win(
            51,
            500,
            "ApplicationFrameHost.exe",
            r"C:\Windows\System32\ApplicationFrameHost.exe",
            "设置",
        );
        let calc_new = make_win(
            52,
            500,
            "ApplicationFrameHost.exe",
            r"C:\Windows\System32\ApplicationFrameHost.exe",
            "计算器",
        );
        let snap = vec![settings, calc_new];
        reloaded.sync(&snap, |_, _| false);

        let hwnds: Vec<isize> = reloaded
            .ordered_windows_for_space(tools, &snap)
            .iter()
            .map(|w| w.hwnd)
            .collect();
        assert_eq!(hwnds, vec![52]);
    }

    #[test]
    fn window_can_belong_to_multiple_spaces_and_removal_does_not_re_pull() {
        let mut store = VirtualSpaceStore::new();
        let s1 = store.create_space(Some("A".to_string()));
        let s2 = store.create_space(Some("B".to_string()));

        let spotify = make_win(
            70,
            700,
            "Spotify.exe",
            r"C:\Apps\Spotify.exe",
            "Spotify",
        );
        let snap = vec![spotify.clone()];
        store.add_window(s1, &spotify, &snap);
        store.add_window(s2, &spotify, &snap);
        assert!(store.get(s1).unwrap().contains_hwnd(70));
        assert!(store.get(s2).unwrap().contains_hwnd(70));

        // Remove from s1 -> stays in s2, and does NOT auto-home back into s1
        assert!(store.remove_window(s1, 70, 700));
        store.sync(&snap, |_, _| true);
        assert!(!store.get(s1).unwrap().contains_hwnd(70));
        assert!(store.get(s2).unwrap().contains_hwnd(70));
        assert!(store.get(s1).unwrap().rules.is_empty());
    }

    #[test]
    fn identical_multi_window_terminals_claim_only_one_instance_per_rule() {
        let mut store = VirtualSpaceStore::new();
        let ops = store.create_space(Some("运维".to_string()));

        let wt1 = make_win(
            81,
            800,
            "WindowsTerminal.exe",
            r"C:\WT\WindowsTerminal.exe",
            "PowerShell",
        );
        let wt2 = make_win(
            82,
            800,
            "WindowsTerminal.exe",
            r"C:\WT\WindowsTerminal.exe",
            "PowerShell",
        );
        let snap = vec![wt1.clone(), wt2.clone()];
        store.add_window(ops, &wt1, &snap);
        store.sync(&snap, |_, _| true);

        let hwnds: Vec<isize> = store
            .ordered_windows_for_space(ops, &snap)
            .iter()
            .map(|w| w.hwnd)
            .collect();
        assert_eq!(hwnds, vec![81]);
    }

    #[test]
    fn title_signature_extraction_handles_ides_browsers_explorer_and_badges() {
        assert_eq!(
            extract_title_signature("Code.exe", "● switcher_view.rs - windeck - Visual Studio Code"),
            "windeck"
        );
        assert_eq!(
            extract_title_signature("Cursor.exe", "windeck - Cursor"),
            "windeck"
        );
        assert_eq!(
            extract_title_signature("idea64.exe", "payments-api – OrderService.java"),
            "payments-api"
        );
        assert_eq!(
            extract_title_signature("chrome.exe", "(12) Pull Requests - Google Chrome"),
            "Pull Requests"
        );
        assert_eq!(
            extract_title_signature("explorer.exe", "windeck - 文件资源管理器"),
            "windeck"
        );
        assert_eq!(
            extract_title_signature("Obsidian.exe", "2026-11-24 - NotesVault - Obsidian v1.7.0"),
            "NotesVault"
        );
    }

    #[test]
    fn save_to_path_atomic_round_trip_and_space_lifecycle() {
        let mut store = VirtualSpaceStore::new();
        let s1 = store.create_space(None);
        let s2 = store.create_space(None);
        assert_eq!(store.get(s1).unwrap().name, "空间 1");
        assert_eq!(store.get(s2).unwrap().name, "空间 2");
        assert!(store.rename_space(s1, "  项目开发  "));
        assert_eq!(store.get(s1).unwrap().name, "项目开发");
        assert!(!store.rename_space(s1, "   "));

        let win = make_win(42, 420, "Notion.exe", r"C:\Apps\Notion.exe", "Roadmap");
        store.add_window(s1, &win, &[win.clone()]);

        let temp_path = std::env::temp_dir().join(format!(
            "windeck_vspaces_test_{}.json",
            std::process::id()
        ));
        store.save_to_path(&temp_path).expect("save json");
        let raw = fs::read_to_string(&temp_path).expect("read json");
        let _ = fs::remove_file(&temp_path);

        let mut loaded = VirtualSpaceStore::from_json(&raw).expect("parse json");
        assert_eq!(loaded.spaces().len(), 2);
        assert_eq!(loaded.get(s1).unwrap().name, "项目开发");
        loaded.sync(&[win.clone()], |_, _| true);
        assert_eq!(loaded.live_count(s1, &[win]), 1);
        assert_eq!(loaded.delete_space(s2), Some("空间 2".to_string()));
        assert_eq!(loaded.spaces().len(), 1);
    }

    #[test]
    fn move_space_reorders_cleanly_and_persists_through_json() {
        let mut store = VirtualSpaceStore::new();
        let s1 = store.create_space(Some("工作".to_string()));
        let s2 = store.create_space(Some("聊天".to_string()));
        let s3 = store.create_space(Some("文档".to_string()));

        // No-op & unknown id
        assert!(!store.move_space(s1, 0));
        assert!(!store.move_space(SpaceId(999), 1));

        // Move forward: [s1, s2, s3] -> [s2, s3, s1]
        assert!(store.move_space(s1, 99));
        let ids: Vec<SpaceId> = store.spaces().iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![s2, s3, s1]);

        // Move backward: [s2, s3, s1] -> [s3, s2, s1]
        assert!(store.move_space(s3, 0));
        let ids: Vec<SpaceId> = store.spaces().iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![s3, s2, s1]);

        // Persisted JSON preserves reordered array order
        let reloaded = VirtualSpaceStore::from_json(&store.to_json()).expect("round-trip json");
        let reloaded_ids: Vec<SpaceId> = reloaded.spaces().iter().map(|s| s.id).collect();
        assert_eq!(reloaded_ids, vec![s3, s2, s1]);
    }
}
