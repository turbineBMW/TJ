//! The GitHub tab's Other repos page: the pull requests, merge requests and mailed patches a
//! project sent to other repositories and hosts ([`crate::tracked`]). A project folder with no
//! GitHub remote of its own shows it as the whole tab ([`crate::github_view`]). The list (open, merged, closed or all) is on the
//! left and the picked one on the right: its state, checks and review, the branch and folder it
//! came from, the thread that sent it (with a way back to it), and a note. GitHub and GitLab
//! ones are read again in the background; the rest are set by hand.
//!
//! Claude adds them through the notes server's `pr_track`, and TJ adds the ones `gh pr create`
//! prints; Track a PR adds one by its link.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Escape, Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    spinner::Spinner,
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::notes::{Author, Notebook, NotesStore};
use crate::notes_view::{ago, ago_long, pill_icon};
use crate::tracked::{self, Checks, Forge, PrStatus, Review, TrackedPr};
use crate::workspace::Workspace;

pub const CONTEXT: &str = "Prs";
const LIST_WIDTH: f32 = 340.0;
/// Showing the tab reads again the open ones last read longer ago than this.
const STALE: Duration = Duration::from_secs(120);
/// While TJ runs, the open ones are read again this often.
const POLL: Duration = Duration::from_secs(10 * 60);

gpui_kit::actions!(
    prs,
    [
        NextPr,
        PreviousPr,
        OpenInBrowser,
        TrackPr,
        RefreshPrs,
        EditPrNote,
        GoToThread,
        ShowActive,
        ShowMerged,
        ShowClosed,
        ShowAllPrs,
        UntrackPr,
        CopyPrLink
    ]
);

/// The tab's own keys; they apply while it has focus and a text box doesn't take them.
pub fn bindings() -> Vec<KeyBinding> {
    let cx = Some(CONTEXT);
    vec![
        KeyBinding::new("down", NextPr, cx),
        KeyBinding::new("up", PreviousPr, cx),
        KeyBinding::new("enter", OpenInBrowser, cx),
        KeyBinding::new("alt-n", TrackPr, cx),
        KeyBinding::new("alt-r", RefreshPrs, cx),
        KeyBinding::new("alt-e", EditPrNote, cx),
        KeyBinding::new("alt-t", GoToThread, cx),
        KeyBinding::new("alt-1", ShowActive, cx),
        KeyBinding::new("alt-2", ShowMerged, cx),
        KeyBinding::new("alt-3", ShowClosed, cx),
        KeyBinding::new("alt-4", ShowAllPrs, cx),
        KeyBinding::new("delete", UntrackPr, cx),
        KeyBinding::new("alt-y", CopyPrLink, cx),
    ]
}

pub enum PrsEvent {
    /// Show a thread, at the user's `prompt`th message when known.
    OpenThread { thread: String, prompt: Option<usize> },
}

impl EventEmitter<PrsEvent> for PrsView {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    Active,
    Merged,
    Closed,
    All,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::Active, Filter::Merged, Filter::Closed, Filter::All];

    fn label(self) -> &'static str {
        match self {
            Filter::Active => "Open",
            Filter::Merged => "Merged",
            Filter::Closed => "Closed",
            Filter::All => "All",
        }
    }

    fn keeps(self, pr: &TrackedPr) -> bool {
        match self {
            Filter::Active => pr.status.is_active(),
            Filter::Merged => pr.status == PrStatus::Merged,
            Filter::Closed => pr.status == PrStatus::Closed,
            Filter::All => true,
        }
    }
}

/// The boxes for tracking a PR by its link.
struct Adder {
    url: Entity<InputState>,
    note: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// A PR's note being changed.
struct NoteEditor {
    url: String,
    input: Entity<InputState>,
    _subscription: Subscription,
}

pub struct PrsView {
    store: Entity<NotesStore>,
    /// For thread titles (read only).
    workspace: WeakEntity<Workspace>,
    project_id: String,
    project_name: String,
    project_path: PathBuf,
    filter: Filter,
    selected: Option<String>,
    adder: Option<Adder>,
    note_editor: Option<NoteEditor>,
    /// The ones being read now, by address.
    reading: HashSet<String>,
    /// The project's own GitHub repository (`owner/repo`); its pull requests are on the Pull
    /// requests page, so they're left out here.
    own_repo: Option<String>,
    /// Never read from hosts (tests).
    offline: bool,
    /// What the header says after an action, and whether it went well.
    flash: Option<(bool, String)>,
    list_scroll: ScrollHandle,
    focus: FocusHandle,
    _poll: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PrsView {
    pub fn new(
        store: Entity<NotesStore>,
        workspace: WeakEntity<Workspace>,
        project_id: String,
        project_name: String,
        project_path: PathBuf,
        offline: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        // A PR Claude just tracked is read at once, wherever it was added from.
        let subscriptions = vec![cx.observe(&store, |this: &mut Self, _, cx| {
            this.read_unread(cx);
            cx.notify();
        })];
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |this: &mut Self, cx| this.read_stale(POLL / 2, cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            store,
            workspace,
            project_id,
            project_name,
            project_path,
            filter: Filter::Active,
            selected: None,
            adder: None,
            note_editor: None,
            reading: HashSet::new(),
            own_repo: None,
            offline,
            flash: None,
            list_scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            _poll: Some(poll),
            _subscriptions: subscriptions,
        };
        this.read_unread(cx);
        this
    }

    /// How many it shows that are still open.
    pub fn active(&self, cx: &App) -> usize {
        self.prs(cx).iter().filter(|p| p.status.is_active()).count()
    }

    /// How many it shows.
    pub fn count(&self, cx: &App) -> usize {
        self.prs(cx).len()
    }

    /// The project's own GitHub repository, whose pull requests this page leaves out.
    pub fn set_own_repo(&mut self, repo: Option<String>, cx: &mut Context<Self>) {
        self.own_repo = repo;
        cx.notify();
    }

    /// The page came to the front: focus it, and read again the open ones not read lately.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.adder {
            Some(adder) => adder.url.update(cx, |t, cx| t.focus(window, cx)),
            None => window.focus(&self.focus, cx),
        }
        self.read_stale(STALE, cx);
    }

    #[cfg(test)]
    pub fn selected_url(&self, cx: &App) -> Option<String> {
        self.current(cx).map(|p| p.url)
    }

    #[cfg(test)]
    pub fn is_adding(&self) -> bool {
        self.adder.is_some()
    }

    fn book<'a>(&self, cx: &'a App) -> Option<&'a Notebook> {
        self.store.read(cx).book(&self.project_id)
    }

    /// The tracked ones, but for the project's own repository's.
    fn prs(&self, cx: &App) -> Vec<TrackedPr> {
        let own = |pr: &TrackedPr| {
            pr.forge == Forge::GitHub && self.own_repo.as_ref().is_some_and(|r| r.eq_ignore_ascii_case(&pr.repo))
        };
        self.book(cx).map(|b| b.prs.iter().filter(|p| !own(p)).cloned().collect()).unwrap_or_default()
    }

    /// The ones the filter shows: open first, then by when they last changed.
    fn visible(&self, cx: &App) -> Vec<TrackedPr> {
        let mut prs: Vec<TrackedPr> = self.prs(cx).into_iter().filter(|p| self.filter.keeps(p)).collect();
        let rank = |s: PrStatus| match s {
            PrStatus::Open => 0,
            PrStatus::Draft => 1,
            PrStatus::Merged => 2,
            PrStatus::Closed => 3,
        };
        prs.sort_by(|a, b| {
            rank(a.status)
                .cmp(&rank(b.status))
                .then_with(|| b.changed.unwrap_or(b.added).cmp(&a.changed.unwrap_or(a.added)))
        });
        prs
    }

    /// The picked one, falling back to the first shown.
    fn current(&self, cx: &App) -> Option<TrackedPr> {
        let visible = self.visible(cx);
        visible.iter().find(|p| Some(&p.url) == self.selected.as_ref()).or_else(|| visible.first()).cloned()
    }

    fn select_step(&mut self, step: isize, cx: &mut Context<Self>) {
        self.flash = None;
        let urls: Vec<String> = self.visible(cx).into_iter().map(|p| p.url).collect();
        if urls.is_empty() {
            return;
        }
        let at = self.current(cx).and_then(|c| urls.iter().position(|u| *u == c.url));
        let next = match at {
            Some(at) => (at as isize + step).clamp(0, urls.len() as isize - 1) as usize,
            None => 0,
        };
        self.selected = Some(urls[next].clone());
        self.list_scroll.scroll_to_item(next);
        cx.notify();
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.flash = None;
        self.filter = filter;
        cx.notify();
    }

    /// Change this project's notebook and save it.
    fn edit<R>(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Notebook) -> R) -> Option<R> {
        let (id, name, path) = (self.project_id.clone(), self.project_name.clone(), self.project_path.clone());
        self.store.update(cx, |s, cx| s.edit(&id, &name, &path, change, cx))
    }

    // ---------- reading their state ----------

    /// Read these from their hosts in the background, and save what they say.
    fn read(&mut self, prs: Vec<TrackedPr>, cx: &mut Context<Self>) {
        let prs: Vec<TrackedPr> =
            prs.into_iter().filter(|p| p.forge.refreshes() && !self.reading.contains(&p.url)).collect();
        if prs.is_empty() || self.offline {
            return;
        }
        self.reading.extend(prs.iter().map(|p| p.url.clone()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let results = cx.background_executor().spawn(async move { tracked::fetch_all(prs) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                for (url, _) in &results {
                    this.reading.remove(url);
                }
                this.edit(cx, |book| {
                    for (url, result) in results {
                        let Some(pr) = book.tracked_mut(&url) else { continue };
                        match result {
                            Ok(Some(fresh)) => pr.apply(fresh),
                            Ok(None) => {}
                            Err(err) => pr.failed(format!("{err:#}")),
                        }
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Read the ones never read yet.
    fn read_unread(&mut self, cx: &mut Context<Self>) {
        let unread: Vec<TrackedPr> = self.prs(cx).into_iter().filter(|p| p.checked.is_none()).collect();
        self.read(unread, cx);
    }

    /// Read the open ones last read longer ago than `age`.
    fn read_stale(&mut self, age: Duration, cx: &mut Context<Self>) {
        let now = crate::store::now();
        let stale: Vec<TrackedPr> = self
            .prs(cx)
            .into_iter()
            .filter(|p| p.status.is_active() && p.checked.is_none_or(|at| now.saturating_sub(at) > age.as_secs()))
            .collect();
        self.read(stale, cx);
    }

    /// Read every one the filter shows, now.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.flash = None;
        let shown = self.visible(cx);
        if !shown.iter().any(|p| p.forge.refreshes()) {
            self.flash = Some((true, "Nothing here TJ can read; set these by hand".into()));
            cx.notify();
            return;
        }
        self.read(shown, cx);
    }

    // ---------- acting on them ----------

    fn open_in_browser(&mut self, cx: &mut Context<Self>) {
        if let Some(pr) = self.current(cx) {
            cx.open_url(&pr.url);
        }
    }

    fn copy_link(&mut self, cx: &mut Context<Self>) {
        if let Some(pr) = self.current(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(pr.url.clone()));
            self.flash = Some((true, "Link copied".into()));
            cx.notify();
        }
    }

    fn go_to_thread(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.current(cx).and_then(|p| p.source) else { return };
        if let Some(thread) = source.thread.filter(|t| self.thread_title(t, cx).is_some()) {
            cx.emit(PrsEvent::OpenThread { thread, prompt: source.prompt });
        }
    }

    fn set_status(&mut self, url: String, status: PrStatus, cx: &mut Context<Self>) {
        self.edit(cx, |book| {
            if let Some(pr) = book.tracked_mut(&url) {
                pr.status = status;
            }
        });
    }

    fn untrack(&mut self, cx: &mut Context<Self>) {
        let Some(pr) = self.current(cx) else { return };
        if self.edit(cx, |book| book.untrack(&pr.url)).flatten().is_some() {
            self.flash = Some((true, format!("Stopped tracking {}", pr.reference())));
            self.selected = None;
        }
        cx.notify();
    }

    /// Show the fields for tracking one by its link, the link focused.
    pub fn start_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adder.is_some() {
            return;
        }
        self.flash = None;
        let url = cx.new(|cx| {
            InputState::new(window, cx).placeholder("https://github.com/owner/repo/pull/123, a GitLab MR, a lore link…")
        });
        let note = cx.new(|cx| InputState::new(window, cx).placeholder("What it's for (optional)"));
        let subscriptions = vec![
            cx.subscribe_in(&url, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    match &this.adder {
                        Some(adder) if adder.note.read(cx).value().is_empty() => {
                            adder.note.update(cx, |n, cx| n.focus(window, cx))
                        }
                        _ => this.save_adder(window, cx),
                    }
                }
            }),
            cx.subscribe_in(&note, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.save_adder(window, cx)
                }
            }),
        ];
        url.update(cx, |u, cx| u.focus(window, cx));
        self.adder = Some(Adder { url, note, _subscriptions: subscriptions });
        cx.notify();
    }

    fn cancel_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.adder.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn save_adder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(adder) = &self.adder else { return };
        let url = adder.url.read(cx).value().trim().to_string();
        let note = adder.note.read(cx).value().trim().to_string();
        let Some(mut pr) = TrackedPr::new(&url, Author::User) else {
            self.flash = Some((false, "That isn't a web address (https://…)".into()));
            adder.url.update(cx, |u, cx| u.focus(window, cx));
            cx.notify();
            return;
        };
        pr.note = note;
        let tidy = pr.url.clone();
        let new = self.edit(cx, |book| book.track(pr));
        if new == Some(false) {
            self.flash = Some((true, "Already tracked".into()));
        }
        // Show it, whichever list it lands in.
        self.filter = Filter::All;
        self.selected = Some(tidy);
        self.adder = None;
        window.focus(&self.focus, cx);
        self.read_unread(cx);
        cx.notify();
    }

    fn start_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pr) = self.current(cx) else { return };
        let input =
            cx.new(|cx| InputState::new(window, cx).default_value(pr.note.clone()).placeholder("What it's for"));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save_note(window, cx)
            }
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.note_editor = Some(NoteEditor { url: pr.url, input, _subscription: subscription });
        cx.notify();
    }

    fn save_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.note_editor.take() else { return };
        let note = editor.input.read(cx).value().trim().to_string();
        self.edit(cx, |book| {
            if let Some(pr) = book.tracked_mut(&editor.url) {
                pr.note = note;
            }
        });
        window.focus(&self.focus, cx);
        cx.notify();
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
        }
    }

    fn render_header(&self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self.store.read(cx).error.clone();
        let prs = self.prs(cx);
        let count = |f: Filter| prs.iter().filter(|p| f.keeps(p)).count();
        let summary = if prs.is_empty() {
            "Nothing tracked yet".to_string()
        } else {
            format!(
                "{} open · {} merged · {} closed",
                count(Filter::Active),
                count(Filter::Merged),
                count(Filter::Closed)
            )
        };
        let reading = !self.reading.is_empty();
        h_flex()
            .flex_none()
            .px_4()
            .py_2p5()
            .gap_3()
            .border_b_1()
            .border_color(c.border)
            .child(Icon::new(IconName::GitPullRequest).text_color(c.accent))
            .child(
                v_flex()
                    .min_w_0()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(match self.own_repo {
                        // Under the repository's own header: these are the others.
                        Some(_) => "Pull requests on other repositories".to_string(),
                        None => format!("Pull requests sent from {}", self.project_name),
                    }))
                    .child(div().text_xs().text_color(c.muted).child(summary)),
            )
            .child(div().flex_1())
            .when_some(error, |el, error| {
                el.child(div().text_xs().text_color(c.red).max_w(px(320.)).line_clamp(2).child(error))
            })
            .when_some(self.flash.clone(), |el, (ok, text)| {
                el.child(div().text_xs().text_color(if ok { c.green } else { c.red }).child(text))
            })
            .when(reading, |el| el.child(Spinner::new().small().color(c.muted)))
            .child(
                Button::new("prs-refresh")
                    .small()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .label("Refresh")
                    .tooltip("Read the state of the ones shown again  (Alt+R)")
                    .disabled(reading)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_all(cx))),
            )
            .child(
                Button::new("prs-add")
                    .small()
                    .primary()
                    .icon(IconName::Plus)
                    .label("Track a PR")
                    .tooltip("Alt+N")
                    .disabled(self.adder.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.start_adding(window, cx))),
            )
    }

    fn render_adder(&self, adder: &Adder, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_none()
            .px_4()
            .py_3()
            .gap_2()
            .border_b_1()
            .border_color(c.border)
            .bg(c.card.opacity(0.4))
            .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                this.cancel_adding(window, cx);
                cx.stop_propagation();
            }))
            .child(div().text_xs().text_color(c.muted).child(
                "Its link: a GitHub pull request, a GitLab merge request, a mailing list thread, or any page. Enter adds it, Escape cancels.",
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().relative().flex_1().child(Input::new(&adder.url)).child(crate::caret::overlay(&adder.url)))
                    .child(
                        div().relative().w(px(280.)).child(Input::new(&adder.note)).child(crate::caret::overlay(&adder.note)),
                    )
                    .child(
                        Button::new("prs-add-save")
                            .small()
                            .primary()
                            .label("Track")
                            .on_click(cx.listener(|this, _, window, cx| this.save_adder(window, cx))),
                    )
                    .child(
                        Button::new("prs-add-cancel")
                            .small()
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.cancel_adding(window, cx))),
                    ),
            )
    }

    fn render_list(
        &self,
        visible: &[TrackedPr],
        current: Option<&str>,
        c: &Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let prs = self.prs(cx);
        let filter = self.filter;
        let segments = Filter::ALL.into_iter().map(|f| {
            let on = f == filter;
            let count = prs.iter().filter(|p| f.keeps(p)).count();
            div()
                .id(SharedString::from(format!("prs-filter-{}", f.label())))
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
        let rows = visible.iter().enumerate().map(|(ix, pr)| {
            let selected = current == Some(pr.url.as_str());
            let url = pr.url.clone();
            let (icon, color) = status_look(pr.status, c);
            let finished = !pr.status.is_active();
            let reading = self.reading.contains(&pr.url);
            div()
                .id(SharedString::from(format!("prs-row-{ix}")))
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
                    this.selected = Some(url.clone());
                    this.note_editor = None;
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .gap_2p5()
                        .items_start()
                        .child(div().pt_0p5().child(Icon::new(icon).small().text_color(color)))
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
                                        .when(finished, |el| el.text_color(c.muted))
                                        .child(pr.display_title()),
                                )
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .text_xs()
                                        .text_color(c.muted)
                                        .child(Icon::new(forge_icon(pr.forge)).xsmall().text_color(c.muted))
                                        .child(div().min_w_0().truncate().child(pr.reference()))
                                        .child("·")
                                        .child(ago(pr.changed.unwrap_or(pr.added)))
                                        .child(div().flex_1())
                                        .when(reading, |el| el.child(Spinner::new().xsmall().color(c.muted)))
                                        .when(pr.error.is_some(), |el| {
                                            el.child(Icon::new(IconName::TriangleAlert).xsmall().text_color(c.yellow))
                                        })
                                        .when(pr.status.is_active(), |el| {
                                            el.children(checks_dot(pr.checks, c))
                                                .when(pr.review == Review::Approved, |el| {
                                                    el.child(Icon::new(IconName::Check).xsmall().text_color(c.green))
                                                })
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
                    .id("prs-list")
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
                                    .child(Icon::new(IconName::GitPullRequest).large().text_color(c.muted.opacity(0.5)))
                                    .child(match filter {
                                        Filter::Active => "Nothing open. Claude tracks each PR it opens, on any repository, or press Alt+N to add a link.",
                                        Filter::Merged => "Nothing merged yet",
                                        Filter::Closed => "Nothing closed",
                                        Filter::All => "No pull requests tracked yet",
                                    }),
                            )
                        } else {
                            el.children(rows)
                        }
                    }),
            )
    }

    fn render_detail(&self, pr: Option<TrackedPr>, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let Some(pr) = pr else {
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(c.muted)
                .child(Icon::new(IconName::GitPullRequest).large().text_color(c.muted.opacity(0.5)))
                .child("Pick a pull request, or press Alt+N to track one by its link")
                .into_any_element();
        };
        let (icon, color) = status_look(pr.status, c);
        let source = pr.source.clone().unwrap_or_default();
        let thread = source.thread.as_deref().map(|t| (t.to_string(), self.thread_title(t, cx)));
        let reading = self.reading.contains(&pr.url);
        let mut facts: Vec<(&'static str, AnyElement)> = Vec::new();
        let text = |s: String| div().min_w_0().child(s).into_any_element();
        if pr.status.is_active() && pr.forge.refreshes() {
            let (look, tint) = match pr.checks {
                Checks::None => (IconName::CircleDashed, c.muted),
                Checks::Pending => (IconName::CircleDot, c.yellow),
                Checks::Passing => (IconName::CircleCheck, c.green),
                Checks::Failing => (IconName::CircleX, c.red),
            };
            facts.push((
                "Checks",
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(look).xsmall().text_color(tint))
                    .child(pr.checks.label())
                    .into_any_element(),
            ));
            if pr.review != Review::None {
                let tint = match pr.review {
                    Review::Approved => c.green,
                    Review::ChangesRequested => c.red,
                    _ => c.muted,
                };
                facts.push(("Review", div().text_color(tint).child(pr.review.label()).into_any_element()));
            }
        }
        if let Some(author) = &pr.author {
            facts.push(("Author", text(author.clone())));
        }
        match (&pr.head, &pr.base) {
            (Some(head), Some(base)) => facts.push(("Branch", text(format!("{head} → {base}")))),
            (Some(head), None) => facts.push(("Branch", text(head.clone()))),
            // The branch it was sent from is only its own when it has a folder of its own;
            // otherwise it's the project folder's, which may be another repository.
            _ if pr.folder.is_some() => {
                if let Some(branch) = &source.branch {
                    facts.push(("Branch", text(branch.clone())));
                }
            }
            _ => {}
        }
        if let Some(folder) = &pr.folder {
            facts.push(("Folder", text(crate::workspace::display_path(folder, &crate::store::home_dir()))));
        }
        if let Some(changed) = pr.changed {
            facts.push(("Last activity", text(ago_long(changed))));
        }
        facts.push((
            "Tracked",
            text(format!("by {} {}", if pr.added_by == Author::Agent { "Claude" } else { "you" }, ago_long(pr.added))),
        ));
        if pr.forge.refreshes() {
            facts.push((
                "Read",
                text(match (reading, pr.checked) {
                    (true, _) => "now…".into(),
                    (false, Some(at)) => ago_long(at),
                    (false, None) => "not yet".into(),
                }),
            ));
        }
        let thread_link = thread.map(|(id, title)| match title {
            Some(title) => h_flex()
                .gap_1()
                .child(
                    Button::new("prs-thread")
                        .xsmall()
                        .ghost()
                        .icon(IconName::MessageSquareText)
                        .label(format!("From {title}"))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(PrsEvent::OpenThread { thread: id.clone(), prompt: source.prompt })
                        })),
                )
                .child(div().text_xs().text_color(c.muted).child("Alt+T"))
                .into_any_element(),
            None => div().text_xs().text_color(c.muted).child("From a thread that was deleted").into_any_element(),
        });
        let editing_note = self.note_editor.as_ref().filter(|e| e.url == pr.url).map(|e| e.input.clone());
        let note = match editing_note {
            Some(input) => h_flex()
                .gap_2()
                .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                    this.note_editor = None;
                    window.focus(&this.focus, cx);
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(div().relative().flex_1().child(Input::new(&input)).child(crate::caret::overlay(&input)))
                .child(
                    Button::new("prs-note-save")
                        .small()
                        .primary()
                        .label("Save")
                        .on_click(cx.listener(|this, _, window, cx| this.save_note(window, cx))),
                )
                .into_any_element(),
            None if pr.note.is_empty() => div()
                .id("prs-note")
                .text_sm()
                .text_color(c.muted)
                .cursor_pointer()
                .child("No note. Alt+E adds one.")
                .on_click(cx.listener(|this, _, window, cx| this.start_note(window, cx)))
                .into_any_element(),
            None => div()
                .id("prs-note")
                .p_3()
                .rounded_lg()
                .bg(c.card)
                .text_sm()
                .cursor_pointer()
                .child(pr.note.clone())
                .on_click(cx.listener(|this, _, window, cx| this.start_note(window, cx)))
                .into_any_element(),
        };
        let this = cx.entity();
        let current_status = pr.status;
        let status_url = pr.url.clone();
        let status_menu = Button::new("prs-status")
            .small()
            .outline()
            .icon(IconName::ChevronDown)
            .label("Set status")
            .tooltip(if pr.forge.refreshes() {
                "The host sets this one; the next read replaces what you pick"
            } else {
                "TJ can't read this link's state, so set it here"
            })
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.label("Status");
                for status in PrStatus::ALL {
                    let (this, url) = (this.clone(), status_url.clone());
                    menu = menu.item(
                        PopupMenuItem::new(status.label())
                            .checked(status == current_status)
                            .on_click(move |_, _, cx| this.update(cx, |v, cx| v.set_status(url.clone(), status, cx))),
                    );
                }
                menu
            });
        v_flex()
            .id("prs-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .id("prs-detail-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_5()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(pill_icon(icon, pr.status.label(), color))
                            .child(pill_icon(forge_icon(pr.forge), pr.forge.label(), c.muted))
                            .child(div().text_xs().text_color(c.muted).child(pr.reference())),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(!pr.status.is_active(), |el| el.text_color(c.muted))
                            .child(pr.display_title()),
                    )
                    .child(
                        div()
                            .id("prs-url")
                            .text_xs()
                            .text_color(c.accent)
                            .cursor_pointer()
                            .hover(|el| el.underline())
                            .child(pr.url.clone())
                            .on_click(cx.listener(|this, _, _, cx| this.open_in_browser(cx))),
                    )
                    .when_some(pr.error.clone(), |el, error| {
                        el.child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .p_3()
                                .rounded_lg()
                                .bg(c.yellow.opacity(0.1))
                                .child(Icon::new(IconName::TriangleAlert).small().text_color(c.yellow))
                                .child(div().flex_1().text_sm().child(format!("Couldn't read it: {error}"))),
                        )
                    })
                    .child(v_flex().gap_1p5().text_sm().children(facts.into_iter().map(|(label, value)| {
                        h_flex()
                            .gap_3()
                            .items_start()
                            .child(div().w(px(96.)).flex_none().text_color(c.muted).child(label))
                            .child(value)
                    })))
                    .children(thread_link)
                    .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(c.muted).child("NOTE"))
                    .child(note),
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
                    .child(
                        Button::new("prs-open")
                            .small()
                            .primary()
                            .icon(IconName::ExternalLink)
                            .label("Open")
                            .tooltip("Open it in the browser  (Enter)")
                            .on_click(cx.listener(|this, _, _, cx| this.open_in_browser(cx))),
                    )
                    .when(pr.forge.refreshes(), |el| {
                        let url = pr.clone();
                        el.child(
                            Button::new("prs-read")
                                .small()
                                .outline()
                                .icon(IconName::RefreshCw)
                                .label("Read again")
                                .disabled(reading)
                                .on_click(cx.listener(move |this, _, _, cx| this.read(vec![url.clone()], cx))),
                        )
                    })
                    .child(status_menu)
                    .child(
                        Button::new("prs-edit-note")
                            .small()
                            .outline()
                            .icon(IconName::Pencil)
                            .label("Note")
                            .tooltip("Alt+E")
                            .on_click(cx.listener(|this, _, window, cx| this.start_note(window, cx))),
                    )
                    .child(
                        Button::new("prs-copy")
                            .small()
                            .ghost()
                            .icon(IconName::Copy)
                            .tooltip("Copy the link  (Alt+Y)")
                            .on_click(cx.listener(|this, _, _, cx| this.copy_link(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("prs-untrack")
                            .small()
                            .ghost()
                            .icon(IconName::Trash)
                            .tooltip("Stop tracking it  (Delete)")
                            .on_click(cx.listener(|this, _, _, cx| this.untrack(cx))),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px_5()
                    .pb_2()
                    .text_xs()
                    .text_color(c.muted)
                    .child("↑↓ pick · Alt+1/2/3/4 Open, Merged, Closed, All · Alt+N track · Alt+R refresh"),
            )
            .into_any_element()
    }
}

impl Focusable for PrsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PrsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Self::colors(cx);
        let visible = self.visible(cx);
        let current = self.current(cx);
        let current_url = current.as_ref().map(|p| p.url.clone());
        let adder = self.adder.as_ref().map(|a| self.render_adder(a, &c, cx).into_any_element());
        let list = self.render_list(&visible, current_url.as_deref(), &c, cx).into_any_element();
        let detail = self.render_detail(current, &c, cx);
        let typing = |this: &Self| this.adder.is_some() || this.note_editor.is_some();
        div()
            .id("prs")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(c.bg)
            .text_color(c.fg)
            .on_action(cx.listener(|this, _: &NextPr, _, cx| this.select_step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousPr, _, cx| this.select_step(-1, cx)))
            .on_action(cx.listener(move |this, _: &OpenInBrowser, _, cx| {
                if !typing(this) {
                    this.open_in_browser(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &TrackPr, window, cx| this.start_adding(window, cx)))
            .on_action(cx.listener(|this, _: &RefreshPrs, _, cx| this.refresh_all(cx)))
            .on_action(cx.listener(move |this, _: &EditPrNote, window, cx| {
                if !typing(this) {
                    this.start_note(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &GoToThread, _, cx| this.go_to_thread(cx)))
            .on_action(cx.listener(|this, _: &ShowActive, _, cx| this.set_filter(Filter::Active, cx)))
            .on_action(cx.listener(|this, _: &ShowMerged, _, cx| this.set_filter(Filter::Merged, cx)))
            .on_action(cx.listener(|this, _: &ShowClosed, _, cx| this.set_filter(Filter::Closed, cx)))
            .on_action(cx.listener(|this, _: &ShowAllPrs, _, cx| this.set_filter(Filter::All, cx)))
            .on_action(cx.listener(move |this, _: &UntrackPr, _, cx| {
                if !typing(this) {
                    this.untrack(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CopyPrLink, _, cx| this.copy_link(cx)))
            .child(self.render_header(&c, cx))
            .children(adder)
            .child(h_flex().flex_1().min_h_0().items_start().child(list).child(detail))
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
}

/// The icon and colour for a status, as GitHub shows them.
fn status_look(status: PrStatus, c: &Colors) -> (IconName, Hsla) {
    match status {
        PrStatus::Open => (IconName::GitPullRequest, c.green),
        PrStatus::Draft => (IconName::GitPullRequestDraft, c.muted),
        PrStatus::Merged => (IconName::GitMerge, c.purple),
        PrStatus::Closed => (IconName::GitPullRequestClosed, c.red),
    }
}

fn forge_icon(forge: Forge) -> IconName {
    match forge {
        Forge::GitHub => IconName::Github,
        Forge::GitLab => IconName::GitMerge,
        Forge::MailingList => IconName::Mail,
        Forge::Other => IconName::Link,
    }
}

/// A small dot for the checks of an open one; none when it has no checks.
fn checks_dot(checks: Checks, c: &Colors) -> Option<Div> {
    let color = match checks {
        Checks::None => return None,
        Checks::Pending => c.yellow,
        Checks::Passing => c.green,
        Checks::Failing => c.red,
    };
    Some(div().size(px(7.)).rounded_full().bg(color))
}
