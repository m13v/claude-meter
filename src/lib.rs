pub mod api;
pub mod browser;
pub mod cookies;
pub mod format;
pub mod keychain;
pub mod models;

use models::UsageSnapshot;

/// Collapse snapshots that point at the same Claude account, merging their browsers into
/// one entry. Two sessions are "the same account" if they share an email, or, for
/// accounts the email fetch failed on, the same org uuid.
pub fn dedupe_by_account(snaps: Vec<UsageSnapshot>) -> Vec<UsageSnapshot> {
    let mut out: Vec<UsageSnapshot> = Vec::with_capacity(snaps.len());
    for s in snaps {
        let key: &str = s
            .account_email
            .as_deref()
            .unwrap_or(s.org_uuid.as_str());
        let existing = out.iter_mut().find(|e| {
            let ek = e.account_email.as_deref().unwrap_or(e.org_uuid.as_str());
            ek == key
        });
        match existing {
            Some(e) => {
                if !e.browser.split(", ").any(|b| b == s.browser) {
                    e.browser = format!("{}, {}", e.browser, s.browser);
                }
            }
            None => out.push(s),
        }
    }
    out
}
