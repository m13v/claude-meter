use anyhow::Result;
use claude_meter::{format, oauth};
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

    // Single source: the OAuth token Claude Code stores in the macOS Keychain
    // (service "Claude Code-credentials"). api.anthropic.com exposes the usage
    // data on /api/oauth/usage with no Cloudflare in front. One token = one
    // account (the one the active Claude Code CLI is logged into).
    let snapshot = oauth::fetch_oauth_snapshot().await.map_err(|e| {
        anyhow::anyhow!("oauth: {e:#}. Is Claude Code logged in? Run `claude` once to refresh.")
    })?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        format::print_pretty(&snapshot);
    }
    Ok(())
}
