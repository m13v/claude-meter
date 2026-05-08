use anyhow::Result;
use claude_meter::{api, cookies, dedupe_by_account, format, oauth};
use clap::Parser;

#[derive(Parser)]
#[command(name = "claude-meter", version, about = "Live Claude plan usage and extra-usage balance")]
struct Cli {
    /// Emit machine-readable JSON
    #[arg(long)]
    json: bool,

    /// Skip the OAuth (Claude Code keychain) source and use only browser cookies.
    /// Useful for debugging or if the keychain entry is broken.
    #[arg(long)]
    no_oauth: bool,

    /// Skip the cookie source and use only the OAuth (Claude Code keychain) lane.
    /// You'll only see the account Claude Code itself is logged into.
    #[arg(long)]
    no_cookies: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut snapshots: Vec<claude_meter::models::UsageSnapshot> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ---- Source 1: OAuth keychain entry (primary). ----
    // This is the cleanest path: Claude Code stores a Bearer token at
    // service "Claude Code-credentials"; api.anthropic.com exposes the same
    // usage data on /api/oauth/usage with no Cloudflare in front. One token
    // = one account (the one the active Claude Code CLI is logged into).
    if !cli.no_oauth {
        match oauth::fetch_oauth_snapshot().await {
            Ok(s) => snapshots.push(s),
            Err(e) => {
                let msg = format!("oauth: {e:#}");
                eprintln!("warn: {msg}");
                errors.push(msg);
            }
        }
    }

    // ---- Source 2: Browser cookies (fallback / multi-account fill-in). ----
    // Catches accounts logged into other Chromium profiles that aren't the
    // current Claude Code CLI account. Also fills in subscription_details
    // (next_charge_date, payment_method) which OAuth scopes can't reach.
    if !cli.no_cookies {
        match cookies::find_all_claude_sessions() {
            Ok(sessions) => {
                for c in &sessions {
                    match api::fetch_usage_snapshot(c).await {
                        Ok(s) => snapshots.push(s),
                        Err(e) => eprintln!(
                            "warn: {} cookie fetch failed: {e:#}",
                            c.browser.display_name()
                        ),
                    }
                }
            }
            Err(e) => {
                let msg = format!("cookies: {e:#}");
                eprintln!("warn: {msg}");
                errors.push(msg);
            }
        }
    }

    if snapshots.is_empty() {
        anyhow::bail!(
            "no usage source returned data. Tried: {}",
            if errors.is_empty() {
                "no sources enabled".to_string()
            } else {
                errors.join("; ")
            }
        );
    }
    // dedupe_by_account merges sources for the same account, keeping the
    // first-added snapshot's numbers. Because OAuth is pushed first above,
    // OAuth-sourced numbers win on the account it covers.
    let snapshots = dedupe_by_account(snapshots);

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&snapshots)?);
    } else {
        for (i, s) in snapshots.iter().enumerate() {
            if i > 0 {
                println!();
            }
            format::print_pretty(s);
        }
    }
    Ok(())
}
