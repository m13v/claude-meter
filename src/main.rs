use anyhow::{Context, Result};
use claude_meter::{api, cookies, dedupe_by_account, format};
use clap::Parser;

#[derive(Parser)]
#[command(name = "claude-meter", version, about = "Live Claude plan usage and extra-usage balance")]
struct Cli {
    /// Emit machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let sessions = cookies::find_all_claude_sessions()
        .context("could not find a browser profile logged into claude.ai")?;

    let mut snapshots = Vec::with_capacity(sessions.len());
    for cookies in &sessions {
        match api::fetch_usage_snapshot(cookies).await {
            Ok(s) => snapshots.push(s),
            Err(e) => eprintln!(
                "warn: {} fetch failed: {e:#}",
                cookies.browser.display_name()
            ),
        }
    }
    if snapshots.is_empty() {
        anyhow::bail!("every browser session failed to fetch usage");
    }
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
