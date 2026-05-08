pub mod api;
pub mod browser;
pub mod cookies;
pub mod format;
pub mod keychain;
pub mod models;
pub mod oauth;

use models::UsageSnapshot;

/// Collapse snapshots that point at the same Claude account, merging their browsers into
/// one entry. Two sessions are "the same account" if they share an email, or, for
/// accounts the email fetch failed on, the same org uuid.
///
/// When the same account appears from multiple sources (e.g. OAuth + browser cookies),
/// the first snapshot's usage numbers win, but missing fields on it are *back-filled*
/// from later duplicates. This matters because the OAuth source can't reach
/// `subscription_details` (no Stripe scope) or `overage_spend_limit` (returns 405),
/// while the cookie source CAN. Pushing OAuth first then cookie second therefore
/// gives you authoritative usage from OAuth + payment/overage detail from cookies.
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
                // Back-fill fields the existing (winner) snapshot is missing.
                if e.overage.is_none() && s.overage.is_some() {
                    e.overage = s.overage;
                }
                if e.subscription.is_none() && s.subscription.is_some() {
                    e.subscription = s.subscription;
                }
                if e.account_email.is_none() && s.account_email.is_some() {
                    e.account_email = s.account_email;
                }
            }
            None => out.push(s),
        }
    }
    out
}
