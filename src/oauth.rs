//! OAuth-token source for usage snapshots.
//!
//! Reads the OAuth credentials Claude Code stashes in the macOS Keychain under
//! service `Claude Code-credentials` and queries the OAuth-authenticated
//! endpoints on `api.anthropic.com`. This avoids the browser-cookie /
//! Cloudflare path entirely; it's the cleanest source for the *active* CLI
//! account. The cookie path is still used as a fallback (and to surface other
//! browser-logged-in accounts the OAuth token can't see).
//!
//! Endpoints used (token scope `user:profile` is sufficient):
//!   GET https://api.anthropic.com/api/oauth/usage     (rolling-window quotas + extra_usage)
//!   GET https://api.anthropic.com/api/oauth/profile   (account email + org uuid)
//!
//! Token refresh is intentionally NOT implemented here: the running Claude
//! Code CLI rotates the access token automatically (it has the refresh token
//! and the right OAuth client credentials). claude-meter just reads whatever
//! is in the keychain. If the token is expired we bail and let the cookie
//! path take over.

use anyhow::{Context, Result};
use chrono::Utc;
use rquest::Client;
use serde::Deserialize;

use crate::models::{UsageResponse, UsageSnapshot};

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const API_BASE: &str = "https://api.anthropic.com";

/// What the keychain blob looks like:
/// ```json
/// {"claudeAiOauth": {
///   "accessToken": "sk-ant-oat01-...",
///   "refreshToken": "sk-ant-ort01-...",
///   "expiresAt": 1778299177154,
///   "scopes": ["user:profile", "user:inference", ...],
///   "subscriptionType": "max",
///   "rateLimitTier": "default_claude_max_20x"
/// }}
/// ```
#[derive(Deserialize)]
struct KeychainBlob {
    #[serde(rename = "claudeAiOauth")]
    oauth: OAuthCreds,
}

#[derive(Debug, Deserialize)]
pub struct OAuthCreds {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    #[serde(rename = "subscriptionType", default)]
    pub subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier", default)]
    pub rate_limit_tier: Option<String>,
}

pub fn read_token() -> Result<OAuthCreds> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .context("spawn /usr/bin/security")?;
    if !out.status.success() {
        anyhow::bail!(
            "`security find-generic-password -s \"{KEYCHAIN_SERVICE}\"` failed: {}. Is Claude Code logged in?",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8(out.stdout)
        .context("Claude Code keychain blob was not valid UTF-8")?;
    let blob: KeychainBlob = serde_json::from_str(raw.trim()).with_context(|| {
        let snippet = &raw[..raw.len().min(80)];
        format!("parse Claude Code credentials JSON (head: {snippet:?})")
    })?;
    Ok(blob.oauth)
}

#[derive(Deserialize)]
struct OAuthProfile {
    account: ProfileAccount,
    organization: ProfileOrg,
}

#[derive(Deserialize)]
struct ProfileAccount {
    email: Option<String>,
}

#[derive(Deserialize)]
struct ProfileOrg {
    uuid: String,
}

pub async fn fetch_oauth_snapshot() -> Result<UsageSnapshot> {
    let creds = read_token().context("read OAuth token from Keychain")?;

    let now_ms = Utc::now().timestamp_millis();
    if creds.expires_at > 0 && creds.expires_at < now_ms {
        let age_min = (now_ms - creds.expires_at) / 60_000;
        anyhow::bail!(
            "Claude Code OAuth token expired {} minutes ago. Run `claude` once to refresh, or fall back to the cookie path.",
            age_min
        );
    }

    // Plain rquest client; api.anthropic.com is not behind Cloudflare's bot
    // gate, so no Chrome fingerprint emulation needed (unlike claude.ai).
    let client = Client::builder().build().context("build rquest client")?;
    let token = creds.access_token.as_str();

    // /api/oauth/usage carries the rolling-window utilization AND the
    // extra_usage block, so we don't need a separate overage_spend_limit call.
    let usage: UsageResponse = get_json(&client, token, &format!("{API_BASE}/api/oauth/usage"))
        .await
        .context("fetch /api/oauth/usage")?;

    // /api/oauth/profile gives us account email + org uuid for display and
    // dedupe_by_account.
    let profile: OAuthProfile =
        get_json(&client, token, &format!("{API_BASE}/api/oauth/profile"))
            .await
            .context("fetch /api/oauth/profile")?;

    Ok(UsageSnapshot {
        org_uuid: profile.organization.uuid,
        browser: "Claude Code".to_string(),
        account_email: profile.account.email,
        fetched_at: Utc::now(),
        usage: Some(usage),
        // Stripe-flavoured subscription_details and the dedicated
        // overage_spend_limit endpoint are not exposed via OAuth scopes; they
        // require the claude.ai web session cookies. The cookie path fills
        // those in if/when it runs.
        overage: None,
        subscription: None,
        errors: Vec::new(),
        stale: false,
    })
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    token: &str,
    url: &str,
) -> Result<T> {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet = &body[..body.len().min(300)];
        anyhow::bail!("HTTP {status} from {url}: {snippet}");
    }
    serde_json::from_str::<T>(&body).with_context(|| {
        let snippet = &body[..body.len().min(300)];
        format!("parse JSON from {url}: {snippet}")
    })
}
