use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use claude_meter::{dedupe_by_account, models::UsageSnapshot, oauth};
use serde::{Deserialize, Serialize};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Sentry DSN for the `claude-meter` project in the Mediar org. Compiled in
/// because this is a desktop app; users can opt out via `CLAUDE_METER_NO_SENTRY=1`.
const SENTRY_DSN: &str = "https://2a67e355b17fd4e2da6cc2e135f765f8@o4507617161314304.ingest.us.sentry.io/4511372322209792";

/// Log file path under ~/Library/Logs/ClaudeMeter/menubar.log. Lazily opened
/// the first time `log_line` runs; appended to until the process exits. No
/// rotation — desktop app, low write volume, user can `truncate` if it grows.
static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|p| p.join("Library").join("Logs").join("ClaudeMeter"))
}

fn open_log_file() -> Option<std::fs::File> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("menubar.log"))
        .ok()
}

/// Emit a log line: stderr + log file + Sentry breadcrumb. Use `log_warn` for
/// anything user-visible (429s, alarms, fetch failures); use `log_error` for
/// things we want as Sentry events, not just breadcrumbs.
fn log_line(level: &str, msg: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let line = format!("{ts} [{level}] {msg}");
    eprintln!("{line}");
    if let Ok(mut guard) = LOG_FILE.lock() {
        if guard.is_none() {
            *guard = open_log_file();
        }
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
    let sentry_level = match level {
        "error" => sentry::Level::Error,
        "warn" => sentry::Level::Warning,
        _ => sentry::Level::Info,
    };
    sentry::add_breadcrumb(sentry::Breadcrumb {
        category: Some("menubar".into()),
        level: sentry_level,
        message: Some(msg.into()),
        ..Default::default()
    });
}

fn log_info(msg: &str) { log_line("info", msg); }
fn log_warn(msg: &str) { log_line("warn", msg); }
fn log_error(msg: &str) {
    log_line("error", msg);
    sentry::capture_message(msg, sentry::Level::Error);
}

/// Capture a real Sentry event (not just a breadcrumb) at the given level.
/// Use sparingly: for things we want to be able to query in Sentry, like
/// alarm fires or process startup, not for routine 429 backoffs.
fn log_capture(level: &str, msg: &str) {
    log_line(level, msg);
    let sentry_level = match level {
        "error" => sentry::Level::Error,
        "warn" => sentry::Level::Warning,
        _ => sentry::Level::Info,
    };
    sentry::capture_message(msg, sentry_level);
}

/// Smart adaptive polling for /api/oauth/usage. The endpoint is an internal
/// Anthropic surface with no published rate limit, and our token is also being
/// hit by the actual Claude Code CLI in parallel, so a fixed cadence is wrong:
/// too-fast triggers 429s, too-slow leaves the bar stale during active use.
///
/// Strategy (see `smart_interval`):
///   - HIGH-USE FAST PATH: if any window utilization ≥ 80%, poll every 90s so
///     the alarm threshold (95%) and the title number stay responsive.
///   - ACTIVITY FAST PATH: if the last snapshot's numbers changed, poll again
///     in 90s (active CLI session, numbers will keep moving).
///   - IDLE GEOMETRIC SLOWDOWN: when the snapshot is identical N polls in a
///     row, back off 180s → 240s → 320s → 420s → 600s. Reset on any change.
const POLL_MIN: Duration = Duration::from_secs(90);
const POLL_BASE: Duration = Duration::from_secs(180);
const POLL_MAX: Duration = Duration::from_secs(600);
/// Utilization (%) at or above which we switch to the fast cadence. Sits below
/// the alarm threshold so the user gets multiple ticks of warning before fire.
const HIGH_UTIL_FAST_POLL: f64 = 80.0;

/// Backoff schedule after Anthropic returns 429 *without* a `Retry-After`
/// header. The first 429 is often a per-minute bucket transient and clears
/// in 60-90s, so a long wait is overkill and leaves the menu bar showing
/// the error too long. Repeated 429s suggest the per-hour or per-day limit,
/// where we want to back off more aggressively. Index = number of
/// *consecutive* 429s already seen (saturating; the last entry repeats).
///
/// When the server DOES send `Retry-After`, we honor it (clamped) and the
/// ladder is bypassed entirely. Anthropic's `/api/oauth/usage` consistently
/// returns Retry-After in seconds when the soft cap is hit; ignoring it and
/// knocking every 5min just re-extends the window.
const RATE_LIMIT_BACKOFF_LADDER: &[Duration] = &[
    Duration::from_secs(75),
    Duration::from_secs(180),
    Duration::from_secs(300),
];

/// Lower bound on a server-supplied `Retry-After`. Anything shorter is
/// noise we'd rather absorb into the next natural poll. There is no upper
/// bound — if Anthropic says wait an hour, we wait an hour. Clamping below
/// the requested value just guarantees an extra 429 (which is worse than
/// the silence we'd be trying to avoid). The user can quit the app
/// manually if a header value ever goes pathological.
const RETRY_AFTER_MIN: Duration = Duration::from_secs(30);

/// Utilization (%) on the 5-hour rolling window at which the alarm fires.
const ALARM_THRESHOLD_DEFAULT: f64 = 95.0;

/// Effective alarm threshold. Set `CLAUDE_METER_TEST_BLINK=1` in the env to
/// drop the threshold to 0% — useful for verifying the visual blink and
/// dismiss button without actually burning through 95% of a real plan window.
fn alarm_threshold() -> f64 {
    if std::env::var("CLAUDE_METER_TEST_BLINK").is_ok() {
        0.0
    } else {
        ALARM_THRESHOLD_DEFAULT
    }
}

/// System sound played when the alarm fires. Sosumi is the classic Mac alert
/// tone — sharp enough to read as an alarm without sounding like a Slack ping.
const ALARM_SOUND_PATH: &str = "/System/Library/Sounds/Sosumi.aiff";

/// How many times to play the sound back-to-back. Three repetitions feel like
/// an alarm; one feels like a notification ping.
const ALARM_REPEATS: usize = 3;

/// Cadence of the menu-bar blink at/over the alarm threshold. 500ms reads as
/// "blinking", not "flickering", and is still fast enough to grab attention
/// from peripheral vision.
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// RGB for the "alert" phase of the blink and for the >=100% solid color.
const BLINK_RED: (u8, u8, u8) = (215, 58, 73);

/// The browser tag set by the OAuth source (`oauth::fetch_oauth_snapshot`).
/// Used to identify the active Claude Code CLI account vs cookie-sourced
/// snapshots from other browser logins.
const OAUTH_BROWSER_TAG: &str = "Claude Code";

/// Drop any snapshot that didn't come from the OAuth (active Claude Code CLI)
/// source. Match the browser tag exactly: "Claude Code" only. Compound tags
/// like "Arc, Claude Code" are stale rows from the old multi-source dedupe and
/// belong to other accounts, so they get dropped.
fn keep_active_only(snaps: Vec<UsageSnapshot>) -> Vec<UsageSnapshot> {
    snaps
        .into_iter()
        .filter(|s| s.browser == OAUTH_BROWSER_TAG)
        .collect()
}

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

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    title_format: TitleFormat,
    /// The browser whose account drives the menu-bar title. Defaults to the
    /// system's default http handler. User can override via the menu.
    #[serde(default)]
    preferred_browser: Option<String>,
    /// When true (default), play a sound and post a notification when 5-hour
    /// utilization first crosses ALARM_THRESHOLD in the current window. The
    /// alarm fires once per window; rolls over with `resets_at`.
    #[serde(default = "default_alarm_enabled")]
    alarm_enabled: bool,
}

fn default_alarm_enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            title_format: TitleFormat::default(),
            preferred_browser: None,
            alarm_enabled: true,
        }
    }
}

enum AppEvent {
    Refreshing,
    Snapshots(Result<Vec<UsageSnapshot>, String>),
    /// Fires on a fixed cadence from the blink ticker thread. Only does work
    /// when the visual alarm is active (utilization >= ALARM_THRESHOLD and the
    /// user hasn't dismissed it for the current window).
    BlinkTick,
}

fn main() -> Result<()> {
    // Sentry init before anything else so panics in startup are captured.
    // Held in a binding so the guard isn't dropped until the process exits.
    let _sentry_guard = if std::env::var("CLAUDE_METER_NO_SENTRY").is_err() {
        Some(sentry::init((
            SENTRY_DSN,
            sentry::ClientOptions {
                release: Some(format!("claude-meter@{}", env!("CARGO_PKG_VERSION")).into()),
                environment: Some(
                    if cfg!(debug_assertions) { "debug" } else { "release" }.into(),
                ),
                attach_stacktrace: true,
                send_default_pii: false,
                ..Default::default()
            },
        )))
    } else {
        None
    };
    // Heartbeat: capture a real Sentry event on every launch so we can confirm
    // the SDK in the installed binary is reaching Sentry. Cheap (one event per
    // process start, not per poll) and gives an unambiguous "is the wiring
    // alive?" signal in the Sentry UI.
    if _sentry_guard.is_some() {
        log_capture(
            "info",
            &format!("claude-meter v{} startup heartbeat", env!("CARGO_PKG_VERSION")),
        );
    } else {
        log_info(&format!(
            "claude-meter v{} starting (sentry disabled)",
            env!("CARGO_PKG_VERSION")
        ));
    }

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
    let blink_proxy = event_loop.create_proxy();
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>();

    // Bridge listener (port 63762) was the legacy cookie-source path that
    // accepted POSTs from the browser extension. Now that fetch_all is
    // OAuth-only and keep_active_only() drops anything tagged with a
    // browser other than "Claude Code", every bridge POST was discarded
    // anyway. We also saw the bridge holding duplicate FDs per long-lived
    // keepalive connection (Chrome + Brave each kept 2 FDs open). Removed.
    std::thread::spawn(move || poll_loop(proxy, refresh_rx));

    // Blink ticker. Sends BlinkTick on a fixed cadence regardless of state;
    // the event handler short-circuits when the visual alarm isn't active.
    // Cheap (2 events/sec, all integer work), and keeping the cadence steady
    // means turning the blink on/off doesn't require thread coordination.
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(BLINK_INTERVAL);
            if blink_proxy.send_event(AppEvent::BlinkTick).is_err() {
                // Event loop closed (app exiting) — stop ticking.
                break;
            }
        }
    });

    let (menu, ids) = build_initial_menu();
    let mut builder = TrayIconBuilder::new()
        .with_title("Claude: …")
        .with_tooltip("claude-meter")
        .with_menu(Box::new(menu));
    if let Some(icon) = load_menubar_icon() {
        builder = builder.with_icon(icon);
        #[cfg(target_os = "macos")]
        {
            builder = builder.with_icon_as_template(true);
        }
    }
    let mut tray_icon = Some(builder.build()?);

    let menu_channel = MenuEvent::receiver();
    let _tray_channel = TrayIconEvent::receiver();
    let mut current_ids = ids;
    let mut last_fetched: Option<DateTime<Local>> = None;
    // Visual-alarm state. `blink` carries what gets painted right now;
    // `blink_dismissed` is the latch the user flips from the menu to silence
    // the blink for the current 5-hour window. The latch clears automatically
    // when the window rolls over or utilization drops back below threshold,
    // so the next high reading re-arms the visual without persisting state to
    // disk.
    let mut blink = BlinkState::OFF;
    let mut blink_dismissed = false;
    let persisted = load_snapshots();
    // Persisted snapshots are loaded as "last available" but never trigger the
    // blink on their own. The visual alarm only fires from a FRESH snapshot
    // (Snapshots(Ok) branch). If we're in a 429 window at startup, the user
    // just sees the last known percentages with a "!" marker — no blinking
    // off stale data, no spam from app restarts.
    let mut last_snaps: Option<Vec<UsageSnapshot>> = if persisted.is_empty() {
        None
    } else {
        if let Some(tray) = tray_icon.as_ref() {
            current_ids = render_menu_only(
                tray,
                &persisted,
                last_fetched,
                config.title_format,
                config.alarm_enabled,
                blink.active,
            );
        }
        Some(persisted)
    };
    let mut last_error: Option<String> = None;
    // Tracks the `resets_at` of the 5-hour window we already fired the alarm
    // for. When the next snapshot's window has a different `resets_at`, we
    // know we've rolled into a new window and re-arm.
    let mut last_alarm_window: Option<chrono::DateTime<chrono::Utc>> = None;

    // Paint title immediately from whatever we loaded so the bar doesn't show "…" forever.
    if let Some(tray) = tray_icon.as_ref() {
        apply_title(
            tray,
            last_snaps.as_deref(),
            None,
            config.title_format,
            config.preferred_browser.as_deref(),
            blink,
        );
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        let mut dirty = false;
        match event {
            Event::UserEvent(AppEvent::Refreshing) => {}
            Event::UserEvent(AppEvent::Snapshots(Ok(snaps))) => {
                // Filter to active Claude Code CLI account only. Bridge POSTs
                // from the browser extension still arrive on this channel and
                // may carry other-browser cookie data we no longer want to
                // display.
                let snaps = keep_active_only(snaps);
                if snaps.is_empty() {
                    // Nothing to show; keep the old state instead of clearing.
                    return;
                }
                last_fetched = Some(Local::now());
                // Transitioning out of an error state must repaint the title
                // even if the percentages didn't move; otherwise the stale " !"
                // marker from the previous 429 stays pinned forever (the
                // `numbers_changed`-only dirty flag below would never fire when
                // utilization is flat across the backoff window).
                let error_cleared = last_error.is_some();
                last_error = None;
                // OAuth is now the only source and returns exactly one snapshot
                // for the active account. Replace state instead of dedupe-merging
                // with persisted rows, which would re-tag the browser as
                // "Claude Code, Arc" and then get dropped by keep_active_only on
                // the next load.
                let merged = snaps;
                save_snapshots(&merged);

                // Alarm: fire once per 5-hour window when utilization first
                // crosses the threshold. The `resets_at` of the window is the
                // identifier — when it changes, we're in a new window and
                // re-arm. Manual mute via the toggle short-circuits firing
                // but doesn't affect arming.
                //
                // Visual alarm (blinking title) tracks the same threshold but
                // has a separate lifecycle: it stays active for as long as
                // utilization is at/over 95% AND the user hasn't dismissed it
                // for the current window. The audio fires once; the visual
                // keeps signaling until the user acknowledges.
                let was_blink_active = blink.active;
                if let Some((util, resets_at)) = max_five_hour_utilization(&merged) {
                    let already_fired_this_window = match (last_alarm_window, resets_at) {
                        (Some(prev), Some(curr)) => prev == curr,
                        _ => false,
                    };

                    // Detect window rollover and reset both the audio re-arm
                    // and the visual-dismiss latch. We do this independently
                    // of the firing branch so a rollover at <95% still clears
                    // the latch for the next high reading.
                    if let Some(prev) = last_alarm_window {
                        if resets_at.map(|c| c != prev).unwrap_or(false) {
                            last_alarm_window = None;
                            blink_dismissed = false;
                        }
                    }

                    let threshold = alarm_threshold();
                    if util >= threshold && config.alarm_enabled {
                        if !already_fired_this_window {
                            log_capture(
                                "warn",
                                &format!(
                                    "alarm: 5h utilization {:.1}% >= {:.0}% — firing",
                                    util, threshold
                                ),
                            );
                            play_alarm_sound();
                            post_alarm_notification(util);
                            last_alarm_window = resets_at.or(Some(chrono::Utc::now()));
                        }
                        // Visual stays on until dismissed or util drops. When
                        // we're flipping from off→on, start on the red phase
                        // so the first paint is immediate red rather than
                        // waiting half a tick for the first BlinkTick.
                        let should_be_active = !blink_dismissed;
                        if should_be_active && !blink.active {
                            blink.red_phase = true;
                        }
                        blink.active = should_be_active;
                    } else {
                        // Below threshold (or alarm disabled): stop blinking
                        // and re-arm the dismiss latch so a future re-cross
                        // gets the visual back.
                        blink.active = false;
                        blink_dismissed = false;
                    }
                }

                let numbers_changed = last_snaps
                    .as_ref()
                    .map(|old| !snaps_equal(old, &merged))
                    .unwrap_or(true);
                let accounts_changed = last_snaps
                    .as_ref()
                    .map(|old| account_set_changed(old, &merged))
                    .unwrap_or(true);
                last_snaps = Some(merged);
                // Rebuild the menu on account-set changes OR when the visual
                // alarm flipped on/off, so the "Dismiss alarm" item appears
                // and disappears in lockstep with the blink. Mid-flight
                // percentage changes alone still skip the rebuild (we don't
                // want to dismiss an open dropdown for a tick of usage).
                let blink_visibility_changed = was_blink_active != blink.active;
                if accounts_changed || blink_visibility_changed {
                    if let (Some(tray), Some(s)) = (tray_icon.as_ref(), last_snaps.as_deref()) {
                        current_ids = render_menu_only(
                            tray,
                            s,
                            last_fetched,
                            config.title_format,
                            config.alarm_enabled,
                            blink.active,
                        );
                    }
                }
                if numbers_changed || error_cleared || blink_visibility_changed {
                    dirty = true;
                }
            }
            Event::UserEvent(AppEvent::Snapshots(Err(e))) => {
                last_error = Some(e.clone());
                // Only swap to the bare error menu if we have NO last-good
                // data. With data we keep the regular menu (alarm toggle,
                // account submenu, etc.) and let apply_title append a "!"
                // marker so the user still sees usage numbers and can mute
                // the alarm during a 429 backoff.
                if last_snaps.is_none() {
                    let (new_menu, new_ids) = build_error_menu(&e);
                    if let Some(tray) = tray_icon.as_ref() {
                        let _ = tray.set_menu(Some(Box::new(new_menu)));
                    }
                    current_ids = new_ids;
                }
                dirty = true;
            }
            Event::UserEvent(AppEvent::BlinkTick) => {
                // Toggle the blink phase and repaint, but only while the
                // visual alarm is active. We bypass the `dirty` flag and
                // paint directly so the cadence stays even — the dirty path
                // is gated on data changes which we don't have here.
                if blink.active {
                    blink.red_phase = !blink.red_phase;
                    if let Some(tray) = tray_icon.as_ref() {
                        apply_title(
                            tray,
                            last_snaps.as_deref(),
                            last_error.as_deref(),
                            config.title_format,
                            config.preferred_browser.as_deref(),
                            blink,
                        );
                    }
                }
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
                        current_ids = render_menu_only(
                            tray,
                            snaps,
                            last_fetched,
                            config.title_format,
                            config.alarm_enabled,
                            blink.active,
                        );
                    }
                    dirty = true;
                }
                continue;
            }
            if current_ids.alarm_toggle.as_ref() == Some(&menu_event.id) {
                config.alarm_enabled = !config.alarm_enabled;
                save_config(&config);
                // Disabling the sound also kills any visible blink — the
                // toggle is the user's holistic "alarm" preference. The
                // blink will re-arm on the next snapshot if the toggle goes
                // back on while utilization is still high.
                if !config.alarm_enabled && blink.active {
                    blink.active = false;
                    dirty = true;
                }
                if let (Some(tray), Some(snaps)) =
                    (tray_icon.as_ref(), last_snaps.as_deref())
                {
                    current_ids = render_menu_only(
                        tray,
                        snaps,
                        last_fetched,
                        config.title_format,
                        config.alarm_enabled,
                        blink.active,
                    );
                }
                continue;
            }
            if current_ids.alarm_test.as_ref() == Some(&menu_event.id) {
                play_alarm_sound();
                continue;
            }
            if current_ids.dismiss_alarm.as_ref() == Some(&menu_event.id) {
                // Latch the dismiss for the current 5-hour window and stop
                // the blink immediately. The latch clears on window rollover
                // or when utilization drops back below threshold (handled in
                // the snapshot branch), so the user doesn't have to remember
                // to "re-arm" anything.
                log_info("alarm dismissed by user — silencing blink for current window");
                blink_dismissed = true;
                blink.active = false;
                blink.red_phase = false;
                if let (Some(tray), Some(snaps)) =
                    (tray_icon.as_ref(), last_snaps.as_deref())
                {
                    current_ids = render_menu_only(
                        tray,
                        snaps,
                        last_fetched,
                        config.title_format,
                        config.alarm_enabled,
                        blink.active,
                    );
                }
                dirty = true;
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
                            config.alarm_enabled,
                            blink.active,
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
                    blink,
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
    alarm_enabled: bool,
    alarm_visual_active: bool,
) -> MenuIds {
    let (menu, ids) = build_menu(snaps, fetched, fmt, alarm_enabled, alarm_visual_active);
    let _ = tray.set_menu(Some(Box::new(menu)));
    ids
}

/// Visual state for the at-threshold alarm. Independent of the audio alarm:
/// the sound fires once when the window first crosses 95%, but the visual
/// keeps blinking until utilization drops back below threshold, the user
/// dismisses it, or the 5-hour window rolls over.
#[derive(Clone, Copy)]
struct BlinkState {
    /// Whether the blink should currently be expressed visually.
    active: bool,
    /// Toggles every `BLINK_INTERVAL`. When true, override every segment's bg
    /// to red; when false, strip all backgrounds for the "off" phase. This
    /// reads as a clear, full-text "back-and-forth" without losing the
    /// percentage number.
    red_phase: bool,
}

impl BlinkState {
    const OFF: BlinkState = BlinkState { active: false, red_phase: false };
}

fn apply_title(
    tray: &TrayIcon,
    snaps: Option<&[UsageSnapshot]>,
    error: Option<&str>,
    fmt: TitleFormat,
    preferred_browser: Option<&str>,
    blink: BlinkState,
) {
    // Title precedence:
    //   - have snaps: render them, even if the latest fetch errored. Append
    //     " !" so the user can tell the data is stale without losing the
    //     numbers entirely (avoids the bare "Claude: !" that hid usage during
    //     429 backoff).
    //   - no snaps + error: bare "Claude: !" (we have nothing else to show).
    //   - no snaps + no error: "Claude: …" (still loading).
    let mut segs = if let Some(s) = snaps {
        let mut segs = title_segments(fmt, s, preferred_browser);
        if error.is_some() {
            segs.push(TitleSeg { text: " !".into(), bg: None });
        }
        segs
    } else if error.is_some() {
        vec![TitleSeg { text: "Claude: !".into(), bg: None }]
    } else {
        vec![TitleSeg { text: "Claude: …".into(), bg: None }]
    };

    // Blink override: when active, force every segment's background to a
    // uniform red on the "on" phase and clear all backgrounds on the "off"
    // phase. We override rather than additively layer so the whole bar
    // visibly flips together, which is what the user asked for.
    if blink.active {
        let bg = if blink.red_phase { Some(BLINK_RED) } else { None };
        for s in segs.iter_mut() {
            s.bg = bg;
        }
    }

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

/// Pick the next poll interval based on what the last successful fetch showed.
/// See the `POLL_MIN`/`POLL_BASE`/`POLL_MAX` doc-comment for the strategy.
fn smart_interval(
    last_snaps: &[UsageSnapshot],
    new_snaps: &[UsageSnapshot],
    unchanged_streak: u32,
) -> Duration {
    // High-utilization fast path: if any window is in the danger zone, keep
    // the bar (and the alarm threshold check) responsive.
    let high_util = new_snaps.iter().any(|s| {
        let Some(u) = s.usage.as_ref() else { return false };
        let any = |w: Option<&claude_meter::models::Window>| {
            w.map(|w| w.utilization >= HIGH_UTIL_FAST_POLL).unwrap_or(false)
        };
        any(u.five_hour.as_ref())
            || any(u.seven_day.as_ref())
            || any(u.seven_day_sonnet.as_ref())
            || any(u.seven_day_opus.as_ref())
    });
    if high_util {
        return POLL_MIN;
    }
    // Activity fast path: if the numbers (or account set) changed since the
    // last good snapshot, the CLI is burning tokens — poll again soon.
    if !last_snaps.is_empty() && !snaps_equal(last_snaps, new_snaps) {
        return POLL_MIN;
    }
    // Idle geometric slowdown. unchanged_streak counts consecutive identical
    // snapshots *after* the first one, so streak=0 still gets POLL_BASE.
    match unchanged_streak {
        0..=1 => POLL_BASE,
        2 => Duration::from_secs(240),
        3 => Duration::from_secs(320),
        4 => Duration::from_secs(420),
        _ => POLL_MAX,
    }
}

fn poll_loop(
    proxy: EventLoopProxy<AppEvent>,
    refresh_rx: mpsc::Receiver<()>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log_error(&format!("could not start tokio runtime: {e}"));
            let _ = proxy.send_event(AppEvent::Snapshots(Err(format!(
                "could not start tokio runtime: {e}"
            ))));
            return;
        }
    };

    // Always poll OAuth on every tick. The previous bridge-freshness skip was
    // built for the multi-source era, where browser-extension POSTs were a
    // legitimate alternate source. Now that fetch_all is OAuth-only, suppressing
    // the OAuth call when an extension POST arrives just leaves the menu bar
    // showing stale numbers from the active CLI account.
    //
    // Rate-limit backoff: when fetch_all returns 429 we step through
    // RATE_LIMIT_BACKOFF_LADDER (consecutive-failure index). The first 429 is
    // usually a per-minute bucket and clears in ~75s; repeated 429s indicate
    // a longer-window cap and need a longer wait. A successful fetch resets
    // the counter. The Refresh menu button bypasses the wait via refresh_rx.
    let mut consecutive_429s: usize = 0;
    // Smart-interval state: last good snapshot + count of consecutive identical
    // polls. Cleared on any rate-limit (we don't trust those values).
    let mut last_snaps: Vec<UsageSnapshot> = Vec::new();
    let mut unchanged_streak: u32 = 0;

    // Honor any backoff deadline persisted from a previous run. If we
    // restarted in the middle of a server-driven Retry-After window, we owe
    // Anthropic the rest of that wait — polling immediately on startup would
    // burn an attempt and likely re-extend the lockout. The Refresh menu
    // item still wakes us early via refresh_rx, so the user has an explicit
    // override if they want one.
    if let Some(remaining) = load_backoff_remaining() {
        log_info(&format!(
            "respecting persisted backoff: {}s remaining (use Refresh to override)",
            remaining.as_secs()
        ));
        let _ = refresh_rx.recv_timeout(remaining);
        while refresh_rx.try_recv().is_ok() {}
        // Treat startup like the consecutive=1 state — we don't know how
        // deep the upstream lockout was, but we know one already happened.
        consecutive_429s = 1;
    }

    loop {
        let _ = proxy.send_event(AppEvent::Refreshing);
        let result = rt.block_on(fetch_all());
        let rate_limited = match &result {
            Err(e) => is_rate_limit_error(e),
            Ok(_) => false,
        };
        // Snapshot a copy *before* moving `result` into the event so the smart
        // interval can compare against last_snaps after the menu has already
        // been updated. Pull retry-after out here for the same reason.
        let new_snaps_copy: Option<Vec<UsageSnapshot>> = match &result {
            Ok(s) => Some(s.clone()),
            Err(_) => None,
        };
        let retry_after_hint: Option<Duration> = match &result {
            Err(e) => parse_retry_after_seconds(e)
                .map(|secs| Duration::from_secs(secs).max(RETRY_AFTER_MIN)),
            Ok(_) => None,
        };
        let _ = proxy.send_event(AppEvent::Snapshots(result));

        let wait = if rate_limited {
            // Prefer the server-supplied Retry-After when present. The
            // hardcoded ladder is only a fallback for 429s that don't carry
            // a header — Anthropic's `/api/oauth/usage` does carry one, and
            // ignoring it was burning attempts every 5min during a ~22min
            // window, which probably re-extended that window each cycle.
            let server_hint = retry_after_hint;
            let (dur, source) = match server_hint {
                Some(d) => (d, "server retry-after"),
                None => {
                    let idx = consecutive_429s.min(RATE_LIMIT_BACKOFF_LADDER.len() - 1);
                    (RATE_LIMIT_BACKOFF_LADDER[idx], "ladder")
                }
            };
            log_warn(&format!(
                "rate-limited by anthropic (consecutive={}); backing off for {}s ({})",
                consecutive_429s + 1,
                dur.as_secs(),
                source,
            ));
            // Persist the deadline so a restart mid-backoff doesn't poll
            // immediately and re-extend the upstream window. Cleared by
            // clear_backoff() on the first successful fetch.
            save_backoff_until(dur);
            consecutive_429s = consecutive_429s.saturating_add(1);
            dur
        } else {
            consecutive_429s = 0;
            clear_backoff();
            match new_snaps_copy {
                Some(new_snaps) => {
                    let interval = smart_interval(&last_snaps, &new_snaps, unchanged_streak);
                    if !last_snaps.is_empty() && snaps_equal(&last_snaps, &new_snaps) {
                        unchanged_streak = unchanged_streak.saturating_add(1);
                    } else {
                        unchanged_streak = 0;
                    }
                    last_snaps = new_snaps;
                    log_info(&format!(
                        "next poll in {}s (streak={}, accounts={})",
                        interval.as_secs(),
                        unchanged_streak,
                        last_snaps.len()
                    ));
                    interval
                }
                None => POLL_BASE,
            }
        };
        let _ = refresh_rx.recv_timeout(wait);
        while refresh_rx.try_recv().is_ok() {}
    }
}

/// Heuristic: an HTTP 429 from any of the upstream calls.
fn is_rate_limit_error(err: &str) -> bool {
    err.contains("429") || err.to_lowercase().contains("too many requests")
}

/// Extract the Retry-After value (seconds) embedded by `oauth::get_json` in
/// 429 error messages. The format we emit is `... (retry-after=NNNNs): ...`.
/// We pull it back out via a tiny string match rather than threading a
/// structured error type all the way up; the rest of the codebase already
/// treats fetch errors as `String`, so keeping that contract is cheaper than
/// reshaping the type for one signal.
fn parse_retry_after_seconds(err: &str) -> Option<u64> {
    let marker = "retry-after=";
    let start = err.find(marker)? + marker.len();
    let tail = &err[start..];
    let end = tail.find(|c: char| !c.is_ascii_digit())?;
    tail[..end].parse::<u64>().ok()
}

/// Play `Sosumi` ALARM_REPEATS times on a background thread so we don't block
/// the event loop. `afplay` exits when the sound finishes; sleeping between
/// invocations keeps the cadence steady.
fn play_alarm_sound() {
    std::thread::spawn(|| {
        for i in 0..ALARM_REPEATS {
            let _ = std::process::Command::new("/usr/bin/afplay")
                .arg(ALARM_SOUND_PATH)
                .status();
            if i + 1 < ALARM_REPEATS {
                std::thread::sleep(Duration::from_millis(120));
            }
        }
    });
}

/// Best-effort visual notification via `osascript`. The macOS notification
/// surfaces the alarm even if the user wasn't watching the menu bar.
fn post_alarm_notification(utilization: f64) {
    let pct = utilization.round() as i64;
    let body = format!(
        "Your 5-hour Claude usage just hit {pct}%. Wrap up or wait for the window to reset."
    );
    let script = format!(
        "display notification \"{body}\" with title \"Claude usage at {pct}%\" subtitle \"5-hour rolling window\""
    );
    std::thread::spawn(move || {
        let _ = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .status();
    });
}

/// Pull the highest 5-hour utilization across snapshots and the `resets_at`
/// of that row. The reset timestamp is used as a window identifier — when it
/// changes, we know we're in a new 5-hour window and re-arm the alarm.
fn max_five_hour_utilization(
    snaps: &[UsageSnapshot],
) -> Option<(f64, Option<chrono::DateTime<chrono::Utc>>)> {
    let mut best: Option<(f64, Option<chrono::DateTime<chrono::Utc>>)> = None;
    for s in snaps {
        let Some(usage) = s.usage.as_ref() else { continue };
        let Some(window) = usage.five_hour.as_ref() else { continue };
        if best.as_ref().map(|b| window.utilization > b.0).unwrap_or(true) {
            best = Some((window.utilization, window.resets_at));
        }
    }
    best
}

/// The menu-bar template PNG is baked into the binary at compile time. macOS
/// treats template images as masks and auto-inverts them for light/dark menu
/// bars, which is why the file has black pixels on a transparent background.
fn load_menubar_icon() -> Option<tray_icon::Icon> {
    const BYTES: &[u8] = include_bytes!("../../assets/menubar-template@2x.png");
    let decoder = png::Decoder::new(BYTES);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    // Normalize to RGBA8 by converting the decoded slice. The SVG → PNG
    // pipeline always gives us RGBA8 here, but handle the common variants
    // for defensiveness.
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(info.width as usize * info.height as usize * 4);
            for px in buf[..info.buffer_size()].chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            out
        }
        _ => return None,
    };
    tray_icon::Icon::from_rgba(rgba, info.width, info.height).ok()
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
    // OAuth-only: the menu bar surfaces the active Claude Code CLI account,
    // not whatever else might be logged into the user's browsers. Cookie-source
    // multi-account aggregation was removed because users found it confusing
    // to see another account's quota in the bar (e.g. mediar.ai showing 93%
    // while the active CLI account was at 32%).
    match oauth::fetch_oauth_snapshot().await {
        Ok(s) => Ok(vec![s]),
        Err(e) => {
            log_warn(&format!("oauth fetch failed: {e:#}"));
            Err(format!("oauth: {e:#}"))
        }
    }
}

struct MenuIds {
    refresh: MenuId,
    quit: MenuId,
    /// "Alarm sound at 95%" toggle. Absent in `bare` menus (initial / error)
    /// because they're rebuilt on the next successful poll.
    alarm_toggle: Option<MenuId>,
    /// "Test alarm sound" — fires the alarm on demand.
    alarm_test: Option<MenuId>,
    /// "Dismiss alarm" item, only present while the visual alarm (blinking
    /// title) is active. Clicking it silences the blink for the current
    /// 5-hour window; the blink re-arms automatically when the window rolls.
    dismiss_alarm: Option<MenuId>,
    open_urls: HashMap<MenuId, String>,
    format_items: HashMap<MenuId, TitleFormat>,
    forget_account: HashMap<MenuId, String>,
}

impl MenuIds {
    fn bare(refresh: MenuId, quit: MenuId) -> Self {
        Self {
            refresh,
            quit,
            alarm_toggle: None,
            alarm_test: None,
            dismiss_alarm: None,
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
    alarm_enabled: bool,
    alarm_visual_active: bool,
) -> (Menu, MenuIds) {
    let menu = Menu::new();
    let mut open_urls = HashMap::new();
    let mut format_items = HashMap::new();
    let mut forget_account: HashMap<MenuId, String> = HashMap::new();

    // When the visual alarm is firing (utilization >= 95%, not yet dismissed
    // for the current 5-hour window), the very first item is a one-click
    // "Dismiss alarm" so the user can stop the blinking without hunting through
    // submenus.
    let dismiss_alarm = if alarm_visual_active {
        let item = MenuItem::new("Dismiss alarm", true, None);
        let id = item.id().clone();
        menu.append(&item).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        Some(id)
    } else {
        None
    };

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

    // Alarm controls: a single toggle ("Sound on (alerts at 95%)" / "Sound
    // off") plus a "Test alarm sound" item so the user can verify the sound
    // works without waiting to actually hit 95%. Default on; persisted in
    // config.json under `alarm_enabled`.
    let alarm_toggle = CheckMenuItem::new(
        format!(
            "Sound alarm at {}% (5h window)",
            alarm_threshold() as i64
        ),
        true,
        alarm_enabled,
        None,
    );
    let alarm_test = MenuItem::new("Test alarm sound", true, None);
    menu.append(&alarm_toggle).ok();
    menu.append(&alarm_test).ok();

    let refresh = MenuItem::new("Refresh now", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&refresh).ok();
    menu.append(&quit).ok();

    (
        menu,
        MenuIds {
            refresh: refresh.id().clone(),
            quit: quit.id().clone(),
            alarm_toggle: Some(alarm_toggle.id().clone()),
            alarm_test: Some(alarm_test.id().clone()),
            dismiss_alarm,
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
    let merged: Vec<UsageSnapshot> = by_key.into_values().collect();
    // Final pass: collapse rows that point at the same account but have
    // different `browser` keys, e.g. an old persisted "Arc" row for
    // mattdiak@... alongside a new "Claude Code, Arc" row from the OAuth
    // refactor. Sort fresh-first so dedupe_by_account picks fresh as the
    // winner and back-fills any subscription/overage from the stale duplicate.
    let mut sorted = merged;
    sorted.sort_by_key(|s| (s.stale, std::cmp::Reverse(s.fetched_at)));
    dedupe_by_account(sorted)
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
    // Drop any persisted entries from the old multi-account era that aren't
    // tagged with the OAuth source. They'd otherwise resurface as stale rows.
    snaps = keep_active_only(snaps);
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

// Backoff deadline persistence. When Anthropic gives us a `Retry-After`, we
// drop the unix-epoch second of the deadline into a tiny file next to
// snapshots.json. On the next startup we check it before polling so a restart
// (manual quit, crash, install) doesn't knock the API and re-extend the
// window. Cleared on the first successful fetch.
fn backoff_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("ClaudeMeter").join("backoff_until.txt"))
}

fn save_backoff_until(dur_from_now: Duration) {
    let Some(path) = backoff_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let deadline = match std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
    {
        Ok(d) => d + dur_from_now,
        Err(_) => return,
    };
    let _ = std::fs::write(&path, deadline.as_secs().to_string());
}

fn load_backoff_remaining() -> Option<Duration> {
    let path = backoff_path()?;
    let s = std::fs::read_to_string(&path).ok()?;
    let deadline_secs: u64 = s.trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if deadline_secs > now {
        Some(Duration::from_secs(deadline_secs - now))
    } else {
        // Already past; clean up so it doesn't get re-loaded next time.
        let _ = std::fs::remove_file(&path);
        None
    }
}

fn clear_backoff() {
    if let Some(path) = backoff_path() {
        let _ = std::fs::remove_file(&path);
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
        Some(BLINK_RED)
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
    // First pass: only non-stale snapshots, filtered by preferred browser if
    // available. This is the happy path during normal operation.
    let live_all: Vec<&UsageSnapshot> = snaps.iter().filter(|s| !s.stale).collect();
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

    // Fallback: when no fresh snapshot is available (post-restart with only
    // persisted data, or an extended 429 backoff), show the last-known
    // numbers from any stale snapshot rather than the uninformative "—".
    // The caller adds a " !" suffix when there's an active error, so the
    // user still sees "Claude 5h 6% · 7d 1% !" instead of "Claude: — !".
    let live: Vec<&UsageSnapshot> = if !live.is_empty() {
        live
    } else {
        let all: Vec<&UsageSnapshot> = snaps.iter().collect();
        match preferred_browser {
            Some(want) => {
                let want_lc = want.to_lowercase();
                let filtered: Vec<&UsageSnapshot> = all
                    .iter()
                    .copied()
                    .filter(|s| pretty_browser(&s.browser).to_lowercase() == want_lc)
                    .collect();
                if filtered.is_empty() { all } else { filtered }
            }
            None => all,
        }
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
