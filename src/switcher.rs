//! The switcher (Ctrl+Space by default): every project and thread in one filtered list.
//! Picking a project opens a new thread's start screen in it; picking a thread opens it.
//!
//! The filter keeps the keyboard: Up/Down move through the list, Enter opens the row picked,
//! Escape closes. A thread matches on its title or its project's name, so typing a project's
//! name lists the project and its threads.

use std::rc::Rc;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    input::{Input, InputEvent, InputState, MoveDown, MoveUp},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::agent::AgentKind;
use crate::workspace::Workspace;

const ROW_HEIGHT: f32 = 34.;
/// The list scrolls past this; only the rows in view are built.
const MAX_LIST_HEIGHT: f32 = 420.;
const MARK_SIZE: f32 = 14.;
const AVATAR_SIZE: f32 = 18.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Project,
    Thread,
}

/// One row: a project, or one of its threads.
#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: EntryKind,
    /// The project's id for a project, the thread's for a thread.
    pub id: String,
    pub project_id: String,
    pub title: String,
    /// A project's folder.
    pub detail: String,
    pub project_name: String,
    pub updated_at: u64,
    pub agent: AgentKind,
    pub archived: bool,
}

impl Entry {
    /// Whether every word of the (lower-case) query is in the row's title or its project's name.
    pub fn matches(&self, words: &[&str]) -> bool {
        let haystack = match self.kind {
            EntryKind::Project => self.title.to_lowercase(),
            EntryKind::Thread => format!("{} {}", self.title, self.project_name).to_lowercase(),
        };
        words.iter().all(|w| haystack.contains(w))
    }
}

pub struct SwitcherView {
    workspace: WeakEntity<Workspace>,
    filter: Entity<InputState>,
    /// The row picked, in the filtered list.
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SwitcherView {
    pub fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Find a project or thread"));
        let subscriptions =
            vec![cx.subscribe_in(&filter, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.selected = 0;
                    this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.open_selected(window, cx),
                _ => {}
            })];
        Self { workspace, filter, selected: 0, scroll: UniformListScrollHandle::new(), _subscriptions: subscriptions }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |i, cx| i.focus(window, cx));
    }

    fn entries(&self, cx: &App) -> Vec<Entry> {
        let Some(workspace) = self.workspace.upgrade() else { return Vec::new() };
        let query = self.filter.read(cx).value().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        workspace.read(cx).switcher_entries().into_iter().filter(|e| e.matches(&words)).collect()
    }

    fn move_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.entries(cx).len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + step).rem_euclid(count as isize) as usize;
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries(cx).get(self.selected).cloned() {
            open(&self.workspace, &entry, window, cx);
        }
    }
}

/// Close the switcher and open the row: a project's start screen, or the thread.
fn open(workspace: &WeakEntity<Workspace>, entry: &Entry, window: &mut Window, cx: &mut App) {
    window.close_dialog(cx);
    let _ = workspace.update(cx, |w, cx| match entry.kind {
        EntryKind::Project => w.new_thread(&entry.project_id, window, cx),
        EntryKind::Thread => w.open_thread(&entry.id, window, cx),
    });
}

impl Render for SwitcherView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = Rc::new(self.entries(cx));
        self.selected = self.selected.min(entries.len().saturating_sub(1));
        let selected = self.selected;
        let theme = cx.theme();
        let (fg, muted, hover, picked) =
            (theme.foreground, theme.foreground.opacity(0.6), theme.accent.opacity(0.5), theme.accent);
        let now = chrono::Local::now();
        let height = (entries.len() as f32 * ROW_HEIGHT).min(MAX_LIST_HEIGHT);
        let workspace = self.workspace.clone();
        let current = workspace.upgrade().and_then(|w| w.read(cx).active_project_id());

        let list = {
            let entries = entries.clone();
            uniform_list("switcher-list", entries.len(), move |range, _, cx| {
                let ws = workspace.upgrade();
                range
                    .map(|ix| {
                        let entry = &entries[ix];
                        let (workspace, clicked) = (workspace.clone(), entry.clone());
                        let row = h_flex()
                            .id(SharedString::from(format!("switcher-{}", entry.id)))
                            .test_support()
                            .w_full()
                            .h(px(ROW_HEIGHT))
                            .px_2()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(ix == selected, |el| el.bg(picked))
                            .hover(|el| el.bg(hover))
                            .on_click(move |_, window, cx| open(&workspace, &clicked, window, cx));
                        let title = |text: String, weight: FontWeight, color: Hsla| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_sm()
                                .font_weight(weight)
                                .text_color(color)
                                .child(text)
                        };
                        match entry.kind {
                            EntryKind::Project => {
                                let avatar = ws.as_ref().and_then(|w| {
                                    let w = w.read(cx);
                                    w.project_by_id(&entry.project_id).map(|p| w.project_avatar(p, AVATAR_SIZE, cx))
                                });
                                let here = current.as_deref() == Some(entry.project_id.as_str());
                                row.children(avatar)
                                    .child(title(entry.title.clone(), FontWeight::SEMIBOLD, fg))
                                    .child(
                                        div()
                                            .flex_none()
                                            .max_w(px(220.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(if here {
                                                format!("{} · current", entry.detail)
                                            } else {
                                                entry.detail.clone()
                                            }),
                                    )
                                    .child(div().flex_none().text_xs().text_color(muted).child("New thread"))
                            }
                            EntryKind::Thread => row
                                .pl(px(8. + AVATAR_SIZE))
                                .child(
                                    img(crate::agent_icon::icon(entry.agent, fg))
                                        .size(px(MARK_SIZE))
                                        .flex_none()
                                        .object_fit(ObjectFit::Contain),
                                )
                                .child(title(
                                    entry.title.clone(),
                                    FontWeight::NORMAL,
                                    if entry.archived { muted } else { fg },
                                ))
                                .child(div().flex_none().text_xs().text_color(muted).child(if entry.archived {
                                    "Archived".to_string()
                                } else {
                                    crate::history::touched_label(entry.updated_at, &now)
                                })),
                        }
                    })
                    .collect()
            })
            .track_scroll(&self.scroll)
            .w_full()
            .h(px(height))
        };

        v_flex()
            .gap_3()
            // The filter keeps the keyboard; Up and Down move through the list instead of its text.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_selection(-1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_selection(1, cx);
                cx.stop_propagation();
            }))
            .child(Input::new(&self.filter).cleanable(true))
            .map(|el| {
                if entries.is_empty() {
                    el.child(
                        div().py_4().text_sm().text_color(muted).text_center().child("No projects or threads match."),
                    )
                } else {
                    el.child(list)
                }
            })
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("↑↓ move · Enter opens · a project starts a new thread · Esc closes"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, EntryKind};

    fn entry(kind: EntryKind, title: &str, project: &str) -> Entry {
        Entry {
            kind,
            id: title.into(),
            project_id: project.into(),
            title: title.into(),
            detail: String::new(),
            project_name: project.into(),
            updated_at: 0,
            agent: Default::default(),
            archived: false,
        }
    }

    #[test]
    fn a_thread_matches_on_its_title_or_its_project() {
        let thread = entry(EntryKind::Thread, "Fix the parser", "Vord");
        assert!(thread.matches(&["parser"]));
        assert!(thread.matches(&["vord", "fix"]), "words can come from both");
        assert!(!thread.matches(&["rustle"]));
        let project = entry(EntryKind::Project, "Vord", "Vord");
        assert!(project.matches(&["vo"]) && !project.matches(&["parser"]));
        assert!(project.matches(&[]), "no words: everything");
    }
}
