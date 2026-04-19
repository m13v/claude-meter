use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
}

enum AppEvent {
    Refreshing,
    Snapshots(Result<Vec<UsageSnapshot>, String>),
}

enum AnimState {
    Idle,
    Spinning { frame: usize },
    Flashing { frame: usize, until: Instant },
}

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    set_macos_accessory();

    let mut config = load_config();

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();

    std::thread::spawn(move || poll_loop(proxy, refresh_rx));

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
    let mut last_snaps: Option<Vec<UsageSnapshot>> = None;
    let mut last_error: Option<String> = None;
    let mut anim_state = AnimState::Spinning { frame: 0 };

    event_loop.run(move |event, _, control_flow| {
        match event {
            Event::UserEvent(AppEvent::Refreshing) => {
                anim_state = AnimState::Spinning { frame: 0 };
            }
            Event::UserEvent(AppEvent::Snapshots(Ok(snaps))) => {
                last_fetched = Some(Local::now());
                last_error = None;
                let changed = last_snaps
                    .as_ref()
                    .map(|old| !snaps_equal(old, &snaps))
                    .unwrap_or(true);
                last_snaps = Some(snaps);
                if let (Some(tray), Some(s)) = (tray_icon.as_ref(), last_snaps.as_deref()) {
                    current_ids = render_menu_only(tray, s, last_fetched, config.title_format);
                }
                anim_state = if changed {
                    AnimState::Flashing {
                        frame: 0,
                        until: Instant::now() + Duration::from_millis(1400),
                    }
                } else {
                    AnimState::Idle
                };
            }
            Event::UserEvent(AppEvent::Snapshots(Err(e))) => {
                last_error = Some(e.clone());
                let (new_menu, new_ids) = build_error_menu(&e);
                if let Some(tray) = tray_icon.as_ref() {
                    let _ = tray.set_menu(Some(Box::new(new_menu)));
                }
                current_ids = new_ids;
                anim_state = AnimState::Idle;
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
                anim_state = AnimState::Spinning { frame: 0 };
                continue;
            }
            if let Some(&new_fmt) = current_ids.format_items.get(&menu_event.id) {
                if new_fmt != config.title_format {
                    config.title_format = new_fmt;
                    save_config(&config);
                }
                if let (Some(tray), Some(snaps)) = (tray_icon.as_ref(), last_snaps.as_deref()) {
                    current_ids = render_menu_only(tray, snaps, last_fetched, config.title_format);
                }
                continue;
            }
            if let Some(url) = current_ids.open_urls.get(&menu_event.id) {
                let _ = std::process::Command::new("/usr/bin/open").arg(url).status();
            }
        }

        if let Some(tray) = tray_icon.as_ref() {
            tick_title(
                tray,
                &mut anim_state,
                last_snaps.as_deref(),
                last_error.as_deref(),
                config.title_format,
            );
        }

        let delay_ms = match anim_state {
            AnimState::Idle => 500,
            _ => 90,
        };
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(delay_ms));
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

fn tick_title(
    tray: &TrayIcon,
    anim: &mut AnimState,
    snaps: Option<&[UsageSnapshot]>,
    error: Option<&str>,
    fmt: TitleFormat,
) {
    if let AnimState::Flashing { until, .. } = anim {
        if Instant::now() >= *until {
            *anim = AnimState::Idle;
        }
    }
    let title = decorate_title(anim, snaps, error, fmt);
    let _ = tray.set_title(Some(&title));
    match anim {
        AnimState::Idle => {}
        AnimState::Spinning { frame } | AnimState::Flashing { frame, .. } => {
            *frame = frame.wrapping_add(1);
        }
    }
}

fn decorate_title(
    anim: &AnimState,
    snaps: Option<&[UsageSnapshot]>,
    error: Option<&str>,
    fmt: TitleFormat,
) -> String {
    let base = if error.is_some() {
        "Claude: !".to_string()
    } else if let Some(s) = snaps {
        format_title(fmt, s)
    } else {
        "Claude: …".to_string()
    };
    let warn = snaps.map(warning_prefix).unwrap_or("");
    match anim {
        AnimState::Idle => format!("{}{}", warn, base),
        AnimState::Spinning { frame } => {
            const FRAMES: &[&str] = &[
                "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
            ];
            let spin = FRAMES[*frame % FRAMES.len()];
            format!("{} {}{}", spin, warn, base)
        }
        AnimState::Flashing { frame, .. } => {
            let on = (*frame / 4) % 2 == 0;
            let marker = if on { "🔄 " } else { "   " };
            format!("{}{}{}", marker, warn, base)
        }
    }
}

fn warning_prefix(snaps: &[UsageSnapshot]) -> &'static str {
    let worst = snaps
        .iter()
        .flat_map(|s| {
            [
                s.usage.five_hour.as_ref(),
                s.usage.seven_day.as_ref(),
                s.usage.seven_day_sonnet.as_ref(),
                s.usage.seven_day_opus.as_ref(),
            ]
        })
        .flatten()
        .map(|w| w.utilization)
        .fold(0.0_f64, f64::max);
    if worst >= 100.0 {
        "🟥 "
    } else if worst >= 90.0 {
        "🟧 "
    } else {
        ""
    }
}

fn util_fingerprint(s: &UsageSnapshot) -> [Option<i64>; 4] {
    let f = |w: Option<&claude_meter::models::Window>| w.map(|w| (w.utilization * 10.0) as i64);
    [
        f(s.usage.five_hour.as_ref()),
        f(s.usage.seven_day.as_ref()),
        f(s.usage.seven_day_sonnet.as_ref()),
        f(s.usage.seven_day_opus.as_ref()),
    ]
}

fn snaps_equal(a: &[UsageSnapshot], b: &[UsageSnapshot]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| util_fingerprint(x) == util_fingerprint(y))
}

fn poll_loop(proxy: EventLoopProxy<AppEvent>, refresh_rx: mpsc::Receiver<()>) {
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
        let _ = proxy.send_event(AppEvent::Refreshing);
        let result = rt.block_on(fetch_all());
        let _ = proxy.send_event(AppEvent::Snapshots(result));

        let _ = refresh_rx.recv_timeout(POLL_INTERVAL);
        while refresh_rx.try_recv().is_ok() {}
    }
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
}

impl MenuIds {
    fn bare(refresh: MenuId, quit: MenuId) -> Self {
        Self {
            refresh,
            quit,
            open_urls: HashMap::new(),
            format_items: HashMap::new(),
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

    for (i, s) in snaps.iter().enumerate() {
        let label = account_label(s);
        let sub = Submenu::new(label, true);

        if let Some(w) = s.usage.five_hour.as_ref() {
            sub.append(&disabled(&format!(
                "5-hour       {:>5.1}%{}",
                w.utilization,
                reset_suffix(w.resets_at)
            )))
            .ok();
        }
        if let Some(w) = s.usage.seven_day.as_ref() {
            sub.append(&disabled(&format!(
                "7-day all    {:>5.1}%{}",
                w.utilization,
                reset_suffix(w.resets_at)
            )))
            .ok();
        }
        if let Some(w) = s.usage.seven_day_sonnet.as_ref() {
            sub.append(&disabled(&format!(
                "7-day Sonnet {:>5.1}%",
                w.utilization
            )))
            .ok();
        }
        if let Some(w) = s.usage.seven_day_opus.as_ref() {
            sub.append(&disabled(&format!(
                "7-day Opus   {:>5.1}%",
                w.utilization
            )))
            .ok();
        }

        if let Some(ov) = s.overage.as_ref() {
            let used = ov.used_credits / 100.0;
            let limit = ov.monthly_credit_limit as f64 / 100.0;
            let pct = if limit > 0.0 { used / limit * 100.0 } else { 0.0 };
            let blocked = if ov.out_of_credits { "  BLOCKED" } else { "" };
            sub.append(&disabled(&format!(
                "Extra        ${:.2} / ${:.2} ({:.0}%){}",
                used, limit, pct, blocked
            )))
            .ok();
        }

        sub.append(&PredefinedMenuItem::separator()).ok();
        let open = MenuItem::new("Open claude.ai/settings/usage", true, None);
        open_urls.insert(
            open.id().clone(),
            "https://claude.ai/settings/usage".to_string(),
        );
        sub.append(&open).ok();

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
        },
    )
}

fn disabled(text: &str) -> MenuItem {
    MenuItem::new(text, false, None)
}

fn account_label(s: &UsageSnapshot) -> String {
    let who = s.account_email.as_deref().unwrap_or("(unknown)");
    let five = s.usage.five_hour.as_ref().map(|w| w.utilization).unwrap_or(0.0);
    let seven = s.usage.seven_day.as_ref().map(|w| w.utilization).unwrap_or(0.0);
    format!("{}  [{:.0}% / {:.0}%]  via {}", who, five, seven, s.browser)
}

fn format_title(fmt: TitleFormat, snaps: &[UsageSnapshot]) -> String {
    let worst_five = snaps
        .iter()
        .filter_map(|s| s.usage.five_hour.as_ref().map(|w| w.utilization))
        .fold(0.0_f64, f64::max);
    let worst_seven = snaps
        .iter()
        .filter_map(|s| s.usage.seven_day.as_ref().map(|w| w.utilization))
        .fold(0.0_f64, f64::max);
    match fmt {
        TitleFormat::Long => format!("Claude  5h {:.0}%  ·  7d {:.0}%", worst_five, worst_seven),
        TitleFormat::Medium => format!("5h {:.0}% · 7d {:.0}%", worst_five, worst_seven),
        TitleFormat::Compact => format!("{:.0}% · {:.0}%", worst_five, worst_seven),
    }
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
