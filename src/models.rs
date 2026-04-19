use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Window {
    pub utilization: f64,
    pub resets_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit: Option<i64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f64>,
    pub currency: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UsageResponse {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub seven_day_sonnet: Option<Window>,
    pub seven_day_opus: Option<Window>,
    pub seven_day_oauth_apps: Option<Window>,
    pub seven_day_omelette: Option<Window>,
    pub seven_day_cowork: Option<Window>,
    pub extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OverageResponse {
    pub is_enabled: bool,
    pub monthly_credit_limit: i64,
    pub currency: String,
    pub used_credits: f64,
    pub disabled_reason: Option<String>,
    pub disabled_until: Option<chrono::DateTime<chrono::Utc>>,
    pub out_of_credits: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PaymentMethod {
    pub brand: Option<String>,
    pub country: Option<String>,
    pub last4: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubscriptionResponse {
    pub status: String,
    pub next_charge_date: Option<String>,
    pub billing_interval: Option<String>,
    pub payment_method: Option<PaymentMethod>,
    pub currency: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageSnapshot {
    pub org_uuid: String,
    pub browser: String,
    pub account_email: Option<String>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
    pub usage: UsageResponse,
    pub overage: Option<OverageResponse>,
    pub subscription: SubscriptionResponse,
}
