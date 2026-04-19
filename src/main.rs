use anyhow::{Context, Result};
use clap::Parser;

mod api;
mod cookies;
mod format;
mod keychain;
mod models;

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

    let safe_storage_pw = keychain::chrome_safe_storage_password()
        .context("could not read Chrome Safe Storage password from Keychain. Is Chrome installed?")?;

    let cookies = cookies::find_and_decrypt_claude_cookies(&safe_storage_pw)
        .context("could not read claude.ai cookies from Chrome")?;

    let snapshot = api::fetch_usage_snapshot(&cookies)
        .await
        .context("could not fetch usage from claude.ai")?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        format::print_pretty(&snapshot);
    }
    Ok(())
}
