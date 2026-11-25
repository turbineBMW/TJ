//! A project's one agent tab: the conversation of the thread picked in the sidebar. Picking
//! another thread swaps what's in it; the panes and terminals around it stay as they are.
//!
//! It also builds the thread's status column (the overview, the project's changed files and the
//! agent's numbers), which the workspace shows at the right of the window, in the frame like the
//! sidebar, whichever pane is focused. The changes and the numbers are cards in the sidebar's
//! selected-row colour.

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, button::Button, button::ButtonVariants as _};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::changes::{ChangeKind, ProjectChanges};
use crate::thread::ThreadView;

/// How wide the status column is.
pub const STATUS_WIDTH: f32 = 340.0;

pub enum AgentSlotEvent {
    /// Open the review, scrolled to this file if given.
    OpenReview(Option<String>),
}

impl EventEmitter<AgentSlotEvent> for AgentSlot {}

pub struct AgentSlot {
    thread: Option<(String, Entity<ThreadView>)>,
    project_dir: PathBuf,
    changes: Entity<ProjectChanges>,
    _thread_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl AgentSlot {
    pub fn new(project_dir: PathBuf, changes: Entity<ProjectChanges>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&changes, |_, _, cx| cx.notify());
        let usage = cx.observe_global::<crate::usage::Usage>(|_, cx| cx.notify());
        Self { thread: None, project_dir, changes, _thread_subscription: None, _subscriptions: vec![observe, usage] }
    }

    pub fn thread_id(&self) -> Option<&str> {
        self.thread.as_ref().map(|(id, _)| id.as_str())
    }

    pub fn thread(&self) -> Option<&Entity<ThreadView>> {
        self.thread.as_ref().map(|(_, view)| view)
    }

    /// Show this thread instead of the one shown now.
    pub fn set_thread(&mut self, id: &str, view: Entity<ThreadView>, cx: &mut Context<Self>) {
        if self.thread_id() == Some(id) {
            return;
        }
        self._thread_subscription = Some(cx.observe(&view, |_, _, cx| cx.notify()));
        self.thread = Some((id.to_string(), view));
        cx.notify();
    }

    /// Show nothing (the thread shown was archived or deleted).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.thread = None;
        self._thread_subscription = None;
        cx.notify();
    }

    /// Whether there's a status column: whenever a thread is shown, even a new one, whose
    /// overview says what it doesn't have yet.
    pub fn has_status(&self) -> bool {
        self.thread.is_some()
    }

    /// The status column for the thread shown: the overview on top, straight on the frame like
    /// the sidebar's projects, then the changed files and the agent's numbers at the bottom as
    /// cards of `card` colour.
    pub fn status_column(&self, card: Hsla, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.has_status() {
            return None;
        }
        let view = self.thread()?.clone();
        let section = |el: Stateful<Div>| el.flex_none().rounded_lg().overflow_hidden().bg(card);
        Some(
            v_flex()
                .id("status")
                .test_support()
                .w(px(STATUS_WIDTH))
                .flex_none()
                .h_full()
                .gap_2()
                .px_2()
                .pb_2()
                // The overview takes the space, and scrolls when it runs out.
                .child(self.overview(&view, cx).flex_1().min_h_0())
                .children(self.changes_list(&view, card, cx).map(section))
                .child(section(self.metrics(&view, cx)).test_support())
                .into_any_element(),
        )
    }

    /// Goal, Now, Next and tasks for the thread, like pi's session sidebar, in the sidebar's
    /// colours. Every part always shows, saying so when there's nothing yet.
    fn overview(&self, view: &Entity<ThreadView>, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = view.read(cx);
        let ov = &t.overview;
        let theme = cx.theme();
        let (muted, accent, fg) = (theme.sidebar_foreground.opacity(0.6), theme.primary, theme.sidebar_foreground);
        let label =
            |text: &'static str| div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text);
        let empty = |text: &'static str| div().text_sm().italic().text_color(muted).child(text);
        let field = |name: &'static str, value: &Option<String>, color: Hsla, none: &'static str| {
            v_flex().gap_0p5().child(label(name)).child(match value.clone() {
                Some(value) => div().text_sm().text_color(color).child(value),
                None => empty(none),
            })
        };
        let tasks = ov.tasks.iter().map(|task| {
            h_flex()
                .gap_2()
                .items_start()
                .text_sm()
                .child(div().text_color(if task.done { muted } else { accent }).child(if task.done {
                    "✓"
                } else {
                    "○"
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(task.done, |el| el.text_color(muted).line_through())
                        .child(task.text.clone()),
                )
        });
        let body = v_flex()
            .gap_2()
            .child(field("GOAL", &ov.goal, fg, "No goal established yet"))
            .child(field("NOW", &ov.now, accent, "Nothing in progress"))
            .child(field("NEXT", &ov.next, fg, "No next steps established yet"))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(h_flex().gap_1().child(label("TASKS")).when(!ov.tasks.is_empty(), |el| {
                        el.child(div().text_xs().text_color(muted).child(format!(
                            "{}/{}",
                            ov.done_count(),
                            ov.tasks.len()
                        )))
                    }))
                    .children(tasks)
                    .when(ov.tasks.is_empty(), |el| el.child(empty("No tasks yet"))),
            );
        let view = view.clone();
        v_flex()
            .id("overview")
            .overflow_y_scroll()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div().flex_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child("OVERVIEW"),
                    )
                    .when(t.overview_busy, |el| el.child(div().text_xs().text_color(muted).child("updating…")))
                    .child(
                        Button::new("refresh-overview")
                            .xsmall()
                            .ghost()
                            .label("↻")
                            .tooltip("Update the overview now")
                            .on_click(move |_, _, cx| view.update(cx, |t, cx| t.update_overview(cx))),
                    ),
            )
            .child(body)
    }

    /// The agent's numbers, pinned to the bottom of the column: the account's usage limits as
    /// meters (a subscription's 5-hour and weekly windows, or a spending limit), the context
    /// window, and what the thread has cost. The model is left to the prompt card.
    fn metrics(&self, view: &Entity<ThreadView>, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = view.read(cx);
        let account = crate::usage::Usage::get(cx, &t.account_id).cloned().unwrap_or_default();
        // Before the first turn, the room the model had last time.
        let window =
            t.context_window.or_else(|| t.model_name().and_then(|m| crate::usage::Usage::context_window(cx, &m)));
        let (account_name, cost, context) = (t.account_name.clone(), t.cost_usd, (t.context_tokens, window));
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let (warning, danger, primary) = (theme.warning, theme.danger, theme.primary);
        let mono = theme.mono_font_family.clone();
        let (ansi, _, _) = crate::terminal::palette(cx);
        let now = chrono::Utc::now().timestamp();

        // A meter's colour: its own hue while there's room, then the server's (or our) warning.
        let tone = |hue: Hsla, fraction: f64, severity: Option<&str>| match severity {
            Some("critical") => danger,
            _ if fraction >= 0.9 => danger,
            Some("warning") => warning,
            _ if fraction >= 0.75 => warning,
            _ => hue,
        };
        let bar = |fraction: f64, color: Hsla| {
            let fraction = fraction.clamp(0.0, 1.0) as f32;
            let hot = color == danger || color == warning;
            div().h(px(6.)).w_full().rounded_full().bg(muted.opacity(0.18)).child(
                div()
                    .h_full()
                    .rounded_full()
                    .w(relative(fraction))
                    .when(fraction > 0.0, |el| el.min_w(px(6.)))
                    .bg(linear_gradient(90., linear_color_stop(color.opacity(0.55), 0.), linear_color_stop(color, 1.)))
                    .when(hot, |el| {
                        el.shadow(vec![BoxShadow {
                            color: color.opacity(0.6),
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(6.),
                            spread_radius: px(0.),
                            inset: false,
                        }])
                    }),
            )
        };
        let meter = |emoji: &'static str,
                     name: String,
                     value: String,
                     note: Option<String>,
                     fraction: Option<f64>,
                     color: Hsla| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_xs()
                        .child(div().w(px(18.)).child(emoji))
                        .child(div().text_color(fg).font_weight(FontWeight::MEDIUM).child(name))
                        .child(div().flex_1())
                        .children(note.map(|note| div().text_color(muted).child(note)))
                        .child(
                            div()
                                .font_family(mono.clone())
                                .font_weight(FontWeight::BOLD)
                                .text_color(if fraction.is_some() { color } else { fg })
                                .child(value),
                        ),
                )
                .children(fraction.map(|f| bar(f, color)))
        };

        let mut rows = Vec::new();
        for (ix, window) in account.windows.iter().filter(|w| crate::usage::shown(w)).enumerate() {
            let (emoji, name) = crate::usage::label(window);
            // A window that has reset since it was reported has nothing used yet.
            let reset = window.resets_at.is_some_and(|at| at <= now);
            let fraction = if reset { 0.0 } else { window.utilization };
            let hue = match window.id.as_str() {
                "five_hour" => ansi[14],
                "seven_day" => ansi[13],
                id if id.starts_with("spend_") => ansi[10],
                _ => [ansi[12], ansi[11], ansi[6]][ix % 3],
            };
            let color = tone(hue, fraction, if reset { None } else { window.severity.as_deref() });
            let resets = window.resets_at.and_then(|at| crate::usage::resets_in(at, now));
            let (value, note) = match &window.spend {
                Some(spend) => {
                    let used = crate::usage::money(spend.used, &spend.currency);
                    match spend.limit {
                        Some(limit) => (
                            format!("{:.0}%", fraction * 100.0),
                            Some(format!("{used} / {}", crate::usage::money(limit, &spend.currency))),
                        ),
                        None => (used, resets),
                    }
                }
                None => (format!("{:.0}%", fraction * 100.0), resets),
            };
            let capped = window.spend.as_ref().is_none_or(|s| s.limit.is_some());
            rows.push(meter(emoji, name, value, note, capped.then_some(fraction), color).into_any_element());
        }
        // Always shown: empty until the first turn, with the model's window once it's known.
        let (value, note, fraction) = match context {
            (tokens, Some(window)) => {
                let tokens = tokens.unwrap_or(0);
                let fraction = tokens as f64 / window.max(1) as f64;
                (format!("{:.0}%", fraction * 100.0), format!("{} / {}", compact(tokens), compact(window)), fraction)
            }
            (_, None) => ("—".to_string(), "after the first message".to_string(), 0.0),
        };
        rows.push(
            meter("🧠", "Context".into(), value, Some(note), Some(fraction), tone(ansi[12], fraction, None))
                .into_any_element(),
        );
        if cost > 0.0 {
            // On a subscription this is Claude's estimate, not a charge.
            let approx = if account.is_subscription() { "≈" } else { "" };
            let amount = if cost < 1.0 { format!("{approx}${cost:.3}") } else { format!("{approx}${cost:.2}") };
            let note = account.is_subscription().then(|| "estimate".to_string());
            rows.push(meter("💵", "This thread".into(), amount, note, None, fg).into_any_element());
        }
        let plan = account.plan.as_deref().map(|plan| match plan {
            "max" => "Max".to_string(),
            "pro" => "Pro".to_string(),
            "team" => "Team".to_string(),
            "enterprise" => "Enterprise".to_string(),
            other => other.to_string(),
        });
        let badge = h_flex()
            .gap_1()
            .px_1p5()
            .rounded_full()
            .bg(primary.opacity(0.14))
            .border_1()
            .border_color(primary.opacity(0.35))
            .text_xs()
            .text_color(primary)
            .child("👤")
            .child(account_name)
            .children(plan.map(|plan| div().font_weight(FontWeight::SEMIBOLD).child(format!("· {plan}"))));
        let quiet = account.windows.is_empty().then(|| {
            div()
                .text_xs()
                .text_color(muted)
                .child("No usage limits reported yet; they show once Claude starts on this account.")
        });
        v_flex()
            .id("metrics")
            .flex_none()
            .gap_2()
            .px_3()
            .pt_2()
            .pb_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child("USAGE"))
                    .child(badge),
            )
            .children(quiet)
            .children(rows)
    }

    /// The project's uncommitted changes; a dot marks files this thread edited. A file opens
    /// the review of just that file; **View all** (with more than one) opens all of them.
    fn changes_list(&self, view: &Entity<ThreadView>, card: Hsla, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let touched = crate::changes::touched_relative(&self.project_dir, &view.read(cx).touched);
        let model = self.changes.read(cx);
        if !model.changes.is_repo || model.changes.files.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (muted, accent) = (theme.muted_foreground, theme.primary);
        let (added, deleted, modified) = (theme.success, theme.danger, theme.warning);
        let mono = theme.mono_font_family.clone();
        let rows = model.changes.files.iter().enumerate().map(|(ix, file)| {
            let path = file.path.clone();
            let (dir, name) = match path.rsplit_once('/') {
                Some((dir, name)) => (format!("{dir}/"), name.to_string()),
                None => (String::new(), path.clone()),
            };
            let color = match file.kind {
                ChangeKind::Added => added,
                ChangeKind::Deleted => deleted,
                ChangeKind::Modified => modified,
                ChangeKind::Renamed => accent,
            };
            h_flex()
                .id(SharedString::from(format!("change-{ix}")))
                .test_support()
                .gap_2()
                .px_3()
                .py_0p5()
                .cursor_pointer()
                // One more layer of the card's colour.
                .hover(|el| el.bg(card))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(AgentSlotEvent::OpenReview(Some(path.clone())))))
                .child(div().w_3().text_xs().font_family(mono.clone()).text_color(color).child(file.kind.letter()))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_sm()
                        .child(div().text_color(muted).child(dir))
                        .child(div().child(name)),
                )
                .when(touched.contains(&file.path), |el| el.child(div().size_1p5().rounded_full().bg(accent)))
                .child(
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .font_family(mono.clone())
                        .child(div().text_color(added).child(format!("+{}", file.additions)))
                        .child(div().text_color(deleted).child(format!("−{}", file.deletions))),
                )
        });
        Some(
            v_flex()
                .id("changes")
                .max_h(relative(0.4))
                .child(
                    h_flex().px_3().py_1().gap_2().child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("Changes · {} files ·", model.changes.files.len()))
                            .child(div().text_color(added).child(format!("+{}", model.changes.additions())))
                            .child(div().text_color(deleted).child(format!("−{}", model.changes.deletions()))),
                    ),
                )
                .child(div().id("changes-list").pb_1().overflow_y_scroll().children(rows))
                .when(model.changes.files.len() > 1, |el| {
                    el.child(
                        h_flex().px_3().pb_1().child(
                            Button::new("review-changes")
                                .xsmall()
                                .ghost()
                                .label("View all")
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(AgentSlotEvent::OpenReview(None)))),
                        ),
                    )
                }),
        )
    }
}

/// 12_345 -> "12k", 1_000_000 -> "1M".
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{}M", (n as f64 / 1_000_000.0 * 10.0).round() / 10.0),
    }
}

impl Render for AgentSlot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.thread().cloned() else {
            let muted = cx.theme().muted_foreground;
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child("Pick a thread in the sidebar.");
        };
        div().size_full().child(view)
    }
}

#[cfg(test)]
mod tests {
    use super::compact;

    #[test]
    fn compact_numbers() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(12_345), "12k");
        assert_eq!(compact(1_000_000), "1M");
        assert_eq!(compact(1_250_000), "1.3M");
    }
}
