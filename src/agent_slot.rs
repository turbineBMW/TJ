//! A project's one agent tab: the conversation of the thread picked in the sidebar, with its
//! status column on the right (the overview, the project's changed files and the agent's
//! numbers). Picking another thread swaps what's in it; the panes and terminals around it stay
//! as they are. The column folds away when the tab is narrow, and can be shown or hidden by hand.

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, button::Button, button::ButtonVariants as _};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::changes::{ChangeKind, ProjectChanges};
use crate::thread::ThreadView;

/// How wide the status column is.
pub const STATUS_WIDTH: f32 = 340.0;
/// Below this width the status column folds away, unless it was shown by hand.
const FOLD_WIDTH: f32 = 760.0;

pub enum AgentSlotEvent {
    /// Open the review, scrolled to this file if given.
    OpenReview(Option<String>),
}

impl EventEmitter<AgentSlotEvent> for AgentSlot {}

pub struct AgentSlot {
    thread: Option<(String, Entity<ThreadView>)>,
    project_dir: PathBuf,
    changes: Entity<ProjectChanges>,
    /// Shown or hidden by hand; `None` follows the width.
    status_choice: Option<bool>,
    /// The tab's width at the last paint.
    width: Pixels,
    _thread_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl AgentSlot {
    pub fn new(project_dir: PathBuf, changes: Entity<ProjectChanges>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&changes, |_, _, cx| cx.notify());
        let usage = cx.observe_global::<crate::usage::Usage>(|_, cx| cx.notify());
        Self {
            thread: None,
            project_dir,
            changes,
            status_choice: None,
            // Until it's measured, assume there's room.
            width: px(f32::MAX),
            _thread_subscription: None,
            _subscriptions: vec![observe, usage],
        }
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

    /// Whether the status column has anything to show: not while the thread is on its intro.
    pub fn has_status(&self, cx: &App) -> bool {
        let Some(t) = self.thread().map(|t| t.read(cx)) else { return false };
        if t.is_fresh() {
            return false;
        }
        let changes = &self.changes.read(cx).changes;
        !t.overview.is_empty() || (changes.is_repo && !changes.files.is_empty())
    }

    pub fn status_shown(&self, cx: &App) -> bool {
        self.has_status(cx) && self.status_choice.unwrap_or(self.width >= px(FOLD_WIDTH))
    }

    pub fn toggle_status(&mut self, cx: &mut Context<Self>) {
        self.status_choice = Some(!self.status_shown(cx));
        cx.notify();
    }

    /// Goal, Now, Next and tasks for the thread, like pi's session sidebar.
    fn overview(&self, view: &Entity<ThreadView>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = view.read(cx);
        let ov = &t.overview;
        if ov.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (muted, accent, fg) = (theme.muted_foreground, theme.primary, theme.foreground);
        let label =
            |text: &'static str| div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text);
        let field = |name: &'static str, value: &Option<String>, color: Hsla| {
            value.clone().map(|value| {
                v_flex().gap_0p5().child(label(name)).child(div().text_sm().text_color(color).child(value))
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
            .children(field("GOAL", &ov.goal, fg))
            .children(field("NOW", &ov.now, accent))
            .children(field("NEXT", &ov.next, fg))
            .when(!ov.tasks.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_0p5()
                        .child(h_flex().gap_1().child(label("TASKS")).child(
                            div().text_xs().text_color(muted).child(format!("{}/{}", ov.done_count(), ov.tasks.len())),
                        ))
                        .children(tasks),
                )
            });
        let view = view.clone();
        Some(
            v_flex()
                .id("overview")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap_2()
                .px_3()
                .py_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(muted)
                                .child("OVERVIEW"),
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
                .into_any_element(),
        )
    }

    /// The agent's numbers, pinned to the bottom of the column: the account's usage limits as
    /// meters (a subscription's 5-hour and weekly windows, or a spending limit), the context
    /// window, and what the thread has cost. The model is left to the prompt card.
    fn metrics(&self, view: &Entity<ThreadView>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = view.read(cx);
        let account = crate::usage::Usage::get(cx, &t.account_id).cloned().unwrap_or_default();
        let (account_name, cost, context) = (t.account_name.clone(), t.cost_usd, (t.context_tokens, t.context_window));
        let theme = cx.theme();
        let (muted, fg, border) = (theme.muted_foreground, theme.foreground, theme.border);
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
        if let (Some(tokens), Some(window)) = context {
            let fraction = tokens as f64 / window.max(1) as f64;
            rows.push(
                meter(
                    "🧠",
                    "Context".into(),
                    format!("{:.0}%", fraction * 100.0),
                    Some(format!("{} / {}", compact(tokens), compact(window))),
                    Some(fraction),
                    tone(ansi[12], fraction, None),
                )
                .into_any_element(),
            );
        }
        if cost > 0.0 {
            // On a subscription this is Claude's estimate, not a charge.
            let approx = if account.is_subscription() { "≈" } else { "" };
            let amount = if cost < 1.0 { format!("{approx}${cost:.3}") } else { format!("{approx}${cost:.2}") };
            let note = account.is_subscription().then(|| "estimate".to_string());
            rows.push(meter("💵", "This thread".into(), amount, note, None, fg).into_any_element());
        }
        if rows.is_empty() && account.plan.is_none() {
            return None;
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
        let quiet = account
            .windows
            .is_empty()
            .then(|| div().text_xs().text_color(muted).child("No usage limits reported for this account yet."));
        Some(
            v_flex()
                .id("metrics")
                .test_support()
                .flex_none()
                .gap_2()
                .px_3()
                .pt_2()
                .pb_3()
                .border_t_1()
                .border_color(border)
                .bg(linear_gradient(
                    180.,
                    linear_color_stop(primary.opacity(0.0), 0.),
                    linear_color_stop(primary.opacity(0.06), 1.),
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div().flex_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child("USAGE"),
                        )
                        .child(badge),
                )
                .children(quiet)
                .children(rows)
                .into_any_element(),
        )
    }

    /// The project's uncommitted changes; a dot marks files this thread edited.
    fn changes_list(&self, view: &Entity<ThreadView>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let touched = crate::changes::touched_relative(&self.project_dir, &view.read(cx).touched);
        let model = self.changes.read(cx);
        if !model.changes.is_repo || model.changes.files.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (muted, border, accent) = (theme.muted_foreground, theme.border, theme.primary);
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
                .hover(|el| el.bg(border.opacity(0.5)))
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
                .flex_none()
                .max_h(relative(0.4))
                .border_t_1()
                .border_color(border)
                .child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("Changes · {} files ·", model.changes.files.len()))
                                .child(div().text_color(added).child(format!("+{}", model.changes.additions())))
                                .child(div().text_color(deleted).child(format!("−{}", model.changes.deletions()))),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("review-changes")
                                .xsmall()
                                .ghost()
                                .label("Review")
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(AgentSlotEvent::OpenReview(None)))),
                        ),
                )
                .child(div().id("changes-list").pb_1().overflow_y_scroll().children(rows))
                .into_any_element(),
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
        let this = cx.entity();
        // Measure the tab, so the status column can fold away when it gets narrow.
        let measure = canvas(
            move |bounds, _, cx| {
                let width = bounds.size.width;
                if (this.read(cx).width - width).abs() > px(0.5) {
                    cx.defer(move |cx| {
                        this.update(cx, |slot, cx| {
                            slot.width = width;
                            cx.notify();
                        })
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let Some(view) = self.thread().cloned() else {
            let muted = cx.theme().muted_foreground;
            return div()
                .relative()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child("Pick a thread in the sidebar.")
                .child(measure);
        };
        let status = self.status_shown(cx).then(|| {
            let border = cx.theme().border;
            // Summary and tasks take the space; changed files sit above the agent's numbers,
            // which are pinned to the bottom.
            v_flex()
                .id("status")
                .test_support()
                .w(px(STATUS_WIDTH))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_color(border)
                .child(v_flex().flex_1().min_h_0().children(self.overview(&view, cx)))
                .children(self.changes_list(&view, cx))
                .children(self.metrics(&view, cx))
        });
        div()
            .relative()
            .size_full()
            .flex()
            .child(div().flex_1().min_w_0().h_full().child(view))
            .children(status)
            .child(measure)
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
