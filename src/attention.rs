//! When a thread wants the user: it asked something, wants approval, has a plan, finished its
//! turn, or stopped on a problem. Such a thread is marked in the sidebar until it's looked at, and a desktop
//! notification says so. Clicking the notification opens the thread; its buttons bring the
//! notification back later (in 10 minutes, an hour, or tomorrow morning).

use gpui_kit::{SharedString, SystemNotification, SystemNotificationAction};
use serde::{Deserialize, Serialize};

/// Why a thread wants the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// It asked a question (AskUserQuestion).
    Question,
    /// It wants approval for a tool call.
    Approval,
    /// It presented a plan in plan mode.
    Plan,
    /// An MCP server asked for input through it (a form to fill in, or a link to open).
    Input,
    /// Its turn ended.
    Finished,
    /// Its turn failed (a usage limit, an expired login, an API error) or its process died, so
    /// nothing happens until the user sends another message.
    Stopped,
}

impl AttentionKind {
    /// Whether the agent is waiting on the user's answer (a question mark in the sidebar),
    /// rather than done (a check mark) or stopped (a warning).
    pub fn is_waiting(self) -> bool {
        matches!(self, AttentionKind::Question | AttentionKind::Approval | AttentionKind::Plan | AttentionKind::Input)
    }
}

/// What a thread wanted the user for, kept until the thread is looked at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attention {
    pub kind: AttentionKind,
    /// A line about it: the question, the tool call, or the start of the last reply.
    #[serde(default)]
    pub detail: String,
    /// Unix seconds.
    pub at: u64,
}

/// A notification to show again later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reminder {
    pub thread_id: String,
    /// Unix seconds.
    pub due: u64,
}

/// The notification buttons, by action id.
pub const REMIND_10_MINUTES: &str = "remind-10m";
pub const REMIND_HOUR: &str = "remind-1h";
pub const REMIND_TOMORROW: &str = "remind-tomorrow";
/// "Tomorrow" is this hour of the next day, local time.
const TOMORROW_HOUR: u32 = 9;

/// When a reminder picked on a notification is due, given the time now (unix seconds).
pub fn remind_at(action: &str, now: u64) -> Option<u64> {
    match action {
        REMIND_10_MINUTES => Some(now + 10 * 60),
        REMIND_HOUR => Some(now + 60 * 60),
        REMIND_TOMORROW => Some(tomorrow_morning(now)),
        _ => None,
    }
}

/// Nine in the morning of the day after `now`, local time.
fn tomorrow_morning(now: u64) -> u64 {
    use chrono::{Local, TimeZone as _};
    let next = Local
        .timestamp_opt(now as i64, 0)
        .single()
        .and_then(|t| t.date_naive().succ_opt())
        .and_then(|day| day.and_hms_opt(TOMORROW_HOUR, 0, 0))
        // A missing hour (a clock change) falls back to a day from now.
        .and_then(|t| Local.from_local_datetime(&t).earliest());
    next.map(|t| t.timestamp() as u64).unwrap_or(now + 24 * 60 * 60)
}

/// The notification for a thread wanting the user. `agent` names who's asking ("Claude").
pub fn notification(
    thread_id: &str,
    agent: &str,
    project: &str,
    thread: &str,
    attention: &Attention,
) -> SystemNotification {
    let title = match attention.kind {
        AttentionKind::Question => format!("{agent} has a question"),
        AttentionKind::Approval => format!("{agent} needs your approval"),
        AttentionKind::Plan => format!("{agent} has a plan ready"),
        AttentionKind::Input => format!("{agent} needs your input"),
        AttentionKind::Finished => format!("{agent} finished"),
        AttentionKind::Stopped => format!("{agent} stopped"),
    };
    let mut body = format!("{project} · {thread}");
    if !attention.detail.is_empty() {
        body.push('\n');
        body.push_str(&attention.detail);
    }
    let action =
        |id: &'static str, label: &'static str| SystemNotificationAction { id: id.into(), label: label.into() };
    SystemNotification {
        tag: thread_id.to_string().into(),
        title: title.into(),
        // The server may read the body as markup, so keep <, > and & as they are.
        body: SharedString::from(escape_markup(&body)),
        actions: vec![
            action(REMIND_10_MINUTES, "Remind in 10 minutes"),
            action(REMIND_HOUR, "Remind in an hour"),
            action(REMIND_TOMORROW, "Remind tomorrow"),
        ],
    }
}

/// Bring TJ's window forward on Hyprland. gpui asks the compositor to activate the window, but
/// without a recent click or key press in TJ that request can be refused, so ask Hyprland
/// directly too, the way Omarchy focuses an app (its Lua dispatcher, else the older one).
pub fn focus_on_hyprland() {
    if cfg!(test) || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    let pid = std::process::id();
    let hyprctl = |args: &[&str]| {
        std::process::Command::new("hyprctl")
            .args(args)
            .output()
            .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "ok")
    };
    std::thread::spawn(move || {
        let focus = format!("hl.dsp.focus({{ window = \"pid:{pid}\" }})");
        if !hyprctl(&["dispatch", &focus]) {
            hyprctl(&["dispatch", "focuswindow", &format!("pid:{pid}")]);
        }
    });
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Text cut to one line of at most `max` characters, for a notification.
pub fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminders_are_due_in_ten_minutes_an_hour_or_tomorrow_morning() {
        let now = 1_800_000_000;
        assert_eq!(remind_at(REMIND_10_MINUTES, now), Some(now + 600));
        assert_eq!(remind_at(REMIND_HOUR, now), Some(now + 3600));
        assert_eq!(remind_at("something-else", now), None);

        use chrono::{Local, TimeZone as _, Timelike as _};
        let due = remind_at(REMIND_TOMORROW, now).unwrap();
        let (today, then) = (Local.timestamp_opt(now as i64, 0).unwrap(), Local.timestamp_opt(due as i64, 0).unwrap());
        assert_eq!(then.date_naive(), today.date_naive().succ_opt().unwrap());
        assert_eq!((then.hour(), then.minute()), (TOMORROW_HOUR, 0));
    }

    #[test]
    fn the_notification_says_why_names_the_thread_and_offers_reminders() {
        let attention = Attention { kind: AttentionKind::Question, detail: "Use <b> & keep it?".into(), at: 0 };
        let n = notification("t1", "Claude", "TJ", "Notifications", &attention);
        assert_eq!(n.tag.as_ref(), "t1");
        assert_eq!(n.title.as_ref(), "Claude has a question");
        assert_eq!(n.body.as_ref(), "TJ · Notifications\nUse &lt;b&gt; &amp; keep it?");
        let ids: Vec<&str> = n.actions.iter().map(|a| a.id.as_ref()).collect();
        assert_eq!(ids, [REMIND_10_MINUTES, REMIND_HOUR, REMIND_TOMORROW]);

        let done = Attention { kind: AttentionKind::Finished, detail: String::new(), at: 0 };
        let n = notification("t1", "Claude", "TJ", "Notifications", &done);
        assert_eq!((n.title.as_ref(), n.body.as_ref()), ("Claude finished", "TJ · Notifications"));
        assert!(!AttentionKind::Finished.is_waiting() && AttentionKind::Plan.is_waiting());
        let stopped = Attention { kind: AttentionKind::Stopped, detail: "Usage limit reached".into(), at: 0 };
        let n = notification("t1", "Claude", "TJ", "Notifications", &stopped);
        assert_eq!(n.title.as_ref(), "Claude stopped");
        assert!(!AttentionKind::Stopped.is_waiting());
    }

    #[test]
    fn details_are_cut_to_one_line() {
        assert_eq!(one_line("Done.\n\nAll   tests pass.", 100), "Done. All tests pass.");
        assert_eq!(one_line("abcdefghij", 5), "abcd…");
    }
}
