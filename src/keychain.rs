use anyhow::{Context, Result};

#[cfg(target_os = "macos")]
pub fn chrome_safe_storage_password() -> Result<Vec<u8>> {
    // Shell out to `/usr/bin/security` rather than calling the Keychain API directly.
    // Chrome Safe Storage's item ACL already trusts `/usr/bin/security`, so this skips
    // the per-app approval dialog that would otherwise pop up for an unsigned binary.
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-wa", "Chrome", "-s", "Chrome Safe Storage"])
        .output()
        .context("spawn /usr/bin/security")?;
    if !out.status.success() {
        anyhow::bail!(
            "`security find-generic-password` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut pw = out.stdout;
    // Strip trailing newline from CLI output.
    while matches!(pw.last(), Some(b'\n') | Some(b'\r')) {
        pw.pop();
    }
    if pw.is_empty() {
        anyhow::bail!("Chrome Safe Storage password was empty");
    }
    Ok(pw)
}

#[cfg(not(target_os = "macos"))]
pub fn chrome_safe_storage_password() -> Result<Vec<u8>> {
    anyhow::bail!("claude-meter v0.1 supports macOS only")
}
