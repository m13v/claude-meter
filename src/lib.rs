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
///
/// The function is also robust to inputs whose `browser` field is already a
/// comma-joined list (e.g. produced by an earlier dedupe pass that got
/// persisted to disk and re-loaded). Browser segments are deduped piece by
/// piece, so re-running dedupe over and over produces a stable canonical
/// "Claude Code, Arc, Chrome" string instead of accumulating duplicates.
pub fn dedupe_by_account(snaps: Vec<UsageSnapshot>) -> Vec<UsageSnapshot> {
    let mut out: Vec<UsageSnapshot> = Vec::with_capacity(snaps.len());
    for mut s in snaps {
        // Normalize the incoming row's own browser string up front so that
        // ("Arc, Arc, Claude Code, Arc") becomes ("Arc, Claude Code") even
        // before the merge.
        s.browser = canonicalize_browsers(&s.browser);
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
                e.browser = merge_browser_lists(&e.browser, &s.browser);
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

fn canonicalize_browsers(s: &str) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for seg in s.split(", ") {
        let seg = seg.trim();
        if !seg.is_empty() && !seen.contains(&seg) {
            seen.push(seg);
        }
    }
    seen.join(", ")
}

fn merge_browser_lists(existing: &str, new_list: &str) -> String {
    let mut seen: Vec<&str> = existing
        .split(", ")
        .filter(|s| !s.is_empty())
        .collect();
    for seg in new_list.split(", ") {
        let seg = seg.trim();
        if !seg.is_empty() && !seen.contains(&seg) {
            seen.push(seg);
        }
    }
    seen.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_dedupes_segments() {
        assert_eq!(canonicalize_browsers("Arc, Arc, Claude Code, Arc"), "Arc, Claude Code");
        assert_eq!(canonicalize_browsers("Claude Code"), "Claude Code");
        assert_eq!(canonicalize_browsers(""), "");
    }

    #[test]
    fn merge_keeps_order_no_dupes() {
        assert_eq!(
            merge_browser_lists("Claude Code, Arc", "Arc, Chrome"),
            "Claude Code, Arc, Chrome"
        );
        assert_eq!(merge_browser_lists("Arc", "Claude Code, Arc"), "Arc, Claude Code");
    }
}
