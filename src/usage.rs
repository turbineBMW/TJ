//! Each account's usage limits, shared by every thread on it: a subscription's 5-hour and weekly
//! windows (and extra-usage spend), or a team or enterprise seat's spending limit. Whichever
//! thread hears from `claude` last updates its account, and the status column shows the
//! account of the thread on show.

use std::collections::HashMap;

use gpui_kit::{App, Global};

use crate::agent::UsageWindow;

#[derive(Clone, Debug, Default)]
pub struct AccountUsage {
    /// The plan, such as `max`, `pro`, `team` or `enterprise`.
    pub plan: Option<String>,
    pub windows: Vec<UsageWindow>,
}

impl AccountUsage {
    /// Fold in what `claude` just reported. The stream's live updates carry only the
    /// utilization and reset time, so what `get_usage` said about a window is kept.
    pub fn merge(&mut self, windows: Vec<UsageWindow>) {
        for window in windows {
            match self.windows.iter_mut().find(|w| w.id == window.id) {
                Some(existing) => {
                    existing.utilization = window.utilization;
                    existing.resets_at = window.resets_at.or(existing.resets_at);
                    existing.severity = window.severity.or(existing.severity.take());
                    existing.scope = window.scope.or(existing.scope.take());
                    existing.spend = window.spend.or(existing.spend.take());
                }
                None => self.windows.push(window),
            }
        }
        self.windows.sort_by_key(|w| order(&w.id));
    }

    /// A subscription: its cost is Claude's estimate, not a charge.
    pub fn is_subscription(&self) -> bool {
        match self.plan.as_deref() {
            Some(plan) => matches!(plan, "pro" | "max"),
            None => self.windows.iter().any(|w| w.spend.is_none() && !w.id.starts_with("spend_")),
        }
    }
}

/// Usage by account id.
#[derive(Default)]
pub struct Usage(pub HashMap<String, AccountUsage>);

impl Global for Usage {}

impl Usage {
    pub fn get<'a>(cx: &'a App, account: &str) -> Option<&'a AccountUsage> {
        cx.try_global::<Usage>()?.0.get(account)
    }

    pub fn update(cx: &mut App, account: &str, f: impl FnOnce(&mut AccountUsage)) {
        f(cx.default_global::<Usage>().0.entry(account.to_string()).or_default());
    }
}

/// Shortest window first, then the rest as the server sent them.
fn order(id: &str) -> u8 {
    match id {
        "five_hour" => 0,
        "seven_day" => 1,
        "spend_daily" => 2,
        "spend_weekly" => 3,
        "spend_monthly" => 4,
        _ => 5,
    }
}

/// How the status column names a window, and its emoji.
pub fn label(window: &UsageWindow) -> (&'static str, String) {
    let scoped = |name: &str| match &window.scope {
        Some(scope) => format!("{name} · {scope}"),
        None => name.to_string(),
    };
    match window.id.as_str() {
        "five_hour" => ("⏱️", "5-hour".into()),
        "seven_day" => ("📅", "Weekly".into()),
        "spend_daily" => ("💸", "Daily spend".into()),
        "spend_weekly" => ("💸", "Weekly spend".into()),
        "spend_monthly" => ("💳", "Monthly spend".into()),
        id if id.starts_with("seven_day") || id.starts_with("weekly") => ("📅", scoped("Weekly")),
        id if id.starts_with("monthly") => ("🗓️", scoped("Monthly")),
        id if id.starts_with("spend_") => ("💸", format!("{} spend", capitalize(&id["spend_".len()..]))),
        id => ("📊", scoped(&capitalize(&id.replace('_', " ")))),
    }
}

/// Whether a window is worth a row. A model's own weekly window is shown only once the server
/// says it's getting close; the account's windows always are.
pub fn shown(window: &UsageWindow) -> bool {
    window.scope.is_none() || window.severity.as_deref().is_some_and(|s| s != "normal")
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// "USD" -> "$"; other currencies keep their code.
pub fn money(amount: f64, currency: &str) -> String {
    let symbol = match currency.to_ascii_uppercase().as_str() {
        "USD" | "CAD" | "AUD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        _ => return format!("{amount:.2} {currency}"),
    };
    if amount >= 100.0 { format!("{symbol}{amount:.0}") } else { format!("{symbol}{amount:.2}") }
}

/// When a window resets, from `now`: "in 2h 14m", "in 45m", or a day and time further out.
pub fn resets_in(resets_at: i64, now: i64) -> Option<String> {
    let left = resets_at - now;
    if left <= 0 {
        return None;
    }
    let (hours, minutes) = (left / 3600, (left % 3600) / 60);
    Some(match hours {
        0 => format!("in {}m", minutes.max(1)),
        1..24 => format!("in {hours}h {minutes}m"),
        _ => {
            use chrono::TimeZone as _;
            let at = chrono::Local.timestamp_opt(resets_at, 0).single()?;
            if hours < 24 * 6 { at.format("%a %-I %P").to_string() } else { at.format("%b %-d").to_string() }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Spend;

    fn window(id: &str, utilization: f64) -> UsageWindow {
        UsageWindow { id: id.into(), utilization, ..Default::default() }
    }

    #[test]
    fn live_updates_keep_what_get_usage_said() {
        let mut usage = AccountUsage::default();
        usage.merge(vec![
            UsageWindow { severity: Some("normal".into()), resets_at: Some(100), ..window("seven_day", 0.1) },
            window("five_hour", 0.2),
        ]);
        usage.merge(vec![window("seven_day", 0.3)]);
        let ids: Vec<_> = usage.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["five_hour", "seven_day"]);
        assert_eq!(usage.windows[1].utilization, 0.3);
        assert_eq!(usage.windows[1].resets_at, Some(100));
        assert_eq!(usage.windows[1].severity.as_deref(), Some("normal"));
    }

    #[test]
    fn a_spending_limit_is_not_a_subscription() {
        let spend = Spend { used: 271.4, limit: Some(500.0), currency: "USD".into() };
        let usage = AccountUsage {
            plan: Some("enterprise".into()),
            windows: vec![UsageWindow { spend: Some(spend), ..window("spend_monthly", 0.54) }],
        };
        assert!(!usage.is_subscription());
        assert!(AccountUsage { plan: Some("max".into()), windows: vec![] }.is_subscription());
    }

    #[test]
    fn labels_and_amounts() {
        assert_eq!(label(&window("five_hour", 0.0)).1, "5-hour");
        assert_eq!(label(&window("spend_monthly", 0.0)).1, "Monthly spend");
        assert_eq!(
            label(&UsageWindow { scope: Some("Fable".into()), ..window("weekly_scoped:Fable", 0.0) }).1,
            "Weekly · Fable"
        );
        assert_eq!(money(271.4, "USD"), "$271");
        assert_eq!(money(7.268, "usd"), "$7.27");
        assert_eq!(resets_in(10_000, 10_000), None);
        assert_eq!(resets_in(10_000 + 2 * 3600 + 14 * 60, 10_000).as_deref(), Some("in 2h 14m"));
        assert_eq!(resets_in(10_030, 10_000).as_deref(), Some("in 1m"));
    }
}
