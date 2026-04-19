use anyhow::{Context, Result};

use crate::browser::Browser;

#[cfg(target_os = "macos")]
pub fn safe_storage_password(browser: Browser) -> Result<Vec<u8>> {
    // Shell out to `/usr/bin/security` rather than calling the Keychain API directly.
    // Chrome's Safe Storage item ACL already trusts `/usr/bin/security`, so Chrome skips
    // the per-app approval dialog. Arc/Brave/Edge may still prompt once; after the user
    // clicks "Always Allow" the same path works silently thereafter.
    let out = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-wa",
            browser.keychain_account(),
            "-s",
            browser.keychain_service(),
        ])
        .output()
        .context("spawn /usr/bin/security")?;
    if !out.status.success() {
        anyhow::bail!(
            "`security find-generic-password` failed for {}: {}",
            browser.display_name(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut pw = out.stdout;
    while matches!(pw.last(), Some(b'\n') | Some(b'\r')) {
        pw.pop();
    }
    if pw.is_empty() {
        anyhow::bail!("{} Safe Storage password was empty", browser.display_name());
    }
    Ok(pw)
}

#[cfg(not(target_os = "macos"))]
pub fn safe_storage_password(_browser: Browser) -> Result<Vec<u8>> {
    anyhow::bail!("claude-meter supports macOS only")
}
