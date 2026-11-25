//! A project's one agent tab: the conversation of the thread picked in the sidebar, with its
//! status column on the right (the overview, the project's changed files and the agent's
//! numbers). Picking another thread swaps what's in it; the panes and terminals around it stay
//! as they are. The column folds away when the tab is narrow, and can be shown or hidden by hand.

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, button::Button, button::ButtonVariants as _};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::changes::{ChangeKind, ProjectChanges};
use crate::thread::{self, ThreadView};

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
        Self {
            thread: None,
            project_dir,
            changes,
            status_choice: None,
            // Until it's measured, assume there's room.
            width: px(f32::MAX),
            _thread_subscription: None,
            _subscriptions: vec![observe],
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

    /// The agent's numbers, pinned to the bottom of the column: context, usage windows, cost
    /// and model.
    fn metrics(&self, view: &Entity<ThreadView>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = view.read(cx);
        let mut stats = Vec::new();
        if let (Some(tokens), Some(window)) = (t.context_tokens, t.context_window) {
            let percent = tokens as f64 / window.max(1) as f64 * 100.0;
            stats.push(format!("ctx {} / {} · {percent:.0}%", compact(tokens), compact(window)));
        }
        for window in &t.usage {
            let name = match window.id.as_str() {
                "five_hour" => "5h",
                "seven_day" => "7d",
                other => other,
            };
            stats.push(format!("{name} {:.0}%", window.utilization * 100.0));
        }
        if t.cost_usd > 0.0 {
            // On a subscription this is Claude's estimate, not a charge.
            let approx = if t.usage.is_empty() { "" } else { "≈" };
            stats.push(if t.cost_usd < 1.0 {
                format!("{approx}${:.3}", t.cost_usd)
            } else {
                format!("{approx}${:.2}", t.cost_usd)
            });
        }
        if let Some(model) = &t.model {
            stats.push(thread::short_model(model));
        }
        if stats.is_empty() {
            return None;
        }
        let theme = cx.theme();
        Some(
            div()
                .flex_none()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(stats.join("  ·  "))
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
