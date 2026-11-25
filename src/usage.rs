//! Each account's usage limits, shared by every thread on it: a subscription's 5-hour and weekly
//! windows (and extra-usage spend), or a team or enterprise seat's spending limit. Whichever
//! thread hears from `claude` last updates its account, and the status column shows the
//! account of the thread on show.
//!
//! What was last heard is saved (`usage.json`, beside the state file) with the context window
//! each model reported, so a restarted TJ shows numbers before any thread has run. A window that
//! has reset since is shown empty.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};

use crate::agent::UsageWindow;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    #[serde(default)]
    /// The plan, such as `max`, `pro`, `team` or `enterprise`.
    pub plan: Option<String>,
    #[serde(default)]
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

/// What's kept in `usage.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    /// Usage by account id.
    #[serde(default)]
    pub accounts: HashMap<String, AccountUsage>,
    /// The context window each model last reported, by its short name ("Opus 5.5"), so a
    /// thread that hasn't started yet can show the room it will have.
    #[serde(default)]
    pub context_windows: HashMap<String, u64>,
}

/// Every account's usage and every model's context window, saved as they change.
#[derive(Default)]
pub struct Usage {
    saved: Saved,
    /// Where it's saved; nothing is saved when `None`.
    file: Option<PathBuf>,
}

impl Global for Usage {}

impl Usage {
    /// Read what was saved in `file` and keep saving there. A missing or unreadable file
    /// starts empty.
    pub fn load(cx: &mut App, file: PathBuf) {
        let saved = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| {
                serde_json::from_str(&text).inspect_err(|err| eprintln!("tj: ignoring {}: {err}", file.display())).ok()
            })
            .unwrap_or_default();
        cx.set_global(Usage { saved, file: Some(file) });
    }

    pub fn get<'a>(cx: &'a App, account: &str) -> Option<&'a AccountUsage> {
        cx.try_global::<Usage>()?.saved.accounts.get(account)
    }

    pub fn update(cx: &mut App, account: &str, f: impl FnOnce(&mut AccountUsage)) {
        let usage = cx.default_global::<Usage>();
        let entry = usage.saved.accounts.entry(account.to_string()).or_default();
        let before = entry.clone();
        f(entry);
        if *entry != before {
            usage.save();
        }
    }

    pub fn context_window(cx: &App, model: &str) -> Option<u64> {
        cx.try_global::<Usage>()?.saved.context_windows.get(model).copied()
    }

    pub fn set_context_window(cx: &mut App, model: String, window: u64) {
        let usage = cx.default_global::<Usage>();
        if usage.saved.context_windows.insert(model, window) != Some(window) {
            usage.save();
        }
    }

    /// Write atomically, like the state file; a failure only costs the numbers after a restart.
    fn save(&self) {
        let Some(file) = &self.file else { return };
        let result = (|| -> anyhow::Result<()> {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = file.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&self.saved)?)?;
            std::fs::rename(&tmp, file)?;
            Ok(())
        })();
        if let Err(err) = result {
            eprintln!("tj: couldn't save {}: {err:#}", file.display());
        }
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

    #[gpui_kit::test]
    fn usage_and_context_windows_survive_a_restart(cx: &mut gpui_kit::TestAppContext) {
        let file = std::env::temp_dir().join(format!("tj-usage-{}", crate::store::new_id())).join("usage.json");
        cx.update(|cx| {
            Usage::load(cx, file.clone());
            assert_eq!(Usage::get(cx, "personal"), None, "nothing saved yet");
            Usage::update(cx, "personal", |u| {
                u.plan = Some("max".into());
                u.merge(vec![window("five_hour", 0.42)]);
            });
            Usage::set_context_window(cx, "Opus 5.5".into(), 1_000_000);
        });
        // A new app: what the last one heard comes back from the file.
        let restarted = gpui_kit::TestAppContext::single();
        restarted.update(|cx| {
            Usage::load(cx, file.clone());
            let personal = Usage::get(cx, "personal").unwrap();
            assert_eq!(personal.plan.as_deref(), Some("max"));
            assert_eq!(personal.windows[0].utilization, 0.42);
            assert_eq!(Usage::context_window(cx, "Opus 5.5"), Some(1_000_000));
        });
        std::fs::write(&file, "not json").unwrap();
        restarted.update(|cx| {
            Usage::load(cx, file.clone());
            assert_eq!(Usage::get(cx, "personal"), None, "an unreadable file starts empty");
        });
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
