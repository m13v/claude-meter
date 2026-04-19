use anyhow::{anyhow, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Browser {
    Chrome,
    Arc,
    Brave,
    Edge,
}

impl Browser {
    pub const ALL: &'static [Browser] = &[
        Browser::Chrome,
        Browser::Arc,
        Browser::Brave,
        Browser::Edge,
    ];

    pub fn display_name(&self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome",
            Browser::Arc => "Arc",
            Browser::Brave => "Brave",
            Browser::Edge => "Edge",
        }
    }

    pub fn keychain_service(&self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome Safe Storage",
            Browser::Arc => "Arc Safe Storage",
            Browser::Brave => "Brave Safe Storage",
            Browser::Edge => "Microsoft Edge Safe Storage",
        }
    }

    pub fn keychain_account(&self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome",
            Browser::Arc => "Arc",
            Browser::Brave => "Brave",
            Browser::Edge => "Microsoft Edge",
        }
    }

    /// User-data root (holds `Default`, `Profile 1`, …). None if not installed.
    pub fn profile_root(&self) -> Result<Option<PathBuf>> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
        let app_support = home.join("Library/Application Support");
        let root = match self {
            Browser::Chrome => app_support.join("Google/Chrome"),
            Browser::Arc => app_support.join("Arc/User Data"),
            Browser::Brave => app_support.join("BraveSoftware/Brave-Browser"),
            Browser::Edge => app_support.join("Microsoft Edge"),
        };
        Ok(if root.exists() { Some(root) } else { None })
    }
}
