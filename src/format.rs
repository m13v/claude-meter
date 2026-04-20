use crate::models::{UsageSnapshot, Window};
use chrono::{DateTime, Local, Utc};

pub fn print_pretty(s: &UsageSnapshot) {
    println!();
    println!("claude-meter");
    println!("============");

    if let Some(u) = &s.usage {
        if let Some(w) = &u.five_hour {
            println!("{:<16} {}", "5-hour", format_window(w));
        }
        if let Some(w) = &u.seven_day {
            println!("{:<16} {}", "7-day all", format_window(w));
        }
        if let Some(w) = &u.seven_day_sonnet {
            println!("{:<16} {}", "7-day Sonnet", format_window(w));
        }
        if let Some(w) = &u.seven_day_opus {
            println!("{:<16} {}", "7-day Opus", format_window(w));
        }
    }

    if let Some(ov) = &s.overage {
        let u = ov.used_credits.unwrap_or(0.0) / 100.0;
        let status = if ov.out_of_credits { "  BLOCKED" } else { "" };
        let mut line = match ov.monthly_credit_limit {
            Some(l) => {
                let l = l as f64 / 100.0;
                let pct = if l > 0.0 { u / l * 100.0 } else { 0.0 };
                format!("${:.2} / ${:.2} ({:.0}%){}", u, l, pct, status)
            }
            None => format!("${:.2} used (no cap){}", u, status),
        };
        if let Some(until) = &ov.disabled_until {
            let local: DateTime<Local> = (*until).into();
            line.push_str(&format!(" until {}", local.format("%a %b %-d")));
        }
        println!("{:<16} {}", "Extra usage", line);
    }

    if let Some(sub) = &s.subscription {
        let pm = sub
            .payment_method
            .as_ref()
            .map(|p| {
                format!(
                    "{} \u{2022}\u{2022}{}",
                    p.brand.as_deref().unwrap_or("card"),
                    p.last4.as_deref().unwrap_or("????")
                )
            })
            .unwrap_or_else(|| "-".to_string());
        if let Some(d) = &sub.next_charge_date {
            println!("{:<16} {}   {}", "Next charge", d, pm);
        }
    }

    for err in &s.errors {
        println!("{:<16} {}", "error", err);
    }

    println!();
    let local: DateTime<Local> = s.fetched_at.into();
    let who = s.account_email.as_deref().unwrap_or("?");
    println!(
        "fetched {}   {} via {}   org {}",
        local.format("%Y-%m-%d %H:%M:%S %Z"),
        who,
        s.browser,
        s.org_uuid
    );
}

fn format_window(w: &Window) -> String {
    let reset = w
        .resets_at
        .map(|t| {
            let local: DateTime<Local> = t.into();
            let delta = t.signed_duration_since(Utc::now());
            let mut parts: Vec<String> = Vec::new();
            if delta.num_days() > 0 {
                parts.push(format!("{}d", delta.num_days()));
            }
            let hrs = delta.num_hours() - delta.num_days() * 24;
            if hrs > 0 {
                parts.push(format!("{}h", hrs));
            }
            let in_str = if parts.is_empty() {
                String::from("soon")
            } else {
                format!("in {}", parts.join(" "))
            };
            format!("-> resets {} ({})", local.format("%a %b %-d %H:%M"), in_str)
        })
        .unwrap_or_default();
    format!("{:>5.1}% used    {}", w.utilization, reset)
}
