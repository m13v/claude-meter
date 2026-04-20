use anyhow::{Context, Result};
use rquest::Client;
use rquest_util::Emulation;

use crate::cookies::ClaudeCookies;
use crate::models::{OverageResponse, SubscriptionResponse, UsageResponse, UsageSnapshot};

const BASE: &str = "https://claude.ai/api";

pub async fn fetch_usage_snapshot(cookies: &ClaudeCookies) -> Result<UsageSnapshot> {
    let client = build_client()?;
    let cookie_header = build_cookie_header(cookies)?;
    let org = &cookies.last_active_org;
    let mut errors: Vec<String> = Vec::new();

    let usage: Option<UsageResponse> = match get_json(
        &client,
        &cookie_header,
        &format!("{BASE}/organizations/{org}/usage"),
    )
    .await
    {
        Ok(v) => Some(v),
        Err(e) => {
            let msg = format!("usage: {e:#}");
            eprintln!("warn: {msg}");
            errors.push(msg);
            None
        }
    };
    let overage: Option<OverageResponse> = match get_json(
        &client,
        &cookie_header,
        &format!("{BASE}/organizations/{org}/overage_spend_limit"),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("overage: {e:#}");
            eprintln!("warn: {msg}");
            errors.push(msg);
            None
        }
    };
    let subscription: Option<SubscriptionResponse> = match get_json(
        &client,
        &cookie_header,
        &format!("{BASE}/organizations/{org}/subscription_details"),
    )
    .await
    {
        Ok(v) => Some(v),
        Err(e) => {
            let msg = format!("subscription: {e:#}");
            eprintln!("warn: {msg}");
            errors.push(msg);
            None
        }
    };

    let account_email = fetch_account_email(&client, &cookie_header).await;

    Ok(UsageSnapshot {
        org_uuid: org.clone(),
        browser: cookies.browser.display_name().to_string(),
        account_email,
        fetched_at: chrono::Utc::now(),
        usage,
        overage,
        subscription,
        errors,
        stale: false,
    })
}

async fn fetch_account_email(client: &Client, cookie_header: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Account {
        email_address: Option<String>,
    }
    get_json::<Account>(client, cookie_header, "https://claude.ai/api/account")
        .await
        .ok()
        .and_then(|a| a.email_address)
}

fn build_client() -> Result<Client> {
    // Don't pass `default_headers` here — it would override rquest-util's
    // emulation-installed Chrome header set (sec-ch-ua, user-agent, sec-fetch-*),
    // which Cloudflare checks. Pass Cookie/Referer/Accept per-request instead.
    let client = Client::builder()
        .emulation(Emulation::Chrome131)
        .build()?;
    Ok(client)
}

fn build_cookie_header(cookies: &ClaudeCookies) -> Result<String> {
    let header: String = cookies
        .all
        .iter()
        .filter(|(_, v)| v.bytes().all(is_header_safe))
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    // Parse once up front so we fail fast if any byte is bad.
    let _: rquest::header::HeaderValue = header
        .parse()
        .context("cookie header contained an invalid byte")?;
    Ok(header)
}

fn is_header_safe(b: u8) -> bool {
    // HTTP header field-value: visible ASCII plus space and tab, no CR/LF/NUL.
    b == b'\t' || (32..=126).contains(&b)
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    cookie_header: &str,
    url: &str,
) -> Result<T> {
    let resp = client
        .get(url)
        .header("Cookie", cookie_header)
        .header("Referer", "https://claude.ai/settings/usage")
        .header("Accept", "*/*")
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
