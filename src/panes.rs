//! A project's main area, laid out like Vmux: tabs along the top bar, each with its own panes
//! split right or down. A pane shows the project's agent, a terminal, the review of the
//! project's changes, or its GitHub page, and a tab takes its title from its focused pane.
//! There is one agent per project, shown in one pane: picking a thread in the sidebar changes
//! what it shows, and leaves every other pane where it is. Splitting a pane stays in its tab;
//! closing a tab's last pane closes the tab. Shortcuts match Vmux's.

use std::path::PathBuf;
use std::rc::Rc;

use alacritty_terminal::tty::Shell;
use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, button::Button};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::agent_slot::AgentSlot;
use crate::changes::ReviewView;
use crate::github_view::GitHubView;
use crate::notes_view::NotesView;
use crate::store::{AreaLayout, SplitDirection, SplitLayout, TabState};
use crate::terminal::{TerminalExited, TerminalView};
use crate::thread::{Status, ThreadView};

pub const CONTEXT: &str = "Panes";
const MIN_RATIO: f32 = 0.15;
/// How wide a divider between panes is to grab (it draws a 1px line in the middle).
const DIVIDER: f32 = 5.0;

/// A divider being dragged: the path (first = false, second = true) from the root to its split.
#[derive(Clone, Debug, PartialEq)]
struct DividerDrag {
    path: Rc<[bool]>,
}

/// Nothing follows the pointer while a divider is dragged; the panes move instead.
struct NoDragImage;

impl Render for NoDragImage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
const RESIZE_STEP: f32 = 0.05;
/// How tall the bar over each pane of a split tab is: the grip to drag the pane by.
const GRIP_BAR: f32 = 8.0;
/// How far in from a pane's left and right sides a dragged pane lands on that side; between
/// them, it lands above or below.
const SIDE_ZONE: f32 = 0.3;

/// A pane being dragged by its grip, to another pane's edge.
#[derive(Clone, Debug)]
struct PaneDrag {
    pane: usize,
    title: SharedString,
}

/// What follows the pointer while a pane is dragged: its title.
struct PaneDragImage {
    title: SharedString,
}

impl Render for PaneDragImage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.primary)
            .bg(theme.popover)
            .text_xs()
            .text_color(theme.popover_foreground)
            .shadow_md()
            .child(self.title.clone())
    }
}

/// The side of a pane another pane is dropped on.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn direction(self) -> SplitDirection {
        match self {
            Edge::Left | Edge::Right => SplitDirection::Right,
            Edge::Top | Edge::Bottom => SplitDirection::Down,
        }
    }

    /// Whether the dropped pane goes first: left of, or above, the one it's dropped on.
    fn before(self) -> bool {
        matches!(self, Edge::Left | Edge::Top)
    }

    /// The side of `bounds` a pane let go at `at` lands on, if `at` is within them: the left
    /// and right sides take it beside, the middle above or below.
    fn at(bounds: Bounds<Pixels>, at: Point<Pixels>) -> Option<Edge> {
        if !bounds.contains(&at) {
            return None;
        }
        let x = (at.x - bounds.left()) / bounds.size.width;
        let y = (at.y - bounds.top()) / bounds.size.height;
        Some(if x < SIDE_ZONE {
            Edge::Left
        } else if x > 1.0 - SIDE_ZONE {
            Edge::Right
        } else if y < 0.5 {
            Edge::Top
        } else {
            Edge::Bottom
        })
    }
}

gpui_kit::actions!(
    panes,
    [
        NewTab,
        CloseTab,
        SplitRight,
        SplitDown,
        FocusNextPane,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        GrowPane,
        ShrinkPane,
        NextTab,
        PreviousTab,
        MoveTabRight,
        MoveTabLeft
    ]
);

/// What a pane shows.
#[derive(Clone)]
pub enum View {
    /// The project's agent: whichever thread is picked, with its status column.
    Agent(Entity<AgentSlot>),
    Terminal(Entity<TerminalView>),
    Review(Entity<ReviewView>),
    GitHub(Entity<GitHubView>),
    Notes(Entity<NotesView>),
}

impl View {
    fn entity_id(&self) -> EntityId {
        match self {
            View::Agent(agent) => agent.entity_id(),
            View::Terminal(term) => term.entity_id(),
            View::Review(review) => review.entity_id(),
            View::GitHub(github) => github.entity_id(),
            View::Notes(notes) => notes.entity_id(),
        }
    }

    fn state(&self, project_dir: &std::path::Path, cx: &App) -> TabState {
        match self {
            View::Agent(_) => TabState::Agent,
            View::Terminal(term) => {
                TabState::Terminal { cwd: term.read(cx).cwd().unwrap_or_else(|| project_dir.to_path_buf()) }
            }
            View::Review(_) => TabState::Review,
            View::GitHub(_) => TabState::GitHub,
            View::Notes(_) => TabState::Notes,
        }
    }

    /// The thread the agent shows.
    fn thread_id(&self, cx: &App) -> Option<String> {
        match self {
            View::Agent(agent) => agent.read(cx).thread_id().map(str::to_owned),
            _ => None,
        }
    }
}

struct Pane {
    id: usize,
    view: View,
}

enum Node {
    Pane(Pane),
    Split { direction: SplitDirection, ratio: f32, first: Box<Node>, second: Box<Node> },
}

/// A pane's place in the tab, as fractions of its width and height.
#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Node {
    fn pane_ids(&self, out: &mut Vec<usize>) {
        match self {
            Node::Pane(pane) => out.push(pane.id),
            Node::Split { first, second, .. } => {
                first.pane_ids(out);
                second.pane_ids(out);
            }
        }
    }

    fn pane(&self, id: usize) -> Option<&Pane> {
        match self {
            Node::Pane(pane) => (pane.id == id).then_some(pane),
            Node::Split { first, second, .. } => first.pane(id).or_else(|| second.pane(id)),
        }
    }

    /// The first pane whose view matches.
    fn find(&self, matches: &dyn Fn(&View) -> bool) -> Option<&Pane> {
        match self {
            Node::Pane(pane) => matches(&pane.view).then_some(pane),
            Node::Split { first, second, .. } => first.find(matches).or_else(|| second.find(matches)),
        }
    }

    fn views(&self, out: &mut Vec<View>) {
        match self {
            Node::Pane(pane) => out.push(pane.view.clone()),
            Node::Split { first, second, .. } => {
                first.views(out);
                second.views(out);
            }
        }
    }

    fn contains(&self, id: usize) -> bool {
        self.pane(id).is_some()
    }

    fn rects(&self, rect: Rect, out: &mut Vec<(usize, Rect)>) {
        match self {
            Node::Pane(pane) => out.push((pane.id, rect)),
            Node::Split { direction, ratio, first, second } => {
                let (a, b) = match direction {
                    SplitDirection::Right => (
                        Rect { w: rect.w * ratio, ..rect },
                        Rect { x: rect.x + rect.w * ratio, w: rect.w * (1.0 - ratio), ..rect },
                    ),
                    SplitDirection::Down => (
                        Rect { h: rect.h * ratio, ..rect },
                        Rect { y: rect.y + rect.h * ratio, h: rect.h * (1.0 - ratio), ..rect },
                    ),
                };
                first.rects(a, out);
                second.rects(b, out);
            }
        }
    }

    /// Replace pane `id` with a split of it and `new`, with `new` first (left or above) when
    /// `before`. `new` comes back if there's no pane `id`.
    fn split(self, id: usize, direction: SplitDirection, new: Pane, before: bool) -> (Node, Option<Pane>) {
        match self {
            Node::Pane(pane) if pane.id == id => {
                let (pane, new) = (Box::new(Node::Pane(pane)), Box::new(Node::Pane(new)));
                let (first, second) = if before { (new, pane) } else { (pane, new) };
                (Node::Split { direction, ratio: 0.5, first, second }, None)
            }
            Node::Pane(pane) => (Node::Pane(pane), Some(new)),
            Node::Split { direction: d, ratio, first, second } => {
                let (first, rest) = first.split(id, direction, new, before);
                let (second, rest) = match rest {
                    Some(new) => second.split(id, direction, new, before),
                    None => (*second, None),
                };
                (Node::Split { direction: d, ratio, first: Box::new(first), second: Box::new(second) }, rest)
            }
        }
    }

    /// Remove pane `id`; its sibling takes the split's place.
    fn remove(self, id: usize) -> Option<Node> {
        match self {
            Node::Pane(pane) => (pane.id != id).then_some(Node::Pane(pane)),
            Node::Split { direction, ratio, first, second } => match (first.remove(id), second.remove(id)) {
                (Some(first), Some(second)) => {
                    Some(Node::Split { direction, ratio, first: Box::new(first), second: Box::new(second) })
                }
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// Take pane `id` out, as `remove` does, and hand it back.
    fn take(self, id: usize) -> (Option<Node>, Option<Pane>) {
        match self {
            Node::Pane(pane) if pane.id == id => (None, Some(pane)),
            Node::Pane(pane) => (Some(Node::Pane(pane)), None),
            Node::Split { direction, ratio, first, second } => {
                let ((first, a), (second, b)) = (first.take(id), second.take(id));
                let node = match (first, second) {
                    (Some(first), Some(second)) => {
                        Some(Node::Split { direction, ratio, first: Box::new(first), second: Box::new(second) })
                    }
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                };
                (node, a.or(b))
            }
        }
    }

    /// Move pane `id` to `edge` of pane `target`, where it takes half of `target`'s place.
    /// Unchanged (and false) if either is missing or they're the same pane.
    fn move_pane(self, id: usize, target: usize, edge: Edge) -> (Node, bool) {
        if id == target || !self.contains(id) || !self.contains(target) {
            return (self, false);
        }
        // `target` is still there once `id` is out, so there's a root left to put it back in.
        let (Some(root), Some(pane)) = self.take(id) else { unreachable!("both panes were found") };
        let (root, rest) = root.split(target, edge.direction(), pane, edge.before());
        debug_assert!(rest.is_none());
        (root, true)
    }

    /// Set the ratio of the split at `path`, kept within the limits. False if there's none.
    fn set_ratio(&mut self, path: &[bool], value: f32) -> bool {
        match (self, path.split_first()) {
            (Node::Split { ratio, .. }, None) => {
                *ratio = value.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                true
            }
            (Node::Split { first, second, .. }, Some((&go_second, rest))) => {
                if go_second {
                    second.set_ratio(rest, value)
                } else {
                    first.set_ratio(rest, value)
                }
            }
            (Node::Pane(_), _) => false,
        }
    }

    /// Grow (positive) or shrink the pane `id` within its closest split.
    fn resize(&mut self, id: usize, delta: f32) -> bool {
        let Node::Split { ratio, first, second, .. } = self else { return false };
        if first.resize(id, delta) || second.resize(id, delta) {
            return true;
        }
        if first.contains(id) {
            *ratio = (*ratio + delta).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
            true
        } else if second.contains(id) {
            *ratio = (*ratio - delta).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
            true
        } else {
            false
        }
    }

    fn snapshot(&self, project_dir: &std::path::Path, cx: &App) -> SplitLayout {
        match self {
            Node::Pane(pane) => SplitLayout::Pane { view: pane.view.state(project_dir, cx) },
            Node::Split { direction, ratio, first, second } => SplitLayout::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(first.snapshot(project_dir, cx)),
                second: Box::new(second.snapshot(project_dir, cx)),
            },
        }
    }
}

/// A tab in the top bar: its own panes, one of them focused, which names the tab.
struct Tab {
    id: usize,
    root: Node,
    focused: usize,
}

impl Tab {
    fn new(id: usize, root: Node) -> Self {
        let mut ids = Vec::new();
        root.pane_ids(&mut ids);
        Self { id, focused: ids.first().copied().unwrap_or(0), root }
    }

    fn pane_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        self.root.pane_ids(&mut ids);
        ids
    }

    /// What the focused pane shows.
    fn view(&self) -> Option<&View> {
        self.root.pane(self.focused).map(|p| &p.view)
    }
}

/// Which of the project's one-of-a-kind views a restored layout has placed already.
#[derive(Default)]
struct Placed {
    agent: bool,
    review: bool,
    github: bool,
    notes: bool,
}

/// Something the workspace acts on.
pub enum PaneEvent {
    /// Tabs or panes opened, closed or moved; worth saving.
    LayoutChanged,
    /// The agent got focus, showing this thread; the header follows it.
    ThreadFocused(String),
    /// The review closed.
    ReviewClosed,
}

impl EventEmitter<PaneEvent> for Panes {}

/// What a new area starts with. The thread is shown in the agent's tab; with no saved layout,
/// that tab (and a terminal tab, if asked for) is all there is.
pub struct Start {
    pub thread: Option<(String, Entity<ThreadView>)>,
    pub terminal: bool,
}

pub struct Panes {
    tabs: Vec<Tab>,
    /// The tab on show.
    active: usize,
    /// Tab ids in the order they were last shown, most recent last; closing a tab goes back
    /// to the one used before it.
    recent: Vec<usize>,
    /// Ids for tabs and panes alike.
    next_id: usize,
    project_dir: PathBuf,
    /// The project's agent, shown in its one pane.
    agent: Entity<AgentSlot>,
    /// The project's review, shown in at most one pane.
    review: Entity<ReviewView>,
    /// The project's GitHub page, shown in at most one pane.
    github: Entity<GitHubView>,
    /// The project's notes, shown in at most one pane.
    notes: Entity<NotesView>,
    /// The shell to run; the user's login shell when `None` (tests use `/bin/sh`).
    shell: Option<Shell>,
    /// The conversation last reported as focused.
    reported_thread: Option<String>,
    /// The pane being dragged by its grip, while the drag lasts.
    dragging: Option<PaneDrag>,
    /// Where the dragged pane would land if let go now: a pane, and the side of it.
    drop_target: Option<(usize, Edge)>,
    focus: FocusHandle,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Panes {
    /// Restore `layout`, or start with `start` when there's nothing to restore. A layout from
    /// before there was one agent may show several conversations: the first becomes the agent
    /// and the rest are dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_dir: PathBuf,
        layout: Option<&AreaLayout>,
        agent: Entity<AgentSlot>,
        review: Entity<ReviewView>,
        github: Entity<GitHubView>,
        notes: Entity<NotesView>,
        start: Start,
        shell: Option<Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            recent: Vec::new(),
            next_id: 0,
            project_dir,
            agent: agent.clone(),
            review,
            github: github.clone(),
            notes: notes.clone(),
            shell,
            reported_thread: None,
            dragging: None,
            drop_target: None,
            focus: cx.focus_handle(),
            error: None,
            _subscriptions: Vec::new(),
        };
        this._subscriptions.push(cx.observe(&agent, |_, _, cx| cx.notify()));
        this._subscriptions.push(cx.observe(&github, |_, _, cx| cx.notify()));
        this._subscriptions.push(cx.observe(&notes, |_, _, cx| cx.notify()));
        let has_thread = start.thread.is_some();
        if let Some((id, view)) = start.thread {
            agent.update(cx, |a, cx| a.set_thread(&id, view, cx));
        }
        if let Some(layout) = layout {
            let mut placed = Placed::default();
            for tab in &layout.tabs {
                if let Some(root) = this.restore(tab, &mut placed, window, cx) {
                    let id = this.next_id();
                    this.tabs.push(Tab::new(id, root));
                }
            }
            this.active = layout.active.min(this.tabs.len().saturating_sub(1));
        }
        if this.tabs.is_empty() {
            let mut views: Vec<View> = has_thread.then(|| View::Agent(agent)).into_iter().collect();
            if start.terminal || views.is_empty() {
                views.extend(this.spawn(None, window, cx).map(View::Terminal));
            }
            for view in views {
                let root = this.leaf(view);
                let id = this.next_id();
                this.tabs.push(Tab::new(id, root));
            }
            this.active = 0;
        }
        this.touch();
        this
    }

    fn restore(
        &mut self,
        layout: &SplitLayout,
        placed: &mut Placed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Node> {
        match layout {
            SplitLayout::Pane { view } => {
                let once = |seen: &mut bool| !std::mem::replace(seen, true);
                let view = match view {
                    TabState::Agent | TabState::Thread { .. } => {
                        once(&mut placed.agent).then(|| View::Agent(self.agent.clone()))
                    }
                    // A directory that no longer exists falls back to the project folder.
                    TabState::Terminal { cwd } => {
                        self.spawn(Some(cwd.clone()).filter(|d| d.is_dir()), window, cx).map(View::Terminal)
                    }
                    TabState::Review => once(&mut placed.review).then(|| View::Review(self.review.clone())),
                    TabState::GitHub => once(&mut placed.github).then(|| View::GitHub(self.github.clone())),
                    TabState::Notes => once(&mut placed.notes).then(|| View::Notes(self.notes.clone())),
                    TabState::Unknown => None,
                }?;
                Some(self.leaf(view))
            }
            SplitLayout::Split { direction, ratio, first, second } => {
                match (self.restore(first, placed, window, cx), self.restore(second, placed, window, cx)) {
                    (Some(first), Some(second)) => Some(Node::Split {
                        direction: *direction,
                        ratio: ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO),
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    fn next_id(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }

    /// A new pane showing `view`.
    fn leaf(&mut self, view: View) -> Node {
        Node::Pane(Pane { id: self.next_id(), view })
    }

    fn spawn(
        &mut self,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let cwd = cwd.unwrap_or_else(|| self.project_dir.clone());
        match TerminalView::spawn(Some(cwd), self.shell.clone(), window, cx) {
            Ok(term) => {
                self._subscriptions.push(cx.subscribe_in(
                    &term,
                    window,
                    |this, term, _: &TerminalExited, window, cx| {
                        let id = term.entity_id();
                        this.close_entity(id, window, cx);
                    },
                ));
                self._subscriptions.push(cx.observe(&term, |_, _, cx| cx.notify()));
                Some(term)
            }
            Err(err) => {
                self.error = Some(format!("Terminal failed: {err:#}"));
                None
            }
        }
    }

    /// Record the tab on show as the most recently used.
    fn touch(&mut self) {
        if let Some(id) = self.tabs.get(self.active).map(|t| t.id) {
            self.recent.retain(|r| *r != id);
            self.recent.push(id);
        }
    }

    /// What the focused pane of the tab on show shows.
    fn active_view(&self) -> Option<&View> {
        self.tabs.get(self.active)?.view()
    }

    /// The focused pane's terminal, if it shows one.
    pub fn active_terminal(&self) -> Option<Entity<TerminalView>> {
        match self.active_view()? {
            View::Terminal(term) => Some(term.clone()),
            _ => None,
        }
    }

    /// A terminal to type into: the focused one, else the first open one.
    pub fn any_terminal(&self) -> Option<Entity<TerminalView>> {
        self.active_terminal().or_else(|| {
            self.tabs.iter().find_map(|tab| match &tab.root.find(&|v| matches!(v, View::Terminal(_)))?.view {
                View::Terminal(term) => Some(term.clone()),
                _ => None,
            })
        })
    }

    pub fn snapshot(&self, cx: &App) -> Option<AreaLayout> {
        (!self.tabs.is_empty()).then(|| AreaLayout {
            tabs: self.tabs.iter().map(|t| t.root.snapshot(&self.project_dir, cx)).collect(),
            active: self.active,
        })
    }

    /// Where a view is: (tab index, pane id).
    fn find(&self, matches: impl Fn(&View) -> bool) -> Option<(usize, usize)> {
        self.tabs.iter().enumerate().find_map(|(ix, tab)| tab.root.find(&matches).map(|p| (ix, p.id)))
    }

    /// The tabs for the top bar, each floating on the frame and titled after its focused pane;
    /// the one on show stands out by its colour. `favicon` goes on the tab showing the agent.
    pub fn header_tabs(&self, this: Entity<Self>, favicon: Option<AnyElement>, cx: &App) -> Option<AnyElement> {
        if self.tabs.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (fg, muted, shown) = (theme.foreground, theme.muted_foreground, theme.tab_active);
        // Every tab floats on the see-through frame, solid: the one on show in Chrome's colour
        // for a shown tab, the others in the frame's; hovering one lifts it to the shown colour.
        let (tab_bg, tab_fg) = (theme.tab, theme.tab_foreground);
        let mut favicon = favicon;
        let tabs = self.tabs.iter().enumerate().map(|(ix, tab)| {
            let view = tab.view();
            let (title, dot) = view.map(|v| view_label(v, cx)).unwrap_or_default();
            let active = ix == self.active;
            let icon = match view {
                Some(View::Agent(_)) => favicon.take(),
                Some(View::Terminal(_)) => {
                    Some(Icon::new(IconName::SquareTerminal).small().text_color(muted).into_any_element())
                }
                Some(View::Review(_)) => {
                    Some(Icon::new(IconName::FileDiff).small().text_color(muted).into_any_element())
                }
                Some(View::GitHub(_)) => Some(Icon::new(IconName::Github).small().text_color(muted).into_any_element()),
                Some(View::Notes(_)) => {
                    Some(Icon::new(IconName::StickyNote).small().text_color(muted).into_any_element())
                }
                None => None,
            };
            let (activate, close) = (this.clone(), this.clone());
            let tab_id = tab.id;
            h_flex()
                .id(SharedString::from(format!("header-tab-{ix}")))
                .test_support()
                .relative()
                .flex_shrink(1.)
                .min_w(px(90.))
                .max_w(px(240.))
                .gap_2()
                .px_3()
                .text_sm()
                .cursor_pointer()
                .h(px(FLOAT_HEIGHT))
                .mb(px(FLOAT_INSET))
                .rounded_lg()
                .map(|el| {
                    if active {
                        el.text_color(fg).bg(shown)
                    } else {
                        el.text_color(tab_fg).bg(tab_bg).hover(|el| el.bg(shown).text_color(fg))
                    }
                })
                .on_click(move |_, window, cx| activate.update(cx, |p, cx| p.select_tab(tab_id, window, cx)))
                .children(icon.map(|icon| div().flex_none().child(icon)))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
                .when_some(dot, |el, color| el.child(div().flex_none().size_1p5().rounded_full().bg(color)))
                .child(
                    div()
                        .id(SharedString::from(format!("header-close-tab-{ix}")))
                        .flex_none()
                        .size(px(18.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(muted)
                        .hover(|el| el.bg(fg.opacity(0.12)).text_color(fg))
                        .child(Icon::new(IconName::X).xsmall())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            close.update(cx, |p, cx| p.close_whole_tab(tab_id, window, cx));
                        }),
                )
        });
        Some(
            h_flex()
                .h_full()
                .min_w_0()
                .flex_shrink(1.)
                .items_end()
                .gap(px(FLOAT_INSET))
                .children(tabs)
                .into_any_element(),
        )
    }

    /// How many panes each tab has, in order.
    #[cfg(test)]
    pub fn shape(&self) -> Vec<usize> {
        self.tabs.iter().map(|t| t.pane_ids().len()).collect()
    }

    /// The tab on show.
    #[cfg(test)]
    pub fn active_tab(&self) -> usize {
        self.active
    }

    #[cfg(test)]
    pub fn focused_pane(&self) -> usize {
        self.tabs.get(self.active).map(|t| t.focused).unwrap_or(0)
    }

    #[cfg(test)]
    pub fn review_is_showing(&self) -> bool {
        matches!(self.active_view(), Some(View::Review(_)))
    }

    /// Whether the agent is on screen: in the tab on show, focused or not.
    pub fn agent_on_show(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| {
            tab.pane_ids().into_iter().any(|id| matches!(tab.root.pane(id).map(|p| &p.view), Some(View::Agent(_))))
        })
    }

    /// The thread showing in the focused pane, if it shows the agent.
    pub fn active_thread_id(&self, cx: &App) -> Option<String> {
        self.active_view().and_then(|v| v.thread_id(cx))
    }

    /// Tell the workspace which thread the agent shows, if that changed.
    fn report_thread(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active_thread_id(cx)
            && self.reported_thread.as_deref() != Some(&id)
        {
            self.reported_thread = Some(id.clone());
            cx.emit(PaneEvent::ThreadFocused(id));
        }
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_active(window, cx);
        cx.emit(PaneEvent::LayoutChanged);
        cx.notify();
    }

    /// Focus the focused pane of the tab on show, and tell the workspace if it's the agent.
    pub fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_view().cloned() {
            Some(View::Agent(agent)) => {
                match agent.read(cx).thread().map(|view| view.read(cx).prompt_focus(cx)) {
                    Some(focus) => window.focus(&focus, cx),
                    None => self.focus.focus(window, cx),
                }
                self.report_thread(cx);
            }
            Some(View::Terminal(term)) => {
                let focus = Focusable::focus_handle(term.read(cx), cx);
                window.focus(&focus, cx);
            }
            Some(View::GitHub(github)) => github.update(cx, |g, cx| g.shown(window, cx)),
            Some(View::Notes(notes)) => notes.update(cx, |n, cx| n.shown(window, cx)),
            Some(View::Review(review)) => review.update(cx, |r, cx| r.focus(window, cx)),
            None => self.focus.focus(window, cx),
        }
    }

    /// The new terminal starts where the focused one is.
    fn focused_cwd(&self, cx: &App) -> Option<PathBuf> {
        self.active_terminal().and_then(|t| t.read(cx).cwd())
    }

    /// Open `view` in a new tab at the end of the top bar, and show it.
    fn push_tab(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.leaf(view);
        let id = self.next_id();
        self.tabs.push(Tab::new(id, root));
        self.active = self.tabs.len() - 1;
        self.touch();
        self.changed(window, cx);
    }

    /// Show tab `ix` with pane `pane` focused.
    fn activate_at(&mut self, (ix, pane): (usize, usize), window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(ix) else { return };
        tab.focused = pane;
        self.active = ix;
        self.touch();
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Show the tab with id `id`, as its panes were.
    fn select_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((ix, tab)) = self.tabs.iter().enumerate().find(|(_, t)| t.id == id) {
            let pane = tab.focused;
            self.activate_at((ix, pane), window, cx);
        }
    }

    /// Show a thread in the agent's pane, in place of the one there, and focus it. If the
    /// agent was closed, it opens again in a tab of its own.
    pub fn show_thread(&mut self, id: &str, view: Entity<ThreadView>, window: &mut Window, cx: &mut Context<Self>) {
        self.agent.update(cx, |a, cx| a.set_thread(id, view, cx));
        match self.find(|v| matches!(v, View::Agent(_))) {
            Some(at) => self.activate_at(at, window, cx),
            None => self.push_tab(View::Agent(self.agent.clone()), window, cx),
        }
    }

    /// Show the review, opening it in a new tab if needed.
    pub fn show_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.find(|v| matches!(v, View::Review(_))) {
            Some(at) => self.activate_at(at, window, cx),
            None => self.push_tab(View::Review(self.review.clone()), window, cx),
        }
    }

    /// Show the GitHub page, opening it in a new tab if needed.
    pub fn show_github(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.find(|v| matches!(v, View::GitHub(_))) {
            Some(at) => self.activate_at(at, window, cx),
            None => self.push_tab(View::GitHub(self.github.clone()), window, cx),
        }
    }

    /// Show the notes, opening them in a new tab if needed.
    pub fn show_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.find(|v| matches!(v, View::Notes(_))) {
            Some(at) => self.activate_at(at, window, cx),
            None => self.push_tab(View::Notes(self.notes.clone()), window, cx),
        }
    }

    #[cfg(test)]
    pub fn notes_are_showing(&self) -> bool {
        matches!(self.active_view(), Some(View::Notes(_)))
    }

    #[cfg(test)]
    pub fn github_is_showing(&self) -> bool {
        matches!(self.active_view(), Some(View::GitHub(_)))
    }

    /// A terminal in a new tab.
    pub fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.focused_cwd(cx);
        if let Some(term) = self.spawn(cwd, window, cx) {
            self.push_tab(View::Terminal(term), window, cx);
        }
    }

    /// A terminal in a new tab, started in the project's folder (not the focused terminal's).
    pub fn new_project_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let term = self.spawn(None, window, cx)?;
        self.push_tab(View::Terminal(term.clone()), window, cx);
        Some(term)
    }

    /// Split the focused pane of the tab on show; the new pane, in the same tab, starts with a
    /// terminal.
    pub fn split(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.focused_cwd(cx);
        let Some(term) = self.spawn(cwd, window, cx) else { return };
        if self.tabs.is_empty() {
            return self.push_tab(View::Terminal(term), window, cx);
        }
        let id = self.next_id();
        let ix = self.active;
        let Tab { id: tab_id, root, focused } = self.tabs.remove(ix);
        let (root, _) = root.split(focused, direction, Pane { id, view: View::Terminal(term) }, false);
        self.tabs.insert(ix, Tab { id: tab_id, root, focused: id });
        self.changed(window, cx);
    }

    /// Stop showing a thread (archived or deleted): the agent empties until another thread is
    /// picked.
    pub fn close_thread(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        if self.agent.read(cx).thread_id() == Some(thread_id) {
            self.agent.update(cx, |a, cx| a.clear(cx));
            if self.reported_thread.as_deref() == Some(thread_id) {
                self.reported_thread = None;
            }
        }
    }

    /// Close the focused pane; with the tab's last pane, the tab goes too.
    pub fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_view().map(View::entity_id) {
            self.close_entity(id, window, cx);
        }
    }

    /// Close a tab and every pane in it (the top bar's ×).
    fn close_whole_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else { return };
        let tab = self.tabs.remove(ix);
        let mut closed = Vec::new();
        tab.root.views(&mut closed);
        self.tab_removed(ix, tab.id);
        self.closed(&closed, cx);
        self.changed(window, cx);
    }

    /// Close the pane showing `id`. A closed agent opens again when a thread is picked; a
    /// closed terminal's shell ends. The pane's split folds, and an emptied tab closes.
    fn close_entity(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ix, pane_id)) = self.find(|v| v.entity_id() == id) else { return };
        let Tab { id: tab_id, root, focused } = self.tabs.remove(ix);
        let order = {
            let mut ids = Vec::new();
            root.pane_ids(&mut ids);
            ids
        };
        let closed = root.pane(pane_id).map(|p| p.view.clone());
        match root.remove(pane_id) {
            Some(root) => {
                let mut tab = Tab { id: tab_id, root, focused };
                if focused == pane_id {
                    // The pane before it takes focus; failing that, the one after.
                    let at = order.iter().position(|p| *p == pane_id).unwrap_or(0);
                    let remaining = tab.pane_ids();
                    tab.focused = remaining[at.saturating_sub(1).min(remaining.len() - 1)];
                }
                self.tabs.insert(ix, tab);
            }
            None => self.tab_removed(ix, tab_id),
        }
        self.closed(closed.as_slice(), cx);
        self.changed(window, cx);
    }

    /// After the tab at `ix` (id `gone`) was taken out: forget it, and if it was on show, go
    /// back to the tab used before it, failing that its neighbour.
    fn tab_removed(&mut self, ix: usize, gone: usize) {
        self.recent.retain(|r| *r != gone);
        if self.active == ix {
            self.active = self
                .recent
                .iter()
                .rev()
                .find_map(|r| self.tabs.iter().position(|t| t.id == *r))
                .unwrap_or(ix.min(self.tabs.len().saturating_sub(1)));
            self.touch();
        } else if self.active > ix {
            self.active -= 1;
        }
    }

    /// Views that just closed: the agent is reported again when it comes back, and the
    /// workspace hears when the review is gone.
    fn closed(&mut self, views: &[View], cx: &mut Context<Self>) {
        if views.iter().any(|v| matches!(v, View::Agent(_))) {
            self.reported_thread = None;
        }
        if views.iter().any(|v| matches!(v, View::Review(_))) && self.find(|v| matches!(v, View::Review(_))).is_none() {
            cx.emit(PaneEvent::ReviewClosed);
        }
    }

    pub fn focus_next_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let ids = tab.pane_ids();
        if let Some(ix) = ids.iter().position(|id| *id == tab.focused) {
            tab.focused = ids[(ix + 1) % ids.len()];
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Move focus to the nearest pane of the tab in a direction (dx, dy are -1, 0 or 1).
    pub fn focus_direction(&mut self, dx: f32, dy: f32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let mut rects = Vec::new();
        tab.root.rects(Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, &mut rects);
        let Some((_, from)) = rects.iter().find(|(id, _)| *id == tab.focused).copied() else { return };
        let center = |r: &Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (fx, fy) = center(&from);
        let best = rects
            .iter()
            .filter(|(id, r)| {
                *id != tab.focused
                    && match (dx as i32, dy as i32) {
                        (1, _) => r.x >= from.x + from.w - 1e-4,
                        (-1, _) => r.x + r.w <= from.x + 1e-4,
                        (_, 1) => r.y >= from.y + from.h - 1e-4,
                        _ => r.y + r.h <= from.y + 1e-4,
                    }
            })
            .min_by(|(_, a), (_, b)| {
                let dist = |r: &Rect| {
                    let (x, y) = center(r);
                    (x - fx).powi(2) + (y - fy).powi(2)
                };
                dist(a).total_cmp(&dist(b))
            });
        if let Some((id, _)) = best {
            tab.focused = *id;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    pub fn resize(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.get_mut(self.active).is_some_and(|tab| tab.root.resize(tab.focused, delta)) {
            self.changed(window, cx);
        }
    }

    /// Show the next (or, with a negative step, previous) tab.
    pub fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            let len = self.tabs.len() as isize;
            self.active = (self.active as isize + step).rem_euclid(len) as usize;
            self.touch();
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Move the tab on show one place right (or, with a negative step, left), as Chrome's
    /// Ctrl+Shift+PageDown/PageUp do; it stays put at either end.
    pub fn move_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let to = self.active as isize + step;
        if self.active < self.tabs.len() && (0..self.tabs.len() as isize).contains(&to) {
            let tab = self.tabs.remove(self.active);
            self.active = to as usize;
            self.tabs.insert(self.active, tab);
            self.changed(window, cx);
        }
    }

    /// Move pane `id` of the tab on show to `edge` of pane `target`, and focus it there.
    fn move_pane(&mut self, id: usize, target: usize, edge: Edge, window: &mut Window, cx: &mut Context<Self>) {
        (self.dragging, self.drop_target) = (None, None);
        let ix = self.active;
        if ix >= self.tabs.len() {
            return;
        }
        let Tab { id: tab_id, root, focused } = self.tabs.remove(ix);
        let (root, moved) = root.move_pane(id, target, edge);
        self.tabs.insert(ix, Tab { id: tab_id, root, focused: if moved { id } else { focused } });
        if moved {
            self.changed(window, cx);
        } else {
            cx.notify();
        }
    }

    // ---------- rendering ----------

    /// Drag a divider: the split takes the pointer's position within it as its ratio.
    fn drag_divider(&mut self, path: &[bool], ratio: f32, cx: &mut Context<Self>) {
        if self.tabs.get_mut(self.active).is_some_and(|tab| tab.root.set_ratio(path, ratio)) {
            cx.notify();
        }
    }

    /// `outer` is which of the content's top corners the node reaches, where the panes round theirs.
    fn render_node(&self, node: &Node, path: Vec<bool>, outer: Corners, cx: &mut Context<Self>) -> AnyElement {
        let child_path = |second: bool| path.iter().copied().chain([second]).collect::<Vec<_>>();
        // A pane being dragged is out of the layout until it's let go, as if already taken out:
        // its sibling has its place, and the shadow shows exactly where it will land.
        let dragged = self.dragging.as_ref().filter(|_| cx.has_active_drag()).map(|d| d.pane);
        let is_dragged = |node: &Node| matches!(node, Node::Pane(pane) if Some(pane.id) == dragged);
        match node {
            Node::Pane(pane) => self.render_pane(pane, outer, cx),
            Node::Split { first, second, .. } if is_dragged(first) => {
                self.render_node(second, child_path(true), outer, cx)
            }
            Node::Split { first, second, .. } if is_dragged(second) => {
                self.render_node(first, child_path(false), outer, cx)
            }
            Node::Split { direction, ratio, first, second } => {
                let (border, accent) = (cx.theme().border, cx.theme().primary);
                let across = *direction == SplitDirection::Right;
                let (a, b) = (
                    self.render_node(first, child_path(false), outer.first(across), cx),
                    self.render_node(second, child_path(true), outer.second(across), cx),
                );
                let path: Rc<[bool]> = path.into();
                let key = path.iter().map(|b| if *b { '1' } else { '0' }).collect::<String>();
                // A strip to grab between the two sides, with a 1px line down its middle. It lies
                // over their edges rather than between them, so each side's background (a
                // terminal's dark one, say) runs right up to the line.
                let divider = div()
                    .id(SharedString::from(format!("divider-{key}")))
                    .debug_selector(|| format!("divider-{key}"))
                    .group("divider")
                    .flex_none()
                    .flex()
                    .justify_center()
                    .items_center()
                    .absolute()
                    .map(|el| {
                        if across {
                            let el = el.top_0().bottom_0().left(relative(*ratio)).ml(px(-DIVIDER / 2.));
                            el.w(px(DIVIDER)).cursor_col_resize()
                        } else {
                            let el = el.left_0().right_0().top(relative(*ratio)).mt(px(-DIVIDER / 2.));
                            el.h(px(DIVIDER)).cursor_row_resize()
                        }
                    })
                    .child(
                        div()
                            .bg(border)
                            .group_hover("divider", |el| el.bg(accent))
                            .map(|el| if across { el.w(px(1.)).h_full() } else { el.h(px(1.)).w_full() }),
                    )
                    .on_drag(DividerDrag { path: path.clone() }, |_, _, _, cx| cx.new(|_| NoDragImage))
                    // A double click puts the divider back in the middle.
                    .on_click(cx.listener({
                        let path = path.clone();
                        move |this, event: &ClickEvent, window, cx| {
                            if event.click_count() >= 2 {
                                this.drag_divider(&path, 0.5, cx);
                                this.changed(window, cx);
                            }
                        }
                    }));
                let container = if across { h_flex() } else { v_flex() };
                container
                    .size_full()
                    .relative()
                    .on_drag_move(cx.listener({
                        let path = path.clone();
                        move |this, event: &DragMoveEvent<DividerDrag>, _, cx| {
                            if event.drag(cx).path != path {
                                return;
                            }
                            let (bounds, at) = (event.bounds, event.event.position);
                            let ratio = if across {
                                (at.x - bounds.left()) / bounds.size.width
                            } else {
                                (at.y - bounds.top()) / bounds.size.height
                            };
                            this.drag_divider(&path, ratio, cx);
                        }
                    }))
                    // Saved once, when the divider is let go.
                    .on_drop(cx.listener(|this, _: &DividerDrag, window, cx| this.changed(window, cx)))
                    .map(|el| {
                        if across {
                            el.child(div().h_full().w(relative(*ratio)).min_w_0().child(a))
                                .child(div().h_full().flex_1().min_w_0().child(b))
                        } else {
                            el.child(div().w_full().h(relative(*ratio)).min_h_0().child(a))
                                .child(div().w_full().flex_1().min_h_0().child(b))
                        }
                    })
                    .child(divider)
                    .into_any_element()
            }
        }
    }

    /// The pointer moved over pane `over` (to `edge` of it, or off it) while pane `from` is
    /// dragged: the shadow follows.
    fn drag_over(&mut self, from: usize, over: usize, edge: Option<Edge>, cx: &mut Context<Self>) {
        let target = match edge {
            Some(edge) => (from != over).then_some((over, edge)),
            // Off this pane: only its own shadow goes; another pane may have taken over.
            None if self.drop_target.is_some_and(|(pane, _)| pane == over) => None,
            None => return,
        };
        if self.drop_target != target {
            self.drop_target = target;
            cx.notify();
        }
    }

    fn render_pane(&self, pane: &Pane, outer: Corners, cx: &mut Context<Self>) -> AnyElement {
        let (accent, border) = (cx.theme().primary, cx.theme().border);
        let tab = self.tabs.get(self.active);
        let split = tab.is_some_and(|t| matches!(t.root, Node::Split { .. }));
        let focused = tab.is_some_and(|t| t.focused == pane.id);
        // A drag that ended outside the panes leaves `dragging` behind; it's over all the same.
        let dragging = self.dragging.as_ref().filter(|_| cx.has_active_drag());
        let pane_id = pane.id;
        let content = match &pane.view {
            View::Agent(agent) => agent.clone().into_any_element(),
            View::Terminal(term) => term.clone().into_any_element(),
            View::Review(review) => review.clone().into_any_element(),
            View::GitHub(github) => github.clone().into_any_element(),
            View::Notes(notes) => notes.clone().into_any_element(),
        };
        // A terminal's own background fills its whole pane, grip bar and all, so no strip of
        // the content's background shows round it. Other views paint the content's background.
        let fill = matches!(pane.view, View::Terminal(_)).then(|| crate::terminal::background(cx));
        let title: SharedString = view_label(&pane.view, cx).0.into();
        let this = cx.entity().downgrade();
        // The tab is titled after its focused pane; when split, a bar over each pane says which
        // one that is, and is the grip to drag the pane to another pane's side.
        let grip = div()
            .id(SharedString::from(format!("pane-grip-{pane_id}")))
            .debug_selector(|| format!("pane-grip-{pane_id}"))
            .group("pane-grip")
            .h(px(GRIP_BAR))
            .flex_none()
            .relative()
            .flex()
            .justify_center()
            .items_center()
            .cursor_grab()
            .child(
                div()
                    .w(px(32.))
                    .h(px(3.))
                    .mt(px(1.))
                    .rounded_full()
                    .bg(if focused { accent } else { border })
                    .group_hover("pane-grip", |el| el.bg(accent)),
            )
            .on_drag(PaneDrag { pane: pane_id, title }, move |drag, _, _, cx| {
                this.update(cx, |this, cx| {
                    (this.dragging, this.drop_target) = (Some(drag.clone()), None);
                    cx.notify();
                })
                .ok();
                cx.new(|_| PaneDragImage { title: drag.title.clone() })
            });
        let shadow = match (dragging, self.drop_target) {
            (Some(drag), Some((target, edge))) if target == pane_id => Some(drop_shadow(&drag.title, edge, cx)),
            _ => None,
        };
        v_flex()
            .size_full()
            .relative()
            .when_some(fill, |el, fill| {
                el.bg(fill)
                    .when(outer.left, |el| el.rounded_tl(px(CONTENT_RADIUS)))
                    .when(outer.right, |el| el.rounded_tr(px(CONTENT_RADIUS)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if let Some(tab) = this.tabs.get_mut(this.active)
                        && tab.focused != pane_id
                    {
                        tab.focused = pane_id;
                        this.report_thread(cx);
                        cx.notify();
                    }
                }),
            )
            .on_drag_move(cx.listener(move |this, event: &DragMoveEvent<PaneDrag>, _, cx| {
                let edge = Edge::at(event.bounds, event.event.position);
                this.drag_over(event.drag(cx).pane, pane_id, edge, cx);
            }))
            // It lands where its shadow shows, which is where the pointer last moved to.
            .on_drop(cx.listener(move |this, drag: &PaneDrag, window, cx| match this.drop_target {
                Some((target, edge)) if target == pane_id => this.move_pane(drag.pane, target, edge, window, cx),
                _ => {
                    (this.dragging, this.drop_target) = (None, None);
                    cx.notify();
                }
            }))
            .when(split, |el| el.child(grip))
            .child(div().flex_1().min_h_0().child(content))
            .children(shadow)
            .into_any_element()
    }
}

/// Where a dragged pane would land on the pane under it: a shadow of the pane, titled, over the
/// half of that pane it would take.
fn drop_shadow(title: &SharedString, edge: Edge, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let place = div().absolute().p(px(6.));
    let place = match edge {
        Edge::Left => place.top_0().bottom_0().left_0().w(relative(0.5)),
        Edge::Right => place.top_0().bottom_0().right_0().w(relative(0.5)),
        Edge::Top => place.left_0().right_0().top_0().h(relative(0.5)),
        Edge::Bottom => place.left_0().right_0().bottom_0().h(relative(0.5)),
    };
    place
        .debug_selector(|| "drop-shadow".into())
        .child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .rounded_lg()
                .border_2()
                .border_color(theme.primary)
                .bg(theme.primary.opacity(0.18))
                .shadow_lg()
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(theme.popover)
                        .text_sm()
                        .text_color(theme.popover_foreground)
                        .child(title.clone()),
                ),
        )
        .into_any_element()
}

/// A view's title, and the colour of its dot when its thread is working or waiting.
fn view_label(view: &View, cx: &App) -> (String, Option<Hsla>) {
    let theme = cx.theme();
    match view {
        View::Agent(agent) => match agent.read(cx).thread().map(|v| v.read(cx)) {
            Some(t) => {
                let dot = match t.status {
                    Status::Idle => None,
                    Status::Running => Some(theme.warning),
                    Status::AwaitingApproval => Some(theme.primary),
                };
                (t.title.clone().unwrap_or_else(|| "New thread".into()), dot)
            }
            None => ("Agent".into(), None),
        },
        View::Terminal(term) => {
            let t = term.read(cx);
            (tab_title(t.title(), t.cwd()), None)
        }
        View::Review(review) => (review.read(cx).title(cx), None),
        View::GitHub(github) => (github.read(cx).title(cx), None),
        View::Notes(notes) => (notes.read(cx).title(cx), None),
    }
}

/// Which of the content's rounded top corners a node of the split reaches.
#[derive(Clone, Copy)]
struct Corners {
    left: bool,
    right: bool,
}

impl Corners {
    /// The first (left or upper) side's corners: side by side, it loses the right one; stacked,
    /// it keeps both.
    fn first(self, across: bool) -> Self {
        Corners { right: self.right && !across, ..self }
    }

    /// The second (right or lower) side's corners: side by side, it loses the left one; stacked,
    /// it's below the top and has none.
    fn second(self, across: bool) -> Self {
        if across { Corners { left: false, ..self } } else { Corners { left: false, right: false } }
    }
}

/// The content's top corners, where it meets the top bar, like a browser's page. Views that
/// paint their own background round it to match.
pub const CONTENT_RADIUS: f32 = 10.0;

/// How tall the top bar's floating things are (tabs not on show, round buttons), and the gap
/// above and below them, so they sit in the middle of the bar, level with the sidebar's buttons.
pub const FLOAT_HEIGHT: f32 = 30.0;
pub const FLOAT_INSET: f32 = 8.0;

/// A short tab label: a path-like shell title shows its last folder ("~/Projects/TJ" -> "TJ");
/// anything else (a running program's title) is shown as is.
fn tab_title(title: Option<&str>, cwd: Option<PathBuf>) -> String {
    let folder = |path: &str| {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rsplit('/').next() {
            Some(last) if !last.is_empty() => last.to_string(),
            _ => {
                if path.starts_with('~') {
                    "~".into()
                } else {
                    "/".into()
                }
            }
        }
    };
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) if t.starts_with('/') || t.starts_with('~') || t.starts_with("…/") => folder(t),
        Some(t) => t.to_string(),
        None => cwd.as_deref().and_then(|d| d.to_str()).map(folder).unwrap_or_else(|| "Terminal".into()),
    }
}

impl Render for Panes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let body = match self.tabs.get(self.active) {
            Some(tab) => self.render_node(&tab.root, Vec::new(), Corners { left: true, right: true }, cx),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(div().text_sm().text_color(muted).child("Nothing open. Pick a thread in the sidebar."))
                .child(
                    Button::new("open-terminal")
                        .small()
                        .label("New terminal")
                        .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
                )
                .into_any_element(),
        };
        div()
            .id("panes")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_terminal(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| this.close_tab(window, cx)))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| this.split(SplitDirection::Right, window, cx)))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| this.split(SplitDirection::Down, window, cx)))
            .on_action(cx.listener(|this, _: &FocusNextPane, window, cx| this.focus_next_pane(window, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneLeft, window, cx| this.focus_direction(-1.0, 0.0, window, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneRight, window, cx| this.focus_direction(1.0, 0.0, window, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneUp, window, cx| this.focus_direction(0.0, -1.0, window, cx)))
            .on_action(cx.listener(|this, _: &FocusPaneDown, window, cx| this.focus_direction(0.0, 1.0, window, cx)))
            .on_action(cx.listener(|this, _: &GrowPane, window, cx| this.resize(RESIZE_STEP, window, cx)))
            .on_action(cx.listener(|this, _: &ShrinkPane, window, cx| this.resize(-RESIZE_STEP, window, cx)))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &PreviousTab, window, cx| this.cycle_tab(-1, window, cx)))
            .on_action(cx.listener(|this, _: &MoveTabRight, window, cx| this.move_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &MoveTabLeft, window, cx| this.move_tab(-1, window, cx)))
            .when_some(self.error.clone(), |el, error| el.child(div().p_2().text_xs().child(error)))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneEvent, Panes, Start};
    use crate::changes::{ProjectChanges, ReviewView};
    use crate::store::{AreaLayout, PaneLayout, SplitDirection, SplitLayout, TabState};
    use crate::thread::{Backend, ThreadView};
    use alacritty_terminal::tty::Shell;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, bounds, point, px,
        size,
    };
    use std::{cell::RefCell, path::PathBuf, rc::Rc};

    struct Setup {
        window: gpui_kit::AnyWindowHandle,
        panes: Entity<Panes>,
        dir: PathBuf,
        thread: Entity<ThreadView>,
        events: Rc<RefCell<Vec<String>>>,
    }

    /// Panes for a temp project with one known thread ("t1"), running `/bin/sh` terminals.
    fn open(cx: &mut TestAppContext, layout: Option<AreaLayout>, start_thread: bool) -> Setup {
        open_with(cx, layout, start_thread, Shell::new("/bin/sh".into(), vec![]))
    }

    /// Terminals that print nothing (`sleep`), for tests that let the scheduler run: a shell's
    /// prompt would wake the view from the PTY's thread, which the test scheduler rejects.
    fn open_quiet(cx: &mut TestAppContext) -> Setup {
        open_with(cx, None, false, quiet_shell())
    }

    fn quiet_shell() -> Shell {
        Shell::new("sleep".into(), vec!["60".into()])
    }

    fn open_with(cx: &mut TestAppContext, layout: Option<AreaLayout>, start_thread: bool, shell: Shell) -> Setup {
        let dir = std::env::temp_dir().join(format!("tj-panes-{}", crate::store::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::keymap::init(cx);
        });
        let project = dir.clone();
        let events: Rc<RefCell<Vec<String>>> = Rc::default();
        let log = events.clone();
        let (window, panes, thread) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds(point(px(0.), px(0.)), size(px(1200.), px(800.))))),
                ..Default::default()
            };
            let made: Rc<RefCell<Option<Entity<ThreadView>>>> = Rc::default();
            let made_in = made.clone();
            let (window, panes) = gpui_kit::open_window(options, cx, |window, cx| {
                let thread = cx.new(|cx| ThreadView::new(Backend::Scripted(Vec::new()), window, cx));
                let changes = cx.new(|cx| ProjectChanges::new(project.clone(), cx));
                let review = cx.new(|cx| ReviewView::new(changes.clone(), true, cx));
                let store = cx.new(|cx| crate::notes::NotesStore::new(&project.join(".notes-test"), cx));
                let tracked = cx.new(|cx| {
                    crate::prs_view::PrsView::new(
                        store.clone(),
                        gpui_kit::WeakEntity::new_invalid(),
                        "p1".into(),
                        "proj".into(),
                        project.clone(),
                        true,
                        cx,
                    )
                });
                let github = cx.new(|cx| {
                    crate::github_view::GitHubView::new(project.clone(), tracked, None, None, false, window, cx)
                });
                let notes = cx.new(|cx| {
                    crate::notes_view::NotesView::new(
                        store,
                        gpui_kit::WeakEntity::new_invalid(),
                        "p1".into(),
                        "proj".into(),
                        project.clone(),
                        cx,
                    )
                });
                let agent = cx.new(|cx| crate::agent_slot::AgentSlot::new(project.clone(), changes, cx));
                let start = Start { thread: start_thread.then(|| ("t1".to_string(), thread.clone())), terminal: true };
                let panes = cx.new(|cx| {
                    Panes::new(
                        project,
                        layout.as_ref(),
                        agent,
                        review,
                        github,
                        notes,
                        start,
                        Some(shell.clone()),
                        window,
                        cx,
                    )
                });
                cx.subscribe(&panes, move |_, event: &PaneEvent, _| {
                    log.borrow_mut().push(match event {
                        PaneEvent::LayoutChanged => "layout".into(),
                        PaneEvent::ThreadFocused(id) => format!("focus:{id}"),
                        PaneEvent::ReviewClosed => "review-closed".into(),
                    })
                })
                .detach();
                *made_in.borrow_mut() = Some(thread);
                panes
            })
            .unwrap();
            let thread = made.borrow_mut().take().unwrap();
            (window, panes, thread)
        });
        Setup { window, panes, dir, thread, events }
    }

    fn focus_active(panes: &Entity<Panes>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            panes.update(cx, |p, cx| p.focus_active(window, cx));
            window.render_frame(cx);
        });
    }

    #[gpui_kit::test]
    fn a_new_project_starts_with_its_conversation_and_a_terminal(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1], "two tabs of one pane each");
        assert_eq!(s.panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some("t1".into()));
        let layout = s.panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        assert_eq!(layout.tabs[0], SplitLayout::Pane { view: TabState::Agent });
        assert!(matches!(layout.tabs[1], SplitLayout::Pane { view: TabState::Terminal { .. } }));
    }

    #[gpui_kit::test]
    fn picking_another_thread_swaps_the_agent_tab_and_leaves_terminals_alone(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| {
            p.split(SplitDirection::Right, window, cx);
            p.new_terminal(window, cx);
        });
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1, 1]);
        let before = s.panes.read_with(cx, |p, cx| p.snapshot(cx)).map(|l| l.tabs);

        let other = cx.update(|window, cx| cx.new(|cx| ThreadView::new(Backend::Scripted(Vec::new()), window, cx)));
        s.panes.update_in(cx, |p, window, cx| p.show_thread("t2", other, window, cx));
        cx.run_until_parked();
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1, 1], "no tab added for the second thread");
        assert_eq!(s.panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some("t2".into()));
        assert_eq!(
            s.panes.read_with(cx, |p, cx| p.snapshot(cx)).map(|l| l.tabs),
            before,
            "terminals and splits are untouched"
        );
        assert_eq!(s.panes.read_with(cx, |p, _| p.active_tab()), 0, "the agent's tab is on show again");
        assert!(s.events.borrow().iter().any(|e| e == "focus:t2"));

        // Back to the first: still the same one tab.
        let thread = s.thread.clone();
        s.panes.update_in(cx, |p, window, cx| p.show_thread("t1", thread, window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1, 1]);
        assert_eq!(s.panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some("t1".into()));
    }

    #[gpui_kit::test]
    fn a_closed_agent_opens_again_in_a_tab_of_its_own(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.close_tab(window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1], "just the terminal's tab is left");
        let thread = s.thread.clone();
        s.panes.update_in(cx, |p, window, cx| p.show_thread("t1", thread, window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1]);
        assert_eq!(s.panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some("t1".into()));
    }

    #[gpui_kit::test]
    fn showing_a_thread_focuses_its_tab_instead_of_opening_another(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.cycle_tab(1, window, cx));
        assert_eq!(s.panes.read_with(cx, |p, cx| p.active_thread_id(cx)), None, "the terminal tab is showing");
        let thread = s.thread.clone();
        s.panes.update_in(cx, |p, window, cx| p.show_thread("t1", thread, window, cx));
        cx.run_until_parked();
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1], "no third tab");
        assert_eq!(s.panes.read_with(cx, |p, cx| p.active_thread_id(cx)), Some("t1".into()));
        assert!(s.events.borrow().iter().any(|e| e == "focus:t1"));
    }

    #[gpui_kit::test]
    fn splitting_builds_the_layout_and_snapshots_it(cx: &mut TestAppContext) {
        let s = open(cx, None, false);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| {
            p.split(SplitDirection::Right, window, cx);
            p.split(SplitDirection::Down, window, cx);
            p.new_terminal(window, cx);
        });
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![3, 1], "the splits stay in their tab");
        let layout = s.panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        assert_eq!(layout.active, 1, "the new terminal's tab is on show");
        let SplitLayout::Split { direction: SplitDirection::Right, first, second, .. } = &layout.tabs[0] else {
            panic!("expected a right split: {layout:?}");
        };
        let cwd = s.dir.canonicalize().unwrap();
        assert_eq!(**first, SplitLayout::Pane { view: TabState::Terminal { cwd } });
        assert!(matches!(**second, SplitLayout::Split { direction: SplitDirection::Down, .. }));
    }

    use gpui_kit::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};

    /// The panes of the tab on show, in order (left to right, top to bottom), and its root split.
    fn order(panes: &Entity<Panes>, cx: &mut VisualTestContext) -> (Vec<usize>, Option<SplitDirection>) {
        panes.read_with(cx, |p, _| {
            let tab = &p.tabs[p.active];
            let direction = match &tab.root {
                super::Node::Split { direction, .. } => Some(*direction),
                super::Node::Pane(_) => None,
            };
            (tab.pane_ids(), direction)
        })
    }

    /// Drag pane `id` by its grip to `to`, still holding it.
    fn drag_pane_to(id: usize, to: gpui_kit::Point<gpui_kit::Pixels>, cx: &mut VisualTestContext) {
        let selector: &'static str = Box::leak(format!("pane-grip-{id}").into_boxed_str());
        let grip = cx.debug_bounds(selector).expect("a split pane has a grip").center();
        let none = Modifiers::none();
        cx.simulate_mouse_down(grip, MouseButton::Left, none);
        cx.simulate_mouse_move(point(grip.x - px(20.), grip.y + px(20.)), MouseButton::Left, none);
        cx.simulate_mouse_move(to, MouseButton::Left, none);
        cx.run_until_parked();
    }

    /// Drag pane `id` by its grip and let go at `to`.
    fn drag_pane(id: usize, to: gpui_kit::Point<gpui_kit::Pixels>, cx: &mut VisualTestContext) {
        drag_pane_to(id, to, cx);
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    }

    #[test]
    fn where_a_pane_lands_depends_on_where_in_the_other_pane_it_is_let_go() {
        use super::Edge;
        let pane = bounds(point(px(100.), px(100.)), size(px(1000.), px(400.)));
        let at = |x: f32, y: f32| Edge::at(pane, point(px(x), px(y)));
        assert_eq!(at(150., 450.), Some(Edge::Left), "the left side, even low down");
        assert_eq!(at(1050., 150.), Some(Edge::Right), "the right side, even high up");
        assert_eq!(at(600., 150.), Some(Edge::Top));
        assert_eq!(at(600., 450.), Some(Edge::Bottom));
        assert_eq!(at(50., 300.), None, "outside the pane");
    }

    #[gpui_kit::test]
    fn a_shadow_shows_where_a_dragged_pane_will_land(cx: &mut TestAppContext) {
        let s = open_quiet(cx);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.split(SplitDirection::Right, window, cx));
        cx.run_until_parked();
        let (ids, _) = order(&s.panes, cx);
        let [left, right] = ids[..] else { panic!("two panes: {ids:?}") };
        assert!(cx.debug_bounds("drop-shadow").is_none(), "no shadow before a drag");

        // Over the left side of the other pane (the whole tab, while this one's out of it): the
        // shadow covers its left half.
        drag_pane_to(right, point(px(40.), px(400.)), cx);
        assert_eq!(s.panes.read_with(cx, |p, _| p.drop_target), Some((left, super::Edge::Left)));
        let shadow = cx.debug_bounds("drop-shadow").expect("a shadow while over another pane");
        assert!(shadow.left() < px(1.) && (shadow.right() - px(600.)).abs() < px(4.), "left half: {shadow:?}");

        // It follows the pointer: over the top middle, the top half.
        let none = Modifiers::none();
        cx.simulate_mouse_move(point(px(600.), px(100.)), MouseButton::Left, none);
        cx.run_until_parked();
        assert_eq!(s.panes.read_with(cx, |p, _| p.drop_target), Some((left, super::Edge::Top)));
        let shadow = cx.debug_bounds("drop-shadow").expect("a shadow");
        assert!(shadow.top() < px(1.) && (shadow.bottom() - px(400.)).abs() < px(4.), "top half: {shadow:?}");
        cx.simulate_mouse_up(point(px(600.), px(100.)), MouseButton::Left, none);
        cx.run_until_parked();
        assert!(cx.debug_bounds("drop-shadow").is_none(), "the shadow goes when the drag ends");
    }

    #[gpui_kit::test]
    fn dragging_a_pane_by_its_grip_moves_it_to_another_panes_side(cx: &mut TestAppContext) {
        let s = open_quiet(cx);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.split(SplitDirection::Right, window, cx));
        cx.run_until_parked();
        let (before, _) = order(&s.panes, cx);
        let [left, right] = before[..] else { panic!("two panes: {before:?}") };

        // Onto the left side of the left pane: the two swap places.
        s.events.borrow_mut().clear();
        drag_pane(right, point(px(40.), px(400.)), cx);
        assert_eq!(order(&s.panes, cx), (vec![right, left], Some(SplitDirection::Right)));
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), right, "the moved pane has focus");
        assert!(s.events.borrow().iter().any(|e| e == "layout"), "the new layout is saved");
        assert!(s.panes.read_with(cx, |p, _| p.dragging.is_none()));

        // Onto the bottom of the other pane, which has the whole tab while this one's dragged:
        // one above the other.
        cx.run_until_parked();
        drag_pane(right, point(px(600.), px(700.)), cx);
        assert_eq!(order(&s.panes, cx), (vec![left, right], Some(SplitDirection::Down)));
    }

    #[gpui_kit::test]
    fn a_dragged_pane_leaves_the_layout_until_it_is_let_go(cx: &mut TestAppContext) {
        let s = open_quiet(cx);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.split(SplitDirection::Right, window, cx));
        cx.run_until_parked();
        let before = order(&s.panes, cx);
        let [left, right] = before.0[..] else { panic!("two panes: {before:?}") };
        let selector = |id: usize| -> &'static str { Box::leak(format!("pane-grip-{id}").into_boxed_str()) };

        // Picked up: the other pane takes the whole tab, so the shadow over where the dragged
        // one was is the right half of the tab.
        drag_pane_to(right, point(px(1150.), px(400.)), cx);
        assert!(cx.debug_bounds(selector(right)).is_none(), "the dragged pane isn't drawn");
        let grip = cx.debug_bounds(selector(left)).expect("the other pane is");
        assert!(grip.size.width > px(1190.), "and fills the tab: {grip:?}");
        assert!(cx.debug_bounds("divider-").is_none(), "with no divider");
        let shadow = cx.debug_bounds("drop-shadow").expect("a shadow");
        assert!((shadow.left() - px(600.)).abs() < px(4.) && shadow.right() > px(1195.), "right half: {shadow:?}");

        // Let go there: back where it was, drawn again.
        cx.simulate_mouse_up(point(px(1150.), px(400.)), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(order(&s.panes, cx), before);
        assert!(cx.debug_bounds(selector(right)).is_some());
        assert!(cx.debug_bounds("divider-").is_some());
    }

    fn root_ratio(panes: &Entity<Panes>, cx: &mut VisualTestContext) -> f32 {
        match panes.read_with(cx, |p, cx| p.snapshot(cx)) {
            Some(AreaLayout { tabs, .. }) => match tabs[0] {
                SplitLayout::Split { ratio, .. } => ratio,
                ref other => panic!("expected a split: {other:?}"),
            },
            other => panic!("expected a split: {other:?}"),
        }
    }

    #[gpui_kit::test]
    fn dragging_a_divider_resizes_the_split_and_saves_on_release(cx: &mut TestAppContext) {
        let s = open_quiet(cx);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.split(SplitDirection::Right, window, cx));
        cx.run_until_parked();
        let divider = cx.debug_bounds("divider-").expect("the split has a divider");
        assert!((divider.center().x - px(600.)).abs() < px(4.), "it starts in the middle: {divider:?}");

        let (start, y) = (divider.center(), divider.center().y);
        let none = Modifiers::none();
        s.events.borrow_mut().clear();
        cx.simulate_mouse_down(start, MouseButton::Left, none);
        cx.simulate_mouse_move(point(start.x - px(20.), y), MouseButton::Left, none);
        cx.simulate_mouse_move(point(px(300.), y), MouseButton::Left, none);
        let dragged = root_ratio(&s.panes, cx);
        assert!((dragged - 0.25).abs() < 0.01, "ratio follows the pointer: {dragged}");
        assert!(!s.events.borrow().iter().any(|e| e == "layout"), "not saved mid-drag");

        cx.simulate_mouse_move(point(px(20.), y), MouseButton::Left, none);
        cx.simulate_mouse_up(point(px(20.), y), MouseButton::Left, none);
        assert_eq!(root_ratio(&s.panes, cx), super::MIN_RATIO, "it stops at the limit");
        assert!(s.events.borrow().iter().any(|e| e == "layout"), "saved when let go");
    }

    #[gpui_kit::test]
    fn double_clicking_a_divider_centres_it(cx: &mut TestAppContext) {
        let s = open_quiet(cx);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| {
            p.split(SplitDirection::Down, window, cx);
            p.resize(0.2, window, cx);
        });
        assert!((root_ratio(&s.panes, cx) - 0.5).abs() > 0.1);
        cx.run_until_parked();
        let divider = cx.debug_bounds("divider-").expect("divider").center();
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: divider,
            modifiers: Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: divider,
            modifiers: Modifiers::none(),
            click_count: 2,
        });
        assert_eq!(root_ratio(&s.panes, cx), 0.5);
    }

    #[gpui_kit::test]
    fn vmux_shortcuts_split_tab_move_and_close(cx: &mut TestAppContext) {
        let s = open(cx, None, false);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        focus_active(&s.panes, cx);
        let first = s.panes.read_with(cx, |p, _| p.focused_pane());

        cx.update(|window, cx| window.press("ctrl-alt-h", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2], "Ctrl+Alt+H splits right, in the same tab");
        let right = s.panes.read_with(cx, |p, _| p.focused_pane());
        assert_ne!(right, first, "focus moves to the new pane");

        cx.update(|window, cx| window.press("ctrl-t", cx));
        cx.update(|window, cx| window.press("ctrl-w", cx));
        assert_eq!(
            s.panes.read_with(cx, |p, _| p.shape()),
            vec![2],
            "in a terminal, Ctrl+T and Ctrl+W are the shell's"
        );
        cx.update(|window, cx| window.press("ctrl-shift-t", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1], "Ctrl+Shift+T opens a new tab");
        cx.update(|window, cx| window.press("ctrl-pageup", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.active_tab()), 0, "Ctrl+PageUp goes back to the split tab");
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), right, "with the same pane focused");

        cx.update(|window, cx| window.press("ctrl-shift-pageup", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1], "the first tab can't move further left");
        cx.update(|window, cx| window.press("ctrl-shift-pagedown", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 2], "Ctrl+Shift+PageDown moves the tab right");
        assert_eq!(s.panes.read_with(cx, |p, _| p.active_tab()), 1, "and it stays on show");
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), right, "with the same pane focused");
        cx.update(|window, cx| window.press("ctrl-shift-pagedown", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 2], "the last tab can't move further right");
        cx.update(|window, cx| window.press("ctrl-shift-pageup", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1], "Ctrl+Shift+PageUp moves it back left");
        assert_eq!(s.panes.read_with(cx, |p, _| p.active_tab()), 0);

        cx.update(|window, cx| window.press("ctrl-alt-s", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), first, "Ctrl+Alt+S moves to the left pane");
        cx.update(|window, cx| window.press("ctrl-alt-f", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), right, "Ctrl+Alt+F moves back right");
        cx.update(|window, cx| window.press("ctrl-alt-left", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), first, "Ctrl+Alt+Left still moves left");

        cx.update(|window, cx| window.press("ctrl-shift-w", cx));
        assert_eq!(
            s.panes.read_with(cx, |p, _| p.shape()),
            vec![1, 1],
            "Ctrl+Shift+W closes the pane; the split folds"
        );
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), right);
        cx.update(|window, cx| window.press("ctrl-shift-w", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1], "closing a tab's last pane closes the tab");
    }

    #[gpui_kit::test]
    fn shortcuts_work_from_the_conversation_too(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        focus_active(&s.panes, cx);
        cx.update(|window, cx| window.press("ctrl-t", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1, 1], "Ctrl+T from the prompt opens a tab");
        cx.update(|window, cx| window.press("ctrl-pageup", cx));
        cx.update(|window, cx| window.press("ctrl-pageup", cx));
        focus_active(&s.panes, cx);
        cx.update(|window, cx| window.press("ctrl-alt-v", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1, 1], "Ctrl+Alt+V from the prompt splits down");
    }

    #[gpui_kit::test]
    fn a_saved_layout_restores_every_kind_of_pane(cx: &mut TestAppContext) {
        let layout = AreaLayout {
            tabs: vec![
                SplitLayout::Split {
                    direction: SplitDirection::Right,
                    ratio: 0.6,
                    first: Box::new(SplitLayout::Pane { view: TabState::Agent }),
                    second: Box::new(SplitLayout::Pane { view: TabState::Terminal { cwd: "/no/such/dir".into() } }),
                },
                SplitLayout::Pane { view: TabState::Review },
                SplitLayout::Pane { view: TabState::GitHub },
                // Each one-of-a-kind view is shown once.
                SplitLayout::Pane { view: TabState::Review },
                // A tab another build saved is left out.
                SplitLayout::Pane { view: TabState::Unknown },
            ],
            active: 2,
        };
        // Quiet: a shell's prompt, if it came before the test ended, would fail it (see `open_quiet`).
        let s = open_with(cx, Some(layout), false, quiet_shell());
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1, 1]);
        let restored = s.panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        assert_eq!(restored.active, 2);
        let SplitLayout::Split { ratio, first, second, .. } = &restored.tabs[0] else { panic!("{restored:?}") };
        assert_eq!(*ratio, 0.6);
        assert_eq!(**first, SplitLayout::Pane { view: TabState::Agent });
        assert_eq!(
            **second,
            SplitLayout::Pane { view: TabState::Terminal { cwd: s.dir.canonicalize().unwrap() } },
            "a missing folder opens in the project"
        );
    }

    #[test]
    fn a_layout_from_before_keeps_its_splits_in_the_first_tab() {
        // Saved when each pane had tabs, and before there was one agent: two conversations.
        let old = PaneLayout::Split {
            direction: SplitDirection::Right,
            ratio: 0.6,
            first: Box::new(PaneLayout::Pane {
                tabs: vec![
                    TabState::Thread { id: "t1".into() },
                    TabState::Thread { id: "gone".into() },
                    TabState::Review,
                ],
                active: 2,
            }),
            second: Box::new(PaneLayout::Pane { tabs: vec![TabState::GitHub], active: 0 }),
        };
        let area = old.to_area();
        assert_eq!(area.active, 0);
        assert_eq!(
            area.tabs,
            vec![
                SplitLayout::Split {
                    direction: SplitDirection::Right,
                    ratio: 0.6,
                    first: Box::new(SplitLayout::Pane { view: TabState::Review }),
                    second: Box::new(SplitLayout::Pane { view: TabState::GitHub }),
                },
                SplitLayout::Pane { view: TabState::Thread { id: "t1".into() } },
                SplitLayout::Pane { view: TabState::Thread { id: "gone".into() } },
            ],
            "what was on show keeps its splits; what was behind gets a tab each"
        );
    }

    #[gpui_kit::test]
    fn closing_the_review_tab_reports_it(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.show_review(window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1, 1]);
        assert!(s.panes.read_with(cx, |p, _| p.review_is_showing()));
        s.panes.update_in(cx, |p, window, cx| p.close_tab(window, cx));
        cx.run_until_parked();
        assert!(s.events.borrow().iter().any(|e| e == "review-closed"));
    }

    #[test]
    fn tab_titles_show_the_last_folder_of_a_path() {
        use super::tab_title;
        assert_eq!(tab_title(Some("/tmp/work/scratch/tjcopy"), None), "tjcopy");
        assert_eq!(tab_title(Some("~/Projects/TJ"), None), "TJ");
        assert_eq!(tab_title(Some("~"), None), "~");
        assert_eq!(tab_title(Some("/"), None), "/");
        assert_eq!(tab_title(Some("vim main.rs"), None), "vim main.rs");
        assert_eq!(tab_title(None, Some("/home/me/src".into())), "src");
        assert_eq!(tab_title(None, None), "Terminal");
    }

    #[gpui_kit::test]
    fn a_shell_that_exits_closes_its_pane(cx: &mut TestAppContext) {
        let s = open(cx, None, false);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.new_terminal(window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1]);
        let term = s.panes.read_with(cx, |p, _| p.active_terminal().unwrap());
        term.update(cx, |t, _| t.type_text("exit\r"));
        for _ in 0..300 {
            cx.run_until_parked();
            if s.panes.read_with(cx, |p, _| p.shape()) == vec![1] {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1], "its pane, and with it its tab, closes");
    }
}
