//! A project's history: the threads past the few the sidebar lists, last touched first, in a
//! dialog with a filter. Only their saved titles and dates are held; a thread's conversation
//! is read from disk when it's opened from here.

use std::rc::Rc;

use chrono::{DateTime, Datelike as _, TimeZone};
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::store::ThreadMeta;
use crate::workspace::Workspace;

const ROW_HEIGHT: f32 = 34.;
/// The list scrolls past this; only the rows in view are built, however long the history.
const MAX_LIST_HEIGHT: f32 = 440.;
const MARK_SIZE: f32 = 14.;

pub struct HistoryView {
    workspace: WeakEntity<Workspace>,
    project_id: String,
    filter: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl HistoryView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by title"));
        let mut subscriptions = vec![cx.subscribe_in(&filter, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::Change => cx.notify(),
                // Enter opens the first match.
                InputEvent::PressEnter { .. } => {
                    if let Some(first) = this.entries(cx).first() {
                        open(&this.workspace, &first.id, window, cx);
                    }
                }
                _ => {}
            }
        })];
        // Keep up with the workspace: a thread renamed, archived or brought back to the sidebar.
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.observe(&workspace, |_, _, cx| cx.notify()));
        }
        Self { workspace, project_id, filter, _subscriptions: subscriptions }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |i, cx| i.focus(window, cx));
    }

    /// The project's history, narrowed to the titles holding the filter's words.
    fn entries(&self, cx: &App) -> Vec<ThreadMeta> {
        let Some(workspace) = self.workspace.upgrade() else { return Vec::new() };
        let query = self.filter.read(cx).value().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        workspace
            .read(cx)
            .history(&self.project_id, cx)
            .into_iter()
            .filter(|t| {
                let title = t.title.as_deref().unwrap_or("New thread").to_lowercase();
                words.iter().all(|w| title.contains(w))
            })
            .collect()
    }
}

/// Close the dialog and show the thread in its project.
fn open(workspace: &WeakEntity<Workspace>, thread_id: &str, window: &mut Window, cx: &mut App) {
    window.close_dialog(cx);
    let _ = workspace.update(cx, |w, cx| w.open_thread(thread_id, window, cx));
}

impl Render for HistoryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = Rc::new(self.entries(cx));
        let theme = cx.theme();
        let (fg, muted, hover) = (theme.foreground, theme.muted_foreground, theme.accent);
        let now = chrono::Local::now();
        let height = (entries.len() as f32 * ROW_HEIGHT).min(MAX_LIST_HEIGHT);
        let workspace = self.workspace.clone();

        let list = {
            let entries = entries.clone();
            uniform_list("history-list", entries.len(), move |range, _, _| {
                range
                    .map(|ix| {
                        let meta = &entries[ix];
                        let (workspace, id) = (workspace.clone(), meta.id.clone());
                        h_flex()
                            .id(SharedString::from(format!("history-thread-{}", meta.id)))
                            .test_support()
                            .h(px(ROW_HEIGHT))
                            .px_2()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|el| el.bg(hover))
                            .on_click(move |_, window, cx| open(&workspace, &id, window, cx))
                            .child(
                                img(crate::agent_icon::icon(meta.agent, fg))
                                    .size(px(MARK_SIZE))
                                    .flex_none()
                                    .object_fit(ObjectFit::Contain),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .text_color(fg)
                                    .child(meta.title.clone().unwrap_or_else(|| "New thread".into())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(touched_label(meta.updated_at, &now)),
                            )
                    })
                    .collect()
            })
            .h(px(height))
        };

        v_flex().gap_3().child(Input::new(&self.filter).small().cleanable(true)).map(|el| {
            if entries.is_empty() {
                el.child(div().py_4().text_sm().text_color(muted).text_center().child("No threads match."))
            } else {
                el.child(list)
            }
        })
    }
}

/// When a thread was last touched, for the history list: the time for this week, the date
/// before that, and the year only when it isn't this one.
pub fn touched_label<Tz: TimeZone>(at: u64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(when) = now.timezone().timestamp_opt(at as i64, 0).single() else { return String::new() };
    let time = when.format("%-I:%M %p");
    match (now.date_naive() - when.date_naive()).num_days() {
        // A clock set back can make a thread look touched in the future; call it today.
        ..=0 => format!("Today, {time}"),
        1 => format!("Yesterday, {time}"),
        2..=6 => format!("{}, {time}", when.format("%A")),
        _ if when.year() == now.year() => when.format("%b %-d").to_string(),
        _ => when.format("%b %-d, %Y").to_string(),
    }
}

#[cfg(test)]
mod tests {
    // Named imports: `super::*` would bring in gpui's `test` attribute in place of the standard one.
    use super::touched_label;
    use chrono::{TimeZone as _, Utc};

    #[test]
    fn touched_labels_get_coarser_with_age() {
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 15, 30, 0).unwrap();
        let at = |y, m, d, h, min| Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap().timestamp() as u64;
        assert_eq!(touched_label(at(2026, 11, 24, 9, 5), &now), "Today, 9:05 AM");
        assert_eq!(touched_label(at(2026, 11, 23, 23, 59), &now), "Yesterday, 11:59 PM");
        assert_eq!(touched_label(at(2026, 11, 20, 14, 0), &now), "Friday, 2:00 PM");
        assert_eq!(touched_label(at(2026, 11, 3, 14, 0), &now), "Nov 3");
        assert_eq!(touched_label(at(2025, 12, 31, 14, 0), &now), "Dec 31, 2025");
        assert_eq!(touched_label(at(2026, 11, 25, 8, 0), &now), "Today, 8:00 AM");
    }
}
