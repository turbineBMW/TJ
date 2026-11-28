//! The Notes tab: a project's notes ([`crate::notes`]), the things put off during a session so
//! they aren't lost. The list (to do, done or all) is on the left and the picked note on the
//! right: where it was written (with a way back to that message), its body, and Work on this,
//! which starts a thread for it. Claude adds and closes notes through the notes server; they
//! show here as they change.
//!
//! The cleanup page lists the notes of projects removed from TJ: move them into this project,
//! copy them out as Markdown, or delete them.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    input::{Escape, Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    text::TextView,
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::notes::{Author, Kind, NewNote, Note, NoteStatus, Notebook, NotesStore};
use crate::workspace::Workspace;

pub const CONTEXT: &str = "Notes";
const LIST_WIDTH: f32 = 320.0;

gpui_kit::actions!(
    notes,
    [
        NextNote,
        PreviousNote,
        AddNote,
        EditNote,
        ToggleDone,
        WorkOnNote,
        GoToSource,
        ShowPending,
        ShowDone,
        ShowAll,
        DeleteNote,
        ToggleCleanup,
        MoveHere,
        CopyMarkdown
    ]
);

/// The tab's own keys; they apply while it has focus and a text box doesn't take them.
pub fn bindings() -> Vec<KeyBinding> {
    let cx = Some(CONTEXT);
    vec![
        KeyBinding::new("down", NextNote, cx),
        KeyBinding::new("up", PreviousNote, cx),
        KeyBinding::new("alt-n", AddNote, cx),
        KeyBinding::new("enter", EditNote, cx),
        KeyBinding::new("alt-d", ToggleDone, cx),
        KeyBinding::new("alt-w", WorkOnNote, cx),
        KeyBinding::new("alt-t", GoToSource, cx),
        KeyBinding::new("alt-1", ShowPending, cx),
        KeyBinding::new("alt-2", ShowDone, cx),
        KeyBinding::new("alt-3", ShowAll, cx),
        KeyBinding::new("delete", DeleteNote, cx),
        KeyBinding::new("alt-c", ToggleCleanup, cx),
        KeyBinding::new("alt-m", MoveHere, cx),
        KeyBinding::new("alt-y", CopyMarkdown, cx),
    ]
}

pub enum NotesEvent {
    /// Show a thread, at the user's `prompt`th message when known.
    OpenThread { thread: String, prompt: Option<usize> },
    /// Start a thread to work on the note.
    WorkOn(u64),
}

impl EventEmitter<NotesEvent> for NotesView {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    Pending,
    Done,
    All,
}

impl Filter {
    const ALL: [Filter; 3] = [Filter::Pending, Filter::Done, Filter::All];

    fn label(self) -> &'static str {
        match self {
            Filter::Pending => "To do",
            Filter::Done => "Done",
            Filter::All => "All",
        }
    }

    fn keeps(self, note: &Note) -> bool {
        match self {
            Filter::Pending => note.status.is_pending(),
            Filter::Done => note.status == NoteStatus::Done,
            Filter::All => true,
        }
    }
}

/// A note being written or changed.
struct Editor {
    /// The note changed; `None` for a new one.
    number: Option<u64>,
    kind: Kind,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    _subscriptions: Vec<Subscription>,
}

pub struct NotesView {
    store: Entity<NotesStore>,
    /// For thread titles and the list of projects (read only).
    workspace: WeakEntity<Workspace>,
    project_id: String,
    project_name: String,
    project_path: PathBuf,
    filter: Filter,
    selected: Option<u64>,
    editor: Option<Editor>,
    /// Showing the notes of removed projects instead of this project's.
    cleanup: bool,
    /// The removed project picked on the cleanup page, by its id.
    cleanup_selected: Option<String>,
    /// What the header says after copying.
    flash: Option<String>,
    list_scroll: ScrollHandle,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl NotesView {
    pub fn new(
        store: Entity<NotesStore>,
        workspace: WeakEntity<Workspace>,
        project_id: String,
        project_name: String,
        project_path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        // Thread titles and the project list are read from the workspace as the tab draws; it
        // isn't observed, since the workspace observes the panes, which observe this tab.
        let subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        Self {
            store,
            workspace,
            project_id,
            project_name,
            project_path,
            filter: Filter::Pending,
            selected: None,
            editor: None,
            cleanup: false,
            cleanup_selected: None,
            flash: None,
            list_scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// The tab's title, with how many notes are still to do.
    pub fn title(&self, cx: &App) -> String {
        match self.store.read(cx).pending(&self.project_id) {
            0 => "Notes".into(),
            n => format!("Notes · {n}"),
        }
    }

    /// The tab came to the front: focus it (or the note being written).
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.editor {
            Some(editor) => editor.title.update(cx, |t, cx| t.focus(window, cx)),
            None => window.focus(&self.focus, cx),
        }
    }

    #[cfg(test)]
    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    #[cfg(test)]
    pub fn is_editing(&self) -> bool {
        self.editor.is_some()
    }

    #[cfg(test)]
    pub fn is_cleaning_up(&self) -> bool {
        self.cleanup
    }

    fn book<'a>(&self, cx: &'a App) -> Option<&'a Notebook> {
        self.store.read(cx).book(&self.project_id)
    }

    /// The notes the list shows, in its order: in progress, then open, newest first; done ones
    /// by when they were last changed.
    fn visible(&self, cx: &App) -> Vec<Note> {
        let Some(book) = self.book(cx) else { return Vec::new() };
        let mut notes: Vec<Note> = book.notes.iter().filter(|n| self.filter.keeps(n)).cloned().collect();
        let rank = |s: NoteStatus| match s {
            NoteStatus::Doing => 0,
            NoteStatus::Open => 1,
            NoteStatus::Done => 2,
        };
        notes.sort_by(|a, b| {
            rank(a.status).cmp(&rank(b.status)).then_with(|| match a.status {
                NoteStatus::Done => b.updated.cmp(&a.updated),
                _ => b.number.cmp(&a.number),
            })
        });
        notes
    }

    /// The picked note, falling back to the first one shown.
    fn current(&self, cx: &App) -> Option<Note> {
        let visible = self.visible(cx);
        visible.iter().find(|n| Some(n.number) == self.selected).or_else(|| visible.first()).cloned()
    }

    fn select_step(&mut self, step: isize, cx: &mut Context<Self>) {
        self.flash = None;
        if self.cleanup {
            let ids: Vec<String> = self.orphans(cx).into_iter().map(|(id, _)| id).collect();
            let at = ids.iter().position(|id| Some(id) == self.cleanup_selected.as_ref());
            let next = match at {
                Some(at) => (at as isize + step).clamp(0, ids.len() as isize - 1) as usize,
                None => 0,
            };
            self.cleanup_selected = ids.get(next).cloned();
            cx.notify();
            return;
        }
        let numbers: Vec<u64> = self.visible(cx).iter().map(|n| n.number).collect();
        if numbers.is_empty() {
            return;
        }
        let at = numbers.iter().position(|n| Some(*n) == self.current(cx).map(|c| c.number));
        let next = match at {
            Some(at) => (at as isize + step).clamp(0, numbers.len() as isize - 1) as usize,
            None => 0,
        };
        self.selected = Some(numbers[next]);
        self.list_scroll.scroll_to_item(next);
        cx.notify();
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.flash = None;
        self.filter = filter;
        cx.notify();
    }

    /// Change this project's notes and save them.
    fn edit<R>(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Notebook) -> R) -> Option<R> {
        let (id, name, path) = (self.project_id.clone(), self.project_name.clone(), self.project_path.clone());
        self.store.update(cx, |s, cx| s.edit(&id, &name, &path, change, cx))
    }

    // ---------- writing ----------

    fn start_editor(&mut self, note: Option<&Note>, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup {
            return;
        }
        let title_text = note.map(|n| n.title.clone()).unwrap_or_default();
        let body_text = note.map(|n| n.body.clone()).unwrap_or_default();
        let title = cx.new(|cx| InputState::new(window, cx).default_value(title_text).placeholder("What's put off?"));
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(6, 24)
                .placeholder("What to do, why, and where, so a fresh session can pick it up  (Markdown)")
        });
        body.update(cx, |b, cx| b.set_value(body_text, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&title, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { secondary: true, .. } => this.save_editor(window, cx),
                InputEvent::PressEnter { .. } => {
                    if let Some(editor) = &this.editor {
                        editor.body.update(cx, |b, cx| b.focus(window, cx));
                    }
                }
                _ => {}
            }),
            cx.subscribe_in(&body, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { secondary: true, .. } = event {
                    this.save_editor(window, cx)
                }
            }),
        ];
        title.update(cx, |t, cx| t.focus(window, cx));
        self.editor = Some(Editor {
            number: note.map(|n| n.number),
            kind: note.map(|n| n.kind).unwrap_or_default(),
            title,
            body,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn cancel_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else { return };
        let title = editor.title.read(cx).value().trim().to_string();
        if title.is_empty() {
            editor.title.update(cx, |t, cx| t.focus(window, cx));
            return;
        }
        let body = editor.body.read(cx).value().trim().to_string();
        let (number, kind) = (editor.number, editor.kind);
        let saved = match number {
            Some(number) => self.edit(cx, |book| {
                if let Some(note) = book.get_mut(number) {
                    (note.title, note.body, note.kind) = (title, body, kind);
                    note.updated = crate::store::now();
                }
                number
            }),
            None => {
                // A note written here belongs to the thread on show, if any.
                let thread = self.workspace.upgrade().and_then(|w| w.read(cx).shown_thread(&self.project_id, cx));
                let (branch, commit) = crate::notes::git_state(&self.project_path);
                let source = crate::notes::Source { thread, prompt: None, branch, commit };
                let new = NewNote { title, body, kind, author: Author::User, source: Some(source) };
                self.edit(cx, |book| book.add(new))
            }
        };
        if let Some(number) = saved {
            self.selected = Some(number);
            // A new note is to do: show it even if the list was on Done.
            if number_is_hidden(self.filter, number, self.book(cx)) {
                self.filter = Filter::Pending;
            }
            self.editor = None;
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    // ---------- note actions ----------

    fn toggle_done(&mut self, cx: &mut Context<Self>) {
        let Some(note) = self.current(cx) else { return };
        let number = note.number;
        let next = if note.status == NoteStatus::Done { NoteStatus::Open } else { NoteStatus::Done };
        self.edit(cx, |book| {
            if let Some(note) = book.get_mut(number) {
                note.status = next;
                note.updated = crate::store::now();
            }
        });
        // Keep the picked note on the list's next one, now this one left it.
        if !self.filter.keeps(&Note { status: next, ..note }) {
            self.selected = None;
        }
        cx.notify();
    }

    fn work_on(&mut self, cx: &mut Context<Self>) {
        if let Some(note) = self.current(cx).filter(|n| n.status != NoteStatus::Done) {
            cx.emit(NotesEvent::WorkOn(note.number));
        }
    }

    fn go_to_source(&mut self, cx: &mut Context<Self>) {
        let Some(note) = self.current(cx) else { return };
        let Some(source) =
            note.source.filter(|s| s.thread.as_ref().is_some_and(|t| self.thread_title(t, cx).is_some()))
        else {
            return;
        };
        if let Some(thread) = source.thread {
            cx.emit(NotesEvent::OpenThread { thread, prompt: source.prompt });
        }
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup {
            self.confirm_delete_orphan(window, cx);
            return;
        }
        let Some(note) = self.current(cx) else { return };
        let this = cx.entity().downgrade();
        let number = note.number;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .confirm()
                .title(format!("Delete note #{number}?"))
                .description("It's gone for good. Marking it done keeps it in the Done list instead.")
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |v, cx| {
                        v.edit(cx, |book| book.remove(number));
                        v.selected = None;
                        window.focus(&v.focus, cx);
                        cx.notify();
                    });
                    true
                })
        });
    }

    // ---------- cleanup ----------

    /// Notes of projects that aren't in TJ any more.
    fn orphans(&self, cx: &App) -> Vec<(String, Notebook)> {
        let Some(workspace) = self.workspace.upgrade() else { return Vec::new() };
        let projects = workspace.read(cx).project_list();
        let live: Vec<&str> = projects.iter().map(|(id, _, _)| id.as_str()).collect();
        self.store.read(cx).orphans(&live).into_iter().map(|(id, book)| (id.to_string(), book.clone())).collect()
    }

    fn picked_orphan(&self, cx: &App) -> Option<(String, Notebook)> {
        let orphans = self.orphans(cx);
        let at = orphans.iter().position(|(id, _)| Some(id) == self.cleanup_selected.as_ref()).unwrap_or(0);
        orphans.into_iter().nth(at)
    }

    fn toggle_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cleanup = !self.cleanup && self.editor.is_none();
        self.flash = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Move the picked removed project's notes into `to` (this project when `None`).
    fn move_orphan(&mut self, to: Option<(String, String, PathBuf)>, cx: &mut Context<Self>) {
        let Some((from, _)) = self.picked_orphan(cx) else { return };
        let (id, name, path) =
            to.unwrap_or_else(|| (self.project_id.clone(), self.project_name.clone(), self.project_path.clone()));
        self.store.update(cx, |s, cx| s.merge_into(&from, &id, &name, &path, cx));
        self.cleanup_selected = None;
        self.flash = Some(format!("Moved into {name}"));
        cx.notify();
    }

    fn copy_orphan(&mut self, cx: &mut Context<Self>) {
        let book = if self.cleanup { self.picked_orphan(cx).map(|(_, b)| b) } else { self.book(cx).cloned() };
        let Some(book) = book else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(book.to_markdown()));
        self.flash = Some(format!("Copied {}'s notes as Markdown", book.project_name));
        cx.notify();
    }

    fn confirm_delete_orphan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, book)) = self.picked_orphan(cx) else { return };
        let this = cx.entity().downgrade();
        let count = book.notes.len();
        let name = book.project_name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (this, id) = (this.clone(), id.clone());
            alert
                .confirm()
                .title(format!("Delete {name}'s notes?"))
                .description(format!(
                    "{count} note{} from a project removed from TJ. They're gone for good; copy them as Markdown first to keep them.",
                    if count == 1 { "" } else { "s" }
                ))
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |v, cx| {
                        let store = v.store.clone();
                        store.update(cx, |s, cx| s.delete_book(&id, cx));
                        v.cleanup_selected = None;
                        window.focus(&v.focus, cx);
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn thread_title(&self, thread_id: &str, cx: &App) -> Option<String> {
        self.workspace.upgrade().and_then(|w| w.read(cx).thread_title(thread_id))
    }

    // ---------- rendering ----------

    fn colors(cx: &App) -> Colors {
        let (ansi, _, _) = crate::terminal::palette(cx);
        let theme = cx.theme();
        Colors {
            fg: theme.foreground,
            muted: theme.muted_foreground,
            border: theme.border,
            card: theme.secondary,
            bg: theme.background,
            accent: theme.primary,
            green: ansi[2],
            red: ansi[1],
            yellow: ansi[3],
            purple: ansi[5],
            blue: ansi[4],
        }
    }

    fn render_header(&self, orphans: usize, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self.store.read(cx).error.clone();
        let (pending, total) = self.book(cx).map_or((0, 0), |b| (b.pending(), b.notes.len()));
        h_flex()
            .flex_none()
            .px_4()
            .py_2p5()
            .gap_3()
            .border_b_1()
            .border_color(c.border)
            .child(Icon::new(IconName::StickyNote).text_color(c.accent))
            .child(
                v_flex()
                    .min_w_0()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(if self.cleanup {
                        "Notes from removed projects".to_string()
                    } else {
                        format!("{} notes", self.project_name)
                    }))
                    .child(div().text_xs().text_color(c.muted).child(if self.cleanup {
                        "Removing a project keeps its notes. Adding its folder again brings them back.".to_string()
                    } else if total == 0 {
                        "Nothing put off yet".to_string()
                    } else {
                        format!("{pending} to do · {} done", total - pending)
                    })),
            )
            .child(div().flex_1())
            .when_some(error, |el, error| {
                el.child(div().text_xs().text_color(c.red).max_w(px(320.)).line_clamp(2).child(error))
            })
            .when_some(self.flash.clone(), |el, flash| el.child(div().text_xs().text_color(c.green).child(flash)))
            .when(!self.cleanup && orphans > 0, |el| {
                el.child(
                    Button::new("notes-cleanup")
                        .small()
                        .ghost()
                        .icon(IconName::FolderX)
                        .label(format!("Removed projects · {orphans}"))
                        .tooltip("Notes left by projects removed from TJ  (Alt+C)")
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_cleanup(window, cx))),
                )
            })
            .when(self.cleanup, |el| {
                el.child(
                    Button::new("notes-cleanup-back")
                        .small()
                        .outline()
                        .icon(IconName::ArrowLeft)
                        .label("Back to notes")
                        .tooltip("Alt+C or Escape")
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_cleanup(window, cx))),
                )
            })
            .when(!self.cleanup, |el| {
                el.child(
                    Button::new("notes-add")
                        .small()
                        .primary()
                        .icon(IconName::Plus)
                        .label("New note")
                        .tooltip("Alt+N")
                        .disabled(self.editor.is_some())
                        .on_click(cx.listener(|this, _, window, cx| this.start_editor(None, window, cx))),
                )
            })
    }

    fn render_list(
        &self,
        visible: &[Note],
        current: Option<u64>,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let counts =
            self.book(cx).map_or([0; 3], |b| Filter::ALL.map(|f| b.notes.iter().filter(|n| f.keeps(n)).count()));
        let filter = self.filter;
        let segments = Filter::ALL.into_iter().zip(counts).map(|(f, count)| {
            let on = f == filter;
            div()
                .id(SharedString::from(format!("notes-filter-{}", f.label())))
                .test_support()
                .flex_1()
                .py_1()
                .rounded_md()
                .text_center()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .map(|el| {
                    if on {
                        el.bg(c.bg).text_color(c.fg).shadow_sm()
                    } else {
                        el.text_color(c.muted).hover(|el| el.text_color(c.fg))
                    }
                })
                .child(format!("{} · {count}", f.label()))
                .on_click(cx.listener(move |this, _, _, cx| this.set_filter(f, cx)))
        });
        let rows = visible.iter().map(|note| {
            let selected = current == Some(note.number);
            let number = note.number;
            let (icon, color) = kind_look(note.kind, c);
            let done = note.status == NoteStatus::Done;
            div()
                .id(SharedString::from(format!("notes-row-{number}")))
                .test_support()
                .relative()
                .mx_2()
                .mb_1()
                .px_3()
                .py_2()
                .rounded_lg()
                .cursor_pointer()
                .map(|el| if selected { el.bg(c.card) } else { el.hover(|el| el.bg(c.card.opacity(0.5))) })
                .when(selected, |el| {
                    el.child(
                        div().absolute().left_0().top(px(10.)).bottom(px(10.)).w(px(3.)).rounded_full().bg(c.accent),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    this.selected = Some(number);
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .gap_2p5()
                        .items_start()
                        .child(div().pt_0p5().child(Icon::new(icon).small().text_color(if done {
                            c.muted
                        } else {
                            color
                        })))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .line_clamp(2)
                                        .when(done, |el| el.text_color(c.muted).line_through())
                                        .child(note.title.clone()),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(c.muted)
                                        .child(format!("#{number}"))
                                        .child("·")
                                        .child(if note.author == Author::Agent { "Claude" } else { "You" })
                                        .child("·")
                                        .child(ago(note.created))
                                        .child(div().flex_1())
                                        .when(note.status == NoteStatus::Doing, |el| {
                                            el.child(pill("In progress", c.blue))
                                        }),
                                ),
                        ),
                )
                .into_any_element()
        });
        let empty = visible.is_empty();
        v_flex()
            .w(px(LIST_WIDTH))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(c.border)
            .child(h_flex().m_2().p_0p5().gap_0p5().rounded_lg().bg(c.card).children(segments))
            .child(
                div()
                    .id("notes-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .pb_2()
                    .map(|el| {
                        if empty {
                            el.child(
                                v_flex()
                                    .items_center()
                                    .gap_2()
                                    .pt_10()
                                    .px_6()
                                    .text_sm()
                                    .text_center()
                                    .text_color(c.muted)
                                    .child(Icon::new(IconName::StickyNote).large().text_color(c.muted.opacity(0.5)))
                                    .child(match filter {
                                        Filter::Pending => "Nothing to do. Claude adds a note when you put something off, or press Alt+N.",
                                        Filter::Done => "No notes done yet",
                                        Filter::All => "No notes yet",
                                    }),
                            )
                        } else {
                            el.children(rows)
                        }
                    }),
            )
    }

    fn render_editor(&self, editor: &Editor, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let kind = editor.kind;
        let this = cx.entity();
        let kind_menu = Button::new("notes-editor-kind")
            .small()
            .outline()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(kind_look(kind, c).0).small().text_color(kind_look(kind, c).1))
                    .child(kind.label())
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(c.muted)),
            )
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.label("Kind of note");
                for k in Kind::ALL {
                    let this = this.clone();
                    menu = menu.item(PopupMenuItem::new(k.label()).checked(k == kind).on_click(move |_, _, cx| {
                        this.update(cx, |v, cx| {
                            if let Some(editor) = &mut v.editor {
                                editor.kind = k;
                            }
                            cx.notify();
                        })
                    }));
                }
                menu
            });
        v_flex()
            .id("notes-editor")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p_5()
            .gap_3()
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                this.cancel_editor(window, cx);
                cx.stop_propagation();
            }))
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(match editor.number {
                Some(n) => format!("Edit note #{n}"),
                None => "New note".to_string(),
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .child(Input::new(&editor.title))
                            .child(crate::caret::overlay(&editor.title)),
                    )
                    .child(kind_menu),
            )
            .child(Textarea::new(&editor.body))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(c.muted)
                            .child("Enter: to the body · Ctrl+Enter: save · Escape: cancel"),
                    )
                    .child(
                        Button::new("notes-editor-cancel")
                            .small()
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.cancel_editor(window, cx))),
                    )
                    .child(
                        Button::new("notes-editor-save")
                            .small()
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, window, cx| this.save_editor(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_detail(&self, note: Option<Note>, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let Some(note) = note else {
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(c.muted)
                .child(Icon::new(IconName::StickyNote).large().text_color(c.muted.opacity(0.5)))
                .child("Pick a note, or press Alt+N to write one")
                .into_any_element();
        };
        let number = note.number;
        let (icon, color) = kind_look(note.kind, c);
        let done = note.status == NoteStatus::Done;
        let source = note.source.clone().unwrap_or_default();
        let source_title = source.thread.as_deref().map(|t| (t.to_string(), self.thread_title(t, cx)));
        let work_title = note.work_thread.as_deref().map(|t| (t.to_string(), self.thread_title(t, cx)));
        let at: Vec<String> =
            [source.branch.clone().map(|b| format!("on {b}")), source.commit.clone().map(|c| format!("at {c}"))]
                .into_iter()
                .flatten()
                .collect();
        let written = format!(
            "{} by {} {}",
            if note.author == Author::Agent { "Added" } else { "Written" },
            if note.author == Author::Agent { "Claude" } else { "you" },
            ago_long(note.created)
        );
        let thread_link = |id: &'static str,
                           label: &str,
                           thread: (String, Option<String>),
                           prompt: Option<usize>,
                           cx: &mut Context<Self>| {
            let (thread_id, title) = thread;
            match title {
                Some(title) => Button::new(id)
                    .xsmall()
                    .ghost()
                    .icon(IconName::MessageSquareText)
                    .label(format!("{label} {title}"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(NotesEvent::OpenThread { thread: thread_id.clone(), prompt })
                    }))
                    .into_any_element(),
                None => div()
                    .text_xs()
                    .text_color(c.muted)
                    .child(format!("{label} a thread that was deleted"))
                    .into_any_element(),
            }
        };
        let source_link = source_title.map(|t| {
            let link = thread_link("notes-source", "From", t, source.prompt, cx);
            h_flex().gap_1().child(link).child(div().text_xs().text_color(c.muted).child("Alt+T")).into_any_element()
        });
        let work_link = work_title.map(|t| thread_link("notes-work-thread", "Worked on in", t, None, cx));
        v_flex()
            .id("notes-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .id("notes-detail-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_5()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(pill_icon(icon, note.kind.label(), color))
                            .child(pill(
                                note.status.label(),
                                match note.status {
                                    NoteStatus::Open => c.yellow,
                                    NoteStatus::Doing => c.blue,
                                    NoteStatus::Done => c.green,
                                },
                            ))
                            .child(div().text_xs().text_color(c.muted).child(format!("#{number}"))),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(done, |el| el.text_color(c.muted))
                            .child(note.title.clone()),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(c.muted)
                            .child(if at.is_empty() { written } else { format!("{written}, {}", at.join(" ")) })
                            .children(source_link)
                            .children(work_link),
                    )
                    .when_some(note.resolution.clone(), |el, resolution| {
                        el.child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .p_3()
                                .rounded_lg()
                                .bg(c.green.opacity(0.1))
                                .child(Icon::new(IconName::CircleCheck).small().text_color(c.green))
                                .child(div().flex_1().text_sm().child(resolution)),
                        )
                    })
                    .child(if note.body.is_empty() {
                        div()
                            .text_sm()
                            .text_color(c.muted)
                            .child("No details. Press Enter to add some.")
                            .into_any_element()
                    } else {
                        div()
                            .text_sm()
                            .child(
                                TextView::markdown(
                                    SharedString::from(format!("note-body-{number}")),
                                    note.body.clone(),
                                )
                                .selectable(true),
                            )
                            .into_any_element()
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .flex_wrap()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(c.border)
                    .when(!done, |el| {
                        el.child(
                            Button::new("notes-work-on")
                                .small()
                                .primary()
                                .icon(IconName::Play)
                                .label("Work on this")
                                .tooltip("Start a thread with this note as the prompt  (Alt+W)")
                                .on_click(cx.listener(|this, _, _, cx| this.work_on(cx))),
                        )
                    })
                    .child(
                        Button::new("notes-toggle-done")
                            .small()
                            .outline()
                            .icon(if done { IconName::RotateCcw } else { IconName::Check })
                            .label(if done { "Reopen" } else { "Mark done" })
                            .tooltip("Alt+D")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_done(cx))),
                    )
                    .child(
                        Button::new("notes-edit")
                            .small()
                            .outline()
                            .icon(IconName::Pencil)
                            .label("Edit")
                            .tooltip("Enter")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let note = this.book(cx).and_then(|b| b.get(number)).cloned();
                                this.start_editor(note.as_ref(), window, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("notes-delete")
                            .small()
                            .ghost()
                            .icon(IconName::Trash)
                            .tooltip("Delete the note  (Delete)")
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx))),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px_5()
                    .pb_2()
                    .text_xs()
                    .text_color(c.muted)
                    .child("↑↓ pick · Alt+1/2/3 To do, Done, All · Alt+N new · Alt+Y copy all as Markdown"),
            )
            .into_any_element()
    }

    fn render_cleanup(&self, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let orphans = self.orphans(cx);
        if orphans.is_empty() {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(c.muted)
                .child(Icon::new(IconName::CircleCheck).large().text_color(c.green))
                .child("No notes left from removed projects")
                .into_any_element();
        }
        let picked = self.picked_orphan(cx).map(|(id, _)| id);
        let projects = self.workspace.upgrade().map(|w| w.read(cx).project_list()).unwrap_or_default();
        let rows = orphans.into_iter().map(|(id, book)| {
            let selected = picked.as_ref() == Some(&id);
            let exists = book.project_path.is_dir();
            let last = book.notes.iter().map(|n| n.updated).max().unwrap_or(0);
            let (pending, total) = (book.pending(), book.notes.len());
            let pick = id.clone();
            let this = cx.entity();
            let projects = projects.clone();
            let move_menu = Button::new(SharedString::from(format!("notes-orphan-move-{id}")))
                .small()
                .outline()
                .icon(IconName::ArrowRight)
                .label("Move to…")
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu.label("Move these notes into");
                    for (pid, name, path) in &projects {
                        let (this, target) = (this.clone(), (pid.clone(), name.clone(), path.clone()));
                        menu = menu.item(PopupMenuItem::new(name.clone()).on_click(move |_, _, cx| {
                            this.update(cx, |v, cx| v.move_orphan(Some(target.clone()), cx))
                        }));
                    }
                    menu
                });
            let row_id = id.clone();
            v_flex()
                .id(SharedString::from(format!("notes-orphan-{id}")))
                .test_support()
                .mx_4()
                .mb_2()
                .p_3()
                .gap_2()
                .rounded_lg()
                .border_1()
                .border_color(if selected { c.accent } else { c.border })
                .when(selected, |el| el.bg(c.card))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    this.cleanup_selected = Some(pick.clone());
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(IconName::FolderX).small().text_color(c.muted))
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(book.project_name.clone()))
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(c.muted)
                                .child(format!("{pending} to do · {total} in all · last changed {}", ago_long(last))),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(c.muted)
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(book.project_path.display().to_string()),
                        )
                        .child(if exists { pill("folder still there", c.green) } else { pill("folder gone", c.red) }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!("notes-orphan-here-{row_id}")))
                                .small()
                                .outline()
                                .icon(IconName::ArchiveRestore)
                                .label(format!("Move into {}", self.project_name))
                                .tooltip("Alt+M")
                                .on_click(cx.listener({
                                    let id = row_id.clone();
                                    move |this, _, _, cx| {
                                        this.cleanup_selected = Some(id.clone());
                                        this.move_orphan(None, cx)
                                    }
                                })),
                        )
                        .child(move_menu)
                        .child(
                            Button::new(SharedString::from(format!("notes-orphan-copy-{row_id}")))
                                .small()
                                .ghost()
                                .icon(IconName::Copy)
                                .label("Copy as Markdown")
                                .tooltip("Alt+Y")
                                .on_click(cx.listener({
                                    let id = row_id.clone();
                                    move |this, _, _, cx| {
                                        this.cleanup_selected = Some(id.clone());
                                        this.copy_orphan(cx)
                                    }
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new(SharedString::from(format!("notes-orphan-delete-{row_id}")))
                                .small()
                                .ghost()
                                .icon(IconName::Trash)
                                .label("Delete…")
                                .tooltip("Delete")
                                .on_click(cx.listener({
                                    let id = row_id.clone();
                                    move |this, _, window, cx| {
                                        this.cleanup_selected = Some(id.clone());
                                        this.confirm_delete_orphan(window, cx)
                                    }
                                })),
                        ),
                )
                .into_any_element()
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(div().id("notes-orphans").flex_1().min_h_0().overflow_y_scroll().pt_3().children(rows))
            .child(
                div()
                    .flex_none()
                    .px_5()
                    .py_2()
                    .text_xs()
                    .text_color(c.muted)
                    .child("↑↓ pick · Alt+M move here · Alt+Y copy · Delete delete · Escape back"),
            )
            .into_any_element()
    }
}

/// Whether note `number` would be missing from the list under `filter`.
fn number_is_hidden(filter: Filter, number: u64, book: Option<&Notebook>) -> bool {
    book.and_then(|b| b.get(number)).is_some_and(|n| !filter.keeps(n))
}

impl Focusable for NotesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for NotesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Self::colors(cx);
        let orphans = self.orphans(cx).len();
        if self.cleanup && orphans == 0 {
            self.cleanup = false;
        }
        let body = if self.cleanup {
            self.render_cleanup(&c, cx)
        } else {
            let visible = self.visible(cx);
            let current = self.current(cx);
            let detail = match &self.editor {
                Some(editor) => self.render_editor(editor, &c, cx),
                None => self.render_detail(current.clone(), &c, cx),
            };
            h_flex()
                .flex_1()
                .min_h_0()
                .items_start()
                .child(self.render_list(&visible, current.map(|n| n.number), &c, cx))
                .child(detail)
                .into_any_element()
        };
        div()
            .id("notes")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .rounded_t(px(crate::panes::CONTENT_RADIUS))
            .overflow_hidden()
            .bg(c.bg)
            .text_color(c.fg)
            .on_action(cx.listener(|this, _: &NextNote, _, cx| this.select_step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousNote, _, cx| this.select_step(-1, cx)))
            .on_action(cx.listener(|this, _: &AddNote, window, cx| {
                if this.editor.is_none() {
                    this.cleanup = false;
                    this.start_editor(None, window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &EditNote, window, cx| {
                if this.editor.is_none()
                    && let Some(note) = this.current(cx)
                {
                    this.start_editor(Some(&note), window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleDone, _, cx| {
                if this.editor.is_none() && !this.cleanup {
                    this.toggle_done(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &WorkOnNote, _, cx| {
                if this.editor.is_none() && !this.cleanup {
                    this.work_on(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &GoToSource, _, cx| {
                if !this.cleanup {
                    this.go_to_source(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ShowPending, _, cx| this.set_filter(Filter::Pending, cx)))
            .on_action(cx.listener(|this, _: &ShowDone, _, cx| this.set_filter(Filter::Done, cx)))
            .on_action(cx.listener(|this, _: &ShowAll, _, cx| this.set_filter(Filter::All, cx)))
            .on_action(cx.listener(|this, _: &DeleteNote, window, cx| {
                if this.editor.is_none() {
                    this.confirm_delete(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleCleanup, window, cx| this.toggle_cleanup(window, cx)))
            .on_action(cx.listener(|this, _: &MoveHere, _, cx| {
                if this.cleanup {
                    this.move_orphan(None, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CopyMarkdown, _, cx| this.copy_orphan(cx)))
            .on_action(cx.listener(
                |this, _: &Escape, window, cx| {
                    if this.cleanup { this.toggle_cleanup(window, cx) } else { cx.propagate() }
                },
            ))
            .child(self.render_header(orphans, &c, cx))
            .child(body)
    }
}

struct Colors {
    fg: Hsla,
    muted: Hsla,
    border: Hsla,
    card: Hsla,
    bg: Hsla,
    accent: Hsla,
    green: Hsla,
    red: Hsla,
    yellow: Hsla,
    purple: Hsla,
    blue: Hsla,
}

fn kind_look(kind: Kind, c: &Colors) -> (IconName, Hsla) {
    match kind {
        Kind::Deferred => (IconName::Clock, c.blue),
        Kind::Bug => (IconName::Bug, c.red),
        Kind::Idea => (IconName::Lightbulb, c.yellow),
        Kind::Question => (IconName::CircleQuestionMark, c.purple),
    }
}

pub(crate) fn pill(text: &str, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .text_xs()
        .bg(color.opacity(0.15))
        .text_color(color)
        .child(text.to_string())
}

pub(crate) fn pill_icon(icon: IconName, text: &str, color: Hsla) -> Div {
    h_flex()
        .flex_none()
        .gap_1()
        .px_2()
        .py_0p5()
        .rounded_full()
        .text_xs()
        .bg(color.opacity(0.15))
        .text_color(color)
        .child(Icon::new(icon).xsmall())
        .child(text.to_string())
}

/// How long ago a time (seconds since the epoch) was, briefly: "now", "5m", "3h", "2d", "Mar 4".
pub(crate) fn ago(at: u64) -> String {
    let secs = crate::store::now().saturating_sub(at);
    match secs {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..2_592_000 => format!("{}d", secs / 86_400),
        _ => date(at),
    }
}

/// How long ago, in words: "just now", "5 minutes ago", "on Mar 4".
pub(crate) fn ago_long(at: u64) -> String {
    let secs = crate::store::now().saturating_sub(at);
    let plural = |n: u64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match secs {
        0..60 => "just now".into(),
        60..3_600 => plural(secs / 60, "minute"),
        3_600..86_400 => plural(secs / 3_600, "hour"),
        86_400..2_592_000 => plural(secs / 86_400, "day"),
        _ => format!("on {}", date(at)),
    }
}

fn date(at: u64) -> String {
    chrono::DateTime::from_timestamp(at as i64, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%b %-d, %Y").to_string())
        .unwrap_or_default()
}
