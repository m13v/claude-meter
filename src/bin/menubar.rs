use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use claude_meter::{api, cookies, dedupe_by_account, models::UsageSnapshot};
use serde::{Deserialize, Serialize};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

const POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TitleFormat {
    Long,
    Medium,
    Compact,
}

impl Default for TitleFormat {
    fn default() -> Self {
        TitleFormat::Long
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    title_format: TitleFormat,
    /// The browser whose account drives the menu-bar title. Defaults to the
    /// system's default http handler. User can override via the menu.
    #[serde(default)]
    preferred_browser: Option<String>,
}

enum AppEvent {
    Refreshing,
    Snapshots(Result<Vec<UsageSnapshot>, String>),
}

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    set_macos_accessory();

    let mut config = load_config();
    if config.preferred_browser.is_none() {
        if let Some(name) = default_browser_name() {
            config.preferred_browser = Some(name);
            save_config(&config);
        }
    }

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();

    let last_bridge: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
    {
        let proxy = proxy.clone();
        let last_bridge = last_bridge.clone();
        std::thread::spawn(move || bridge_loop(proxy, last_bridge));
    }
    {
        let last_bridge = last_bridge.clone();
        std::thread::spawn(move || poll_loop(proxy, refresh_rx, last_bridge));
    }

    let (menu, ids) = build_initial_menu();
    let mut tray_icon = Some(
        TrayIconBuilder::new()
            .with_title("Claude: …")
            .with_tooltip("claude-meter")
            .with_menu(Box::new(menu))
            .build()?,
    );

    let menu_channel = MenuEvent::receiver();
    let _tray_channel = TrayIconEvent::receiver();
    let mut current_ids = ids;
    let mut last_fetched: Option<DateTime<Local>> = None;
    let persisted = load_snapshots();
    let mut last_snaps: Option<Vec<UsageSnapshot>> = if persisted.is_empty() {
        None
    } else {
        if let Some(tray) = tray_icon.as_ref() {
            current_ids = render_menu_only(tray, &persisted, last_fetched, config.title_format);
        }
        Some(persisted)
    };
    let mut last_error: Option<String> = None;

    // Paint title immediately from whatever we loaded so the bar doesn't show "…" forever.
    if let Some(tray) = tray_icon.as_ref() {
        apply_title(
            tray,
            last_snaps.as_deref(),
            None,
            config.title_format,
            config.preferred_browser.as_deref(),
        );
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        let mut dirty = false;
        match event {
            Event::UserEvent(AppEvent::Refreshing) => {}
            Event::UserEvent(AppEvent::Snapshots(Ok(snaps))) => {
                last_fetched = Some(Local::now());
                last_error = None;
                let prev = last_snaps.clone().unwrap_or_default();
                let merged = merge_with_persisted(snaps, prev);
                save_snapshots(&merged);
                let numbers_changed = last_snaps
                    .as_ref()
                    .map(|old| !snaps_equal(old, &merged))
                    .unwrap_or(true);
                let accounts_changed = last_snaps
                    .as_ref()
                    .map(|old| account_set_changed(old, &merged))
                    .unwrap_or(true);
                last_snaps = Some(merged);
                // Only rebuild the menu when the account set itself changed (new
                // email, or stale↔fresh flip). Mid-flight percentage updates
                // reach the user on their next click via title + re-render.
                if accounts_changed {
                    if let (Some(tray), Some(s)) = (tray_icon.as_ref(), last_snaps.as_deref()) {
                        current_ids = render_menu_only(tray, s, last_fetched, config.title_format);
                    }
                }
                if numbers_changed {
                    dirty = true;
                }
            }
            Event::UserEvent(AppEvent::Snapshots(Err(e))) => {
                last_error = Some(e.clone());
                let (new_menu, new_ids) = build_error_menu(&e);
                if let Some(tray) = tray_icon.as_ref() {
                    let _ = tray.set_menu(Some(Box::new(new_menu)));
                }
                current_ids = new_ids;
                dirty = true;
            }
            _ => {}
        }

        while let Ok(menu_event) = menu_channel.try_recv() {
            if menu_event.id == current_ids.quit {
                tray_icon.take();
                *control_flow = ControlFlow::Exit;
                return;
            }
            if menu_event.id == current_ids.refresh {
                let _ = refresh_tx.send(());
                continue;
            }
            if let Some(&new_fmt) = current_ids.format_items.get(&menu_event.id) {
                if new_fmt != config.title_format {
                    config.title_format = new_fmt;
                    save_config(&config);
                    if let (Some(tray), Some(snaps)) =
                        (tray_icon.as_ref(), last_snaps.as_deref())
                    {
                        current_ids =
                            render_menu_only(tray, snaps, last_fetched, config.title_format);
                    }
                    dirty = true;
                }
                continue;
            }
            if let Some(url) = current_ids.open_urls.get(&menu_event.id) {
                let _ = std::process::Command::new("/usr/bin/open").arg(url).status();
                continue;
            }
            if let Some(key) = current_ids.forget_account.get(&menu_event.id).cloned() {
                if let Some(snaps) = last_snaps.as_mut() {
                    snaps.retain(|s| account_key(s) != key);
                    save_snapshots(snaps);
                    if let Some(tray) = tray_icon.as_ref() {
                        current_ids = render_menu_only(
                            tray,
                            snaps,
                            last_fetched,
                            config.title_format,
                        );
                    }
                    dirty = true;
                }
                continue;
            }
        }

        if dirty {
            if let Some(tray) = tray_icon.as_ref() {
                apply_title(
                    tray,
                    last_snaps.as_deref(),
                    last_error.as_deref(),
                    config.title_format,
                    config.preferred_browser.as_deref(),
                );
            }
        }
    });
}

fn render_menu_only(
    tray: &TrayIcon,
    snaps: &[UsageSnapshot],
    fetched: Option<DateTime<Local>>,
    fmt: TitleFormat,
) -> MenuIds {
    let (menu, ids) = build_menu(snaps, fetched, fmt);
    let _ = tray.set_menu(Some(Box::new(menu)));
    ids
}

fn apply_title(
    tray: &TrayIcon,
    snaps: Option<&[UsageSnapshot]>,
    error: Option<&str>,
    fmt: TitleFormat,
    preferred_browser: Option<&str>,
) {
    let segs = if error.is_some() {
        vec![TitleSeg { text: "Claude: !".into(), bg: None }]
    } else if let Some(s) = snaps {
        title_segments(fmt, s, preferred_browser)
    } else {
        vec![TitleSeg { text: "Claude: …".into(), bg: None }]
    };

    #[cfg(target_os = "macos")]
    let applied = {
        let m: Vec<macos_title::Segment> = segs
            .iter()
            .map(|s| macos_title::Segment {
                text: s.text.clone(),
                bg: s.bg,
            })
            .collect();
        macos_title::set_title(&m)
    };
    #[cfg(not(target_os = "macos"))]
    let applied = false;

    if !applied {
        let text: String = segs.iter().map(|s| s.text.as_str()).collect();
        let _ = tray.set_title(Some(&text));
    }
}

fn util_fingerprint(s: &UsageSnapshot) -> [Option<i64>; 4] {
    let f = |w: Option<&claude_meter::models::Window>| w.map(|w| (w.utilization * 10.0) as i64);
    match s.usage.as_ref() {
        Some(u) => [
            f(u.five_hour.as_ref()),
            f(u.seven_day.as_ref()),
            f(u.seven_day_sonnet.as_ref()),
            f(u.seven_day_opus.as_ref()),
        ],
        None => [None, None, None, None],
    }
}

fn snaps_equal(a: &[UsageSnapshot], b: &[UsageSnapshot]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            util_fingerprint(x) == util_fingerprint(y)
                && x.errors.len() == y.errors.len()
                && x.browser == y.browser
                && x.stale == y.stale
                && account_key(x) == account_key(y)
        })
}

/// Returns true when the *set of accounts* changed (different rows, different
/// browsers, or one went stale/fresh). Percentage changes alone return false,
/// so we avoid rebuilding the menu (and dismissing an open dropdown) just
/// because numbers ticked.
fn account_set_changed(a: &[UsageSnapshot], b: &[UsageSnapshot]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    let tuple = |s: &UsageSnapshot| {
        (
            s.browser.to_lowercase(),
            account_key(s).to_string(),
            s.stale,
        )
    };
    let mut ak: Vec<_> = a.iter().map(tuple).collect();
    let mut bk: Vec<_> = b.iter().map(tuple).collect();
    ak.sort();
    bk.sort();
    ak != bk
}

fn poll_loop(
    proxy: EventLoopProxy<AppEvent>,
    refresh_rx: mpsc::Receiver<()>,
    last_bridge: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = proxy.send_event(AppEvent::Snapshots(Err(format!(
                "could not start tokio runtime: {e}"
            ))));
            return;
        }
    };

    loop {
        let bridge_fresh = last_bridge
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|t| t.elapsed() < BRIDGE_FRESHNESS)
            .unwrap_or(false);

        if !bridge_fresh {
            let _ = proxy.send_event(AppEvent::Refreshing);
            let result = rt.block_on(fetch_all());
            let _ = proxy.send_event(AppEvent::Snapshots(result));
        }

        let _ = refresh_rx.recv_timeout(POLL_INTERVAL);
        while refresh_rx.try_recv().is_ok() {}
    }
}

const BRIDGE_PORT: u16 = 63762;
const BRIDGE_FRESHNESS: Duration = Duration::from_secs(120);

fn bridge_loop(
    proxy: EventLoopProxy<AppEvent>,
    last_bridge: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
) {
    use tiny_http::{Header, Method, Response, Server};

    let server = match Server::http(format!("127.0.0.1:{BRIDGE_PORT}")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bridge: could not bind 127.0.0.1:{BRIDGE_PORT}: {e}");
            return;
        }
    };

    let cors_origin: Header = "Access-Control-Allow-Origin: *".parse().unwrap();
    let cors_methods: Header =
        "Access-Control-Allow-Methods: POST, OPTIONS".parse().unwrap();
    let cors_headers: Header =
        "Access-Control-Allow-Headers: Content-Type".parse().unwrap();

    for mut req in server.incoming_requests() {
        if req.method() == &Method::Options {
            let r = Response::empty(204)
                .with_header(cors_origin.clone())
                .with_header(cors_methods.clone())
                .with_header(cors_headers.clone());
            let _ = req.respond(r);
            continue;
        }

        if req.method() != &Method::Post || req.url() != "/snapshots" {
            let r = Response::from_string("not found")
                .with_status_code(404)
                .with_header(cors_origin.clone());
            let _ = req.respond(r);
            continue;
        }

        // Prefer identifying the sending browser by looking up which local
        // process owns the peer socket; fall back to Sec-Ch-Ua / UA sniffing.
        let detected_browser = req
            .remote_addr()
            .and_then(peer_browser_by_port)
            .or_else(|| detect_browser_from_headers(req.headers()));

        let mut body = String::new();
        if let Err(e) = req.as_reader().read_to_string(&mut body) {
            let r = Response::from_string(format!("read error: {e}"))
                .with_status_code(400)
                .with_header(cors_origin.clone());
            let _ = req.respond(r);
            continue;
        }

        match serde_json::from_str::<Vec<UsageSnapshot>>(&body) {
            Ok(mut snaps) => {
                if let Some(name) = detected_browser.as_deref() {
                    for s in &mut snaps {
                        s.browser = name.to_string();
                    }
                }
                if let Ok(mut g) = last_bridge.lock() {
                    *g = Some(std::time::Instant::now());
                }
                let _ = proxy.send_event(AppEvent::Snapshots(Ok(snaps)));
                let r = Response::from_string("{\"ok\":true}")
                    .with_header(cors_origin.clone())
                    .with_header(
                        "Content-Type: application/json".parse::<Header>().unwrap(),
                    );
                let _ = req.respond(r);
            }
            Err(e) => {
                let r = Response::from_string(format!("parse error: {e}"))
                    .with_status_code(400)
                    .with_header(cors_origin.clone());
                let _ = req.respond(r);
            }
        }
    }
}

/// Look at who owns the TCP peer port on localhost and map that process's
/// executable to a browser name. Works regardless of what the extension puts in
/// its Sec-Ch-Ua, because we're asking the OS "which app holds this socket".
fn peer_browser_by_port(peer: &std::net::SocketAddr) -> Option<String> {
    use std::process::Command;
    let port = peer.port();
    let me = std::process::id();
    let out = Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:ESTABLISHED"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // First non-header row whose PID isn't us = the browser process.
    let peer_pid = text.lines().skip(1).find_map(|line| {
        let mut cols = line.split_whitespace();
        let _cmd = cols.next()?;
        let pid: u32 = cols.next()?.parse().ok()?;
        if pid == me { None } else { Some(pid) }
    })?;
    // `ps -o command=` returns the full command line (one line, unaffected by
    // spaces in app paths like "Google Chrome.app"). The first token is the
    // executable path; matching substrings on the whole line is fine.
    let ps = Command::new("/bin/ps")
        .args(["-p", &peer_pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    let cmdline = String::from_utf8_lossy(&ps.stdout).to_string();
    classify_browser_exe(&cmdline)
}

fn classify_browser_exe(path: &str) -> Option<String> {
    let p = path.to_lowercase();
    if p.contains("/arc.app/") { return Some("Arc".to_string()); }
    if p.contains("/google chrome.app/") || p.contains("/chrome.app/") {
        return Some("Chrome".to_string());
    }
    if p.contains("/brave browser.app/") || p.contains("/brave-browser") {
        return Some("Brave".to_string());
    }
    if p.contains("/microsoft edge.app/") { return Some("Edge".to_string()); }
    if p.contains("/chromium.app/") { return Some("Chromium".to_string()); }
    if p.contains("/opera.app/") { return Some("Opera".to_string()); }
    None
}

fn detect_browser_from_headers(headers: &[tiny_http::Header]) -> Option<String> {
    // Chromium browsers send Sec-Ch-Ua like:
    //   "Not_A Brand";v="8", "Chromium";v="120", "Google Chrome";v="120"
    //   "Not_A Brand";v="8", "Chromium";v="120", "Arc";v="..."
    //   "Microsoft Edge";v="120", ...
    //   "Brave";v="120", ...
    let sec_ch_ua = headers
        .iter()
        .find(|h| h.field.equiv("sec-ch-ua"))
        .map(|h| h.value.as_str().to_string());
    if let Some(v) = sec_ch_ua {
        let v_lc = v.to_lowercase();
        if v_lc.contains("\"arc\"") { return Some("Arc".to_string()); }
        if v_lc.contains("brave") { return Some("Brave".to_string()); }
        if v_lc.contains("microsoft edge") || v_lc.contains("\"edge\"") {
            return Some("Edge".to_string());
        }
        if v_lc.contains("google chrome") { return Some("Chrome".to_string()); }
        if v_lc.contains("chromium") { return Some("Chromium".to_string()); }
    }
    // Fall back to User-Agent (less reliable, but works for Edge/Opera/etc.).
    let ua = headers
        .iter()
        .find(|h| h.field.equiv("user-agent"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    let ua_lc = ua.to_lowercase();
    if ua_lc.contains("edg/") { return Some("Edge".to_string()); }
    if ua_lc.contains("opr/") || ua_lc.contains("opera/") { return Some("Opera".to_string()); }
    if ua_lc.contains("chrome/") { return Some("Chrome".to_string()); }
    None
}

/// Ask Launch Services which app handles https and return a short name
/// ("Chrome", "Arc", ...). Best-effort; returns None if we can't parse.
fn default_browser_name() -> Option<String> {
    use std::process::Command;
    let home = std::env::var("HOME").ok()?;
    let plist = format!(
        "{home}/Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure"
    );
    let out = Command::new("/usr/bin/defaults")
        .args(["read", &plist, "LSHandlers"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);

    // The output is a plist array of dicts. Find the dict whose
    // `LSHandlerURLScheme = https;` line is present, then pull LSHandlerRoleAll
    // from the same dict.
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '{' => { depth += 1; current.clear(); }
            '}' => {
                if depth == 1 && current.contains("LSHandlerURLScheme = https;") {
                    let bundle = current
                        .lines()
                        .find(|l| l.contains("LSHandlerRoleAll"))
                        .and_then(|l| l.split('=').nth(1))
                        .map(|v| {
                            v.trim()
                                .trim_end_matches(';')
                                .trim()
                                .trim_matches('"')
                                .to_string()
                        })?;
                    return bundle_id_to_name(bundle);
                }
                if depth > 0 { depth -= 1; }
                current.clear();
            }
            _ => {
                if depth > 0 { current.push(ch); }
            }
        }
    }
    None
}

fn bundle_id_to_name(id: String) -> Option<String> {
    Some(match id.as_str() {
        "com.google.chrome" | "com.google.Chrome" => "Chrome",
        "company.thebrowser.Browser" | "company.thebrowser.browser" => "Arc",
        "com.brave.browser" | "com.brave.Browser" => "Brave",
        "com.microsoft.edgemac" => "Edge",
        "com.apple.safari" | "com.apple.Safari" => "Safari",
        "com.operasoftware.Opera" => "Opera",
        _ => return None,
    }.to_string())
}

async fn fetch_all() -> Result<Vec<UsageSnapshot>, String> {
    let sessions = cookies::find_all_claude_sessions().map_err(|e| format!("{e:#}"))?;
    let mut snaps = Vec::with_capacity(sessions.len());
    for session in &sessions {
        match api::fetch_usage_snapshot(session).await {
            Ok(s) => snaps.push(s),
            Err(e) => eprintln!(
                "warn: {} fetch failed: {e:#}",
                session.browser.display_name()
            ),
        }
    }
    if snaps.is_empty() {
        Err("no browser session returned usage".to_string())
    } else {
        Ok(dedupe_by_account(snaps))
    }
}

struct MenuIds {
    refresh: MenuId,
    quit: MenuId,
    open_urls: HashMap<MenuId, String>,
    format_items: HashMap<MenuId, TitleFormat>,
    forget_account: HashMap<MenuId, String>,
}

impl MenuIds {
    fn bare(refresh: MenuId, quit: MenuId) -> Self {
        Self {
            refresh,
            quit,
            open_urls: HashMap::new(),
            format_items: HashMap::new(),
            forget_account: HashMap::new(),
        }
    }
}

fn build_initial_menu() -> (Menu, MenuIds) {
    let menu = Menu::new();
    let loading = MenuItem::new("loading…", false, None);
    let refresh = MenuItem::new("Refresh now", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&loading).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&refresh).unwrap();
    menu.append(&quit).unwrap();
    (menu, MenuIds::bare(refresh.id().clone(), quit.id().clone()))
}

fn build_error_menu(err: &str) -> (Menu, MenuIds) {
    let menu = Menu::new();
    let err_item = MenuItem::new(format!("error: {}", truncate(err, 80)), false, None);
    let refresh = MenuItem::new("Refresh now", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&err_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&refresh).unwrap();
    menu.append(&quit).unwrap();
    (menu, MenuIds::bare(refresh.id().clone(), quit.id().clone()))
}

fn build_menu(
    snaps: &[UsageSnapshot],
    fetched: Option<DateTime<Local>>,
    fmt: TitleFormat,
) -> (Menu, MenuIds) {
    let menu = Menu::new();
    let mut open_urls = HashMap::new();
    let mut format_items = HashMap::new();
    let mut forget_account: HashMap<MenuId, String> = HashMap::new();

    for (i, s) in snaps.iter().enumerate() {
        let label = account_label(s);
        let sub = Submenu::new(label, true);

        if let Some(u) = s.usage.as_ref() {
            if let Some(w) = u.five_hour.as_ref() {
                sub.append(&disabled(&format!(
                    "5-hour       {:>5.1}%{}",
                    w.utilization,
                    reset_suffix(w.resets_at)
                )))
                .ok();
            }
            if let Some(w) = u.seven_day.as_ref() {
                sub.append(&disabled(&format!(
                    "7-day all    {:>5.1}%{}",
                    w.utilization,
                    reset_suffix(w.resets_at)
                )))
                .ok();
            }
            if let Some(w) = u.seven_day_sonnet.as_ref() {
                sub.append(&disabled(&format!(
                    "7-day Sonnet {:>5.1}%",
                    w.utilization
                )))
                .ok();
            }
            if let Some(w) = u.seven_day_opus.as_ref() {
                sub.append(&disabled(&format!(
                    "7-day Opus   {:>5.1}%",
                    w.utilization
                )))
                .ok();
            }
        }

        if let Some(ov) = s.overage.as_ref() {
            let used = ov.used_credits.unwrap_or(0.0) / 100.0;
            let blocked = if ov.out_of_credits { "  BLOCKED" } else { "" };
            let line = match ov.monthly_credit_limit {
                Some(l) => {
                    let limit = l as f64 / 100.0;
                    let pct = if limit > 0.0 { used / limit * 100.0 } else { 0.0 };
                    format!(
                        "Extra        ${:.2} / ${:.2} ({:.0}%){}",
                        used, limit, pct, blocked
                    )
                }
                None => format!("Extra        ${:.2} used (no cap){}", used, blocked),
            };
            sub.append(&disabled(&line)).ok();
        }

        if !s.errors.is_empty() {
            sub.append(&PredefinedMenuItem::separator()).ok();
            for err in &s.errors {
                sub.append(&disabled(&format!("error: {}", truncate(err, 80))))
                    .ok();
            }
        }

        sub.append(&PredefinedMenuItem::separator()).ok();
        let open = MenuItem::new("Open claude.ai/settings/usage", true, None);
        open_urls.insert(
            open.id().clone(),
            "https://claude.ai/settings/usage".to_string(),
        );
        sub.append(&open).ok();

        if s.stale {
            let forget = MenuItem::new("Forget this account", true, None);
            forget_account.insert(forget.id().clone(), account_key(s).to_string());
            sub.append(&forget).ok();
        }

        menu.append(&sub).ok();
        if i + 1 < snaps.len() {
            menu.append(&PredefinedMenuItem::separator()).ok();
        }
    }

    menu.append(&PredefinedMenuItem::separator()).ok();

    // Title-format picker: each row previews the format applied to live data.
    let fmt_sub = Submenu::new("Menu bar style", true);
    for variant in [TitleFormat::Long, TitleFormat::Medium, TitleFormat::Compact] {
        let item = CheckMenuItem::new(
            format_title(variant, snaps),
            true,
            variant == fmt,
            None,
        );
        format_items.insert(item.id().clone(), variant);
        fmt_sub.append(&item).ok();
    }
    menu.append(&fmt_sub).ok();

    if let Some(t) = fetched {
        menu.append(&disabled(&format!("Updated {}", t.format("%H:%M:%S")))).ok();
    }
    let refresh = MenuItem::new("Refresh now", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&refresh).ok();
    menu.append(&quit).ok();

    (
        menu,
        MenuIds {
            refresh: refresh.id().clone(),
            quit: quit.id().clone(),
            open_urls,
            format_items,
            forget_account,
        },
    )
}

fn disabled(text: &str) -> MenuItem {
    MenuItem::new(text, false, None)
}

fn account_label(s: &UsageSnapshot) -> String {
    let who = s.account_email.as_deref().unwrap_or("(unknown)");
    let five = util_five(s);
    let seven = util_seven(s);
    let browser = pretty_browser(&s.browser);
    let warn = if s.errors.is_empty() { "" } else { "  !" };
    let stale = if s.stale {
        format!("  (stale, last {})", short_last_seen(s.fetched_at))
    } else {
        String::new()
    };
    format!("{} [{}]  [{:.0}% / {:.0}%]{}{}", who, browser, five, seven, warn, stale)
}

fn pretty_browser(b: &str) -> &str {
    match b.to_lowercase().as_str() {
        "chrome" | "google chrome" => "Chrome",
        "arc" => "Arc",
        "brave" | "brave-browser" => "Brave",
        "edge" | "microsoft edge" => "Edge",
        "safari" => "Safari",
        "chromium" => "Chromium",
        "extension" | "" => "browser",
        other => {
            if other.contains("chrome") { "Chrome" }
            else if other.contains("edge") { "Edge" }
            else if other.contains("brave") { "Brave" }
            else if other.contains("arc") { "Arc" }
            else { "browser" }
        }
    }
}

fn account_key(s: &UsageSnapshot) -> &str {
    s.account_email.as_deref().unwrap_or(s.org_uuid.as_str())
}

fn short_last_seen(t: chrono::DateTime<chrono::Utc>) -> String {
    let local: DateTime<Local> = t.into();
    let age = chrono::Utc::now().signed_duration_since(t);
    if age.num_days() >= 1 {
        local.format("%a %b %-d").to_string()
    } else {
        local.format("%H:%M").to_string()
    }
}

fn merge_with_persisted(
    fresh: Vec<UsageSnapshot>,
    persisted: Vec<UsageSnapshot>,
) -> Vec<UsageSnapshot> {
    // Key snapshots by (browser, account) so a POST from one browser
    // doesn't disturb another browser's entries.
    type Key = (String, String);
    let key_of = |s: &UsageSnapshot| -> Key {
        (s.browser.to_lowercase(), account_key(s).to_string())
    };
    let fresh_browsers: std::collections::HashSet<String> = fresh
        .iter()
        .map(|s| s.browser.to_lowercase())
        .collect();
    let mut by_key: std::collections::HashMap<Key, UsageSnapshot> =
        std::collections::HashMap::new();
    for mut s in fresh {
        s.stale = false;
        let k = key_of(&s);
        match by_key.get(&k) {
            None => { by_key.insert(k, s); }
            Some(existing) if prefer(&s, existing) => { by_key.insert(k, s); }
            _ => {}
        }
    }
    let stale_cutoff = chrono::Utc::now() - chrono::Duration::hours(2);
    for mut old in persisted {
        let k = key_of(&old);
        if by_key.contains_key(&k) {
            continue;
        }
        if old.fetched_at < stale_cutoff {
            continue;
        }
        // Only mark as stale if we just received a post from the SAME browser
        // and the account wasn't in it. Entries from other browsers keep
        // their previous state (don't get demoted to stale just because a
        // different browser posted).
        if fresh_browsers.contains(&old.browser.to_lowercase()) {
            old.stale = true;
        }
        by_key.insert(k, old);
    }
    by_key.into_values().collect()
}

fn prefer(a: &UsageSnapshot, b: &UsageSnapshot) -> bool {
    // Prefer the snapshot with a usage body and the freshest fetch.
    match (a.usage.is_some(), b.usage.is_some()) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    a.fetched_at > b.fetched_at
}

fn snapshots_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("ClaudeMeter").join("snapshots.json"))
}

fn load_snapshots() -> Vec<UsageSnapshot> {
    let Some(path) = snapshots_path() else { return Vec::new() };
    let Ok(s) = std::fs::read_to_string(&path) else { return Vec::new() };
    let mut snaps: Vec<UsageSnapshot> = serde_json::from_str(&s).unwrap_or_default();
    for s in &mut snaps {
        s.stale = true;
    }
    snaps
}

fn save_snapshots(snaps: &[UsageSnapshot]) {
    let Some(path) = snapshots_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(snaps) {
        let _ = std::fs::write(&path, s);
    }
}

fn util_five(s: &UsageSnapshot) -> f64 {
    s.usage
        .as_ref()
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.utilization)
        .unwrap_or(0.0)
}

fn util_seven(s: &UsageSnapshot) -> f64 {
    s.usage
        .as_ref()
        .and_then(|u| u.seven_day.as_ref())
        .map(|w| w.utilization)
        .unwrap_or(0.0)
}

#[derive(Clone, Debug)]
struct TitleSeg {
    text: String,
    bg: Option<(u8, u8, u8)>,
}

fn bg_for(util: f64) -> Option<(u8, u8, u8)> {
    if util >= 100.0 {
        Some((215, 58, 73))
    } else if util >= 90.0 {
        Some((219, 118, 32))
    } else {
        None
    }
}

fn account_tag(s: &UsageSnapshot) -> String {
    s.account_email
        .as_deref()
        .and_then(|e| e.chars().next())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn title_segments(
    fmt: TitleFormat,
    snaps: &[UsageSnapshot],
    preferred_browser: Option<&str>,
) -> Vec<TitleSeg> {
    let live_all: Vec<&UsageSnapshot> = snaps.iter().filter(|s| !s.stale).collect();
    // When the user has a preferred browser and at least one snapshot matches,
    // show only that account in the title. Otherwise fall back to all live snaps.
    let live: Vec<&UsageSnapshot> = match preferred_browser {
        Some(want) => {
            let want_lc = want.to_lowercase();
            let filtered: Vec<&UsageSnapshot> = live_all
                .iter()
                .copied()
                .filter(|s| pretty_browser(&s.browser).to_lowercase() == want_lc)
                .collect();
            if filtered.is_empty() { live_all } else { filtered }
        }
        None => live_all,
    };
    let mut segs: Vec<TitleSeg> = Vec::new();
    if live.is_empty() {
        segs.push(TitleSeg {
            text: match fmt {
                TitleFormat::Long => "Claude: —".into(),
                _ => "—".into(),
            },
            bg: None,
        });
        return segs;
    }
    if live.len() == 1 {
        let s = live[0];
        let five = util_five(s);
        let seven = util_seven(s);
        match fmt {
            TitleFormat::Long => segs.push(TitleSeg { text: "Claude  5h ".into(), bg: None }),
            TitleFormat::Medium => segs.push(TitleSeg { text: "5h ".into(), bg: None }),
            TitleFormat::Compact => {}
        }
        segs.push(TitleSeg { text: format!("{:.0}%", five), bg: bg_for(five) });
        let sep = match fmt {
            TitleFormat::Long => "  ·  ",
            TitleFormat::Medium => " · ",
            TitleFormat::Compact => " · ",
        };
        segs.push(TitleSeg { text: sep.into(), bg: None });
        if matches!(fmt, TitleFormat::Long | TitleFormat::Medium) {
            segs.push(TitleSeg { text: "7d ".into(), bg: None });
        }
        segs.push(TitleSeg { text: format!("{:.0}%", seven), bg: bg_for(seven) });
    } else {
        let between = match fmt {
            TitleFormat::Long => "     ",
            TitleFormat::Medium => "    ",
            TitleFormat::Compact => "  ",
        };
        if matches!(fmt, TitleFormat::Long) {
            segs.push(TitleSeg { text: "Claude  ".into(), bg: None });
        }
        for (i, s) in live.iter().enumerate() {
            if i > 0 {
                segs.push(TitleSeg { text: between.into(), bg: None });
            }
            let tag = account_tag(s);
            segs.push(TitleSeg { text: format!("{}: ", tag), bg: None });
            let five = util_five(s);
            let seven = util_seven(s);
            match fmt {
                TitleFormat::Long | TitleFormat::Medium => {
                    segs.push(TitleSeg { text: "5h ".into(), bg: None });
                    segs.push(TitleSeg { text: format!("{:.0}%", five), bg: bg_for(five) });
                    segs.push(TitleSeg {
                        text: if matches!(fmt, TitleFormat::Long) { "  ·  ".into() } else { " · ".into() },
                        bg: None,
                    });
                    segs.push(TitleSeg { text: "7d ".into(), bg: None });
                    segs.push(TitleSeg { text: format!("{:.0}%", seven), bg: bg_for(seven) });
                }
                TitleFormat::Compact => {
                    segs.push(TitleSeg { text: format!("{:.0}", five), bg: bg_for(five) });
                    segs.push(TitleSeg { text: "·".into(), bg: None });
                    segs.push(TitleSeg { text: format!("{:.0}", seven), bg: bg_for(seven) });
                }
            }
        }
    }
    segs
}

fn format_title(fmt: TitleFormat, snaps: &[UsageSnapshot]) -> String {
    // Format preview in the style picker doesn't need preferred-browser filter;
    // it shows what the title would look like for the full data set.
    title_segments(fmt, snaps, None)
        .into_iter()
        .map(|s| s.text)
        .collect()
}

fn reset_suffix(at: Option<DateTime<chrono::Utc>>) -> String {
    match at {
        Some(t) => {
            let delta = t.signed_duration_since(chrono::Utc::now());
            let days = delta.num_days();
            let hrs = delta.num_hours() - days * 24;
            let mut parts = Vec::new();
            if days > 0 {
                parts.push(format!("{days}d"));
            }
            if hrs > 0 {
                parts.push(format!("{hrs}h"));
            }
            if parts.is_empty() {
                " · resets soon".to_string()
            } else {
                format!(" · resets in {}", parts.join(" "))
            }
        }
        None => String::new(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("ClaudeMeter").join("config.json"))
}

fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(s) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    serde_json::from_str(&s).unwrap_or_default()
}

fn save_config(cfg: &Config) {
    let Some(path) = config_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(&path, s);
    }
}

#[cfg(target_os = "macos")]
fn set_macos_accessory() {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::MainThreadMarker;
    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return,
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

#[cfg(target_os = "macos")]
mod macos_title {
    use std::cell::RefCell;
    use objc2::class;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::ClassType;
    use objc2_app_kit::{
        NSApplication, NSBackgroundColorAttributeName, NSColor, NSFont, NSFontAttributeName,
        NSForegroundColorAttributeName, NSStatusBarButton, NSView,
    };
    use objc2_foundation::{
        MainThreadMarker, NSAttributedString, NSDictionary, NSMutableAttributedString,
        NSMutableDictionary, NSString,
    };

    thread_local! {
        static BUTTON: RefCell<Option<Retained<NSStatusBarButton>>> = const { RefCell::new(None) };
        static LAST_FINGERPRINT: RefCell<Option<u64>> = const { RefCell::new(None) };
    }

    #[derive(Clone, Debug)]
    pub struct Segment {
        pub text: String,
        pub bg: Option<(u8, u8, u8)>,
    }

    fn fingerprint(segments: &[Segment]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        for s in segments {
            s.text.hash(&mut h);
            s.bg.hash(&mut h);
            0u8.hash(&mut h);
        }
        h.finish()
    }

    fn class_name(obj: &AnyObject) -> String {
        unsafe {
            let cls: *const AnyClass = msg_send![obj, class];
            if cls.is_null() {
                return String::new();
            }
            (*cls).name().to_string()
        }
    }

    fn find_button_in_view(view: &NSView) -> Option<Retained<NSStatusBarButton>> {
        unsafe {
            let btn_cls = class!(NSStatusBarButton);
            let view_obj: &AnyObject = view.as_ref();
            let is_kind: bool = msg_send![view_obj, isKindOfClass: btn_cls];
            if is_kind {
                let ptr: *mut NSStatusBarButton = (view as *const NSView) as *mut NSStatusBarButton;
                return Retained::retain(ptr);
            }
            let subs = view.subviews();
            let n = subs.count();
            for i in 0..n {
                let sv = subs.objectAtIndex(i);
                if let Some(b) = find_button_in_view(&sv) {
                    return Some(b);
                }
            }
        }
        None
    }

    fn acquire_button(mtm: MainThreadMarker) -> Option<Retained<NSStatusBarButton>> {
        let app = NSApplication::sharedApplication(mtm);
        let windows = app.windows();
        let n = windows.count();
        for i in 0..n {
            let win = unsafe { windows.objectAtIndex(i) };
            let win_any: &AnyObject = win.as_ref();
            let name = class_name(win_any);
            if !name.contains("Status") {
                continue;
            }
            if let Some(v) = win.contentView() {
                if let Some(btn) = find_button_in_view(&v) {
                    return Some(btn);
                }
            }
        }
        None
    }

    pub fn set_title(segments: &[Segment]) -> bool {
        let Some(mtm) = MainThreadMarker::new() else { return false };
        BUTTON.with(|slot| {
            let mut b = slot.borrow_mut();
            if b.is_none() {
                *b = acquire_button(mtm);
            }
            let Some(btn) = b.as_ref() else { return false };
            let fp = fingerprint(segments);
            let should_apply = LAST_FINGERPRINT.with(|f| {
                let mut f = f.borrow_mut();
                if *f == Some(fp) {
                    false
                } else {
                    *f = Some(fp);
                    true
                }
            });
            if !should_apply {
                return true;
            }
            let attr = build_attr(segments);
            unsafe {
                btn.setAttributedTitle(&attr);
            }
            true
        })
    }

    fn build_attr(segments: &[Segment]) -> Retained<NSMutableAttributedString> {
        unsafe {
            let full = NSMutableAttributedString::new();
            let font = NSFont::menuBarFontOfSize(0.0);
            for seg in segments {
                let dict: Retained<NSMutableDictionary<NSString, AnyObject>> =
                    NSMutableDictionary::new();
                let _: () =
                    msg_send![&*dict, setObject: &*font, forKey: NSFontAttributeName];
                if let Some((r, g, b)) = seg.bg {
                    let bg_c = NSColor::colorWithSRGBRed_green_blue_alpha(
                        r as f64 / 255.0,
                        g as f64 / 255.0,
                        b as f64 / 255.0,
                        1.0,
                    );
                    let fg_c = NSColor::whiteColor();
                    let _: () = msg_send![&*dict, setObject: &*bg_c, forKey: NSBackgroundColorAttributeName];
                    let _: () = msg_send![&*dict, setObject: &*fg_c, forKey: NSForegroundColorAttributeName];
                } else {
                    let fg_c = NSColor::labelColor();
                    let _: () = msg_send![&*dict, setObject: &*fg_c, forKey: NSForegroundColorAttributeName];
                }
                let ns_text = NSString::from_str(&seg.text);
                let dict_ns: &NSDictionary<NSString, AnyObject> = &*dict;
                let part = NSAttributedString::initWithString_attributes(
                    NSAttributedString::alloc(),
                    &ns_text,
                    Some(dict_ns),
                );
                let _: () = msg_send![&*full, appendAttributedString: &*part];
            }
            full
        }
    }
}
