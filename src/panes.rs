//! A project's main area, laid out like Vmux: panes split right or down, each with its own
//! tabs. A tab is a conversation, a terminal, or the review of the project's changes.
//! Closing a pane's last tab folds its split back up. Shortcuts match Vmux's.

use std::path::PathBuf;

use alacritty_terminal::tty::Shell;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, button::Button};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use std::collections::HashMap;

use crate::changes::ReviewView;
use crate::store::{PaneLayout, SplitDirection, TabState};
use crate::terminal::{TerminalExited, TerminalView};
use crate::thread::{Status, ThreadView};

pub const CONTEXT: &str = "Panes";
const MIN_RATIO: f32 = 0.15;
const RESIZE_STEP: f32 = 0.05;

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
        PreviousTab
    ]
);

/// Vmux's pane and tab shortcuts, active anywhere in the main area.
pub fn bindings() -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    vec![
        KeyBinding::new("ctrl-shift-t", NewTab, c),
        KeyBinding::new("ctrl-shift-w", CloseTab, c),
        KeyBinding::new("ctrl-shift-e", SplitRight, c),
        KeyBinding::new("ctrl-shift-o", SplitDown, c),
        KeyBinding::new("ctrl-shift-a", FocusNextPane, c),
        KeyBinding::new("ctrl-alt-left", FocusPaneLeft, c),
        KeyBinding::new("ctrl-alt-right", FocusPaneRight, c),
        KeyBinding::new("ctrl-alt-up", FocusPaneUp, c),
        KeyBinding::new("ctrl-alt-down", FocusPaneDown, c),
        KeyBinding::new("ctrl-shift-=", GrowPane, c),
        KeyBinding::new("ctrl-shift-+", GrowPane, c),
        KeyBinding::new("ctrl-shift--", ShrinkPane, c),
        KeyBinding::new("ctrl-shift-_", ShrinkPane, c),
        KeyBinding::new("ctrl-pagedown", NextTab, c),
        KeyBinding::new("ctrl-pageup", PreviousTab, c),
    ]
}

/// What a tab shows.
#[derive(Clone)]
pub enum Tab {
    Thread { id: String, view: Entity<ThreadView> },
    Terminal(Entity<TerminalView>),
    Review(Entity<ReviewView>),
}

impl Tab {
    fn entity_id(&self) -> EntityId {
        match self {
            Tab::Thread { view, .. } => view.entity_id(),
            Tab::Terminal(term) => term.entity_id(),
            Tab::Review(review) => review.entity_id(),
        }
    }

    fn state(&self, project_dir: &std::path::Path, cx: &App) -> TabState {
        match self {
            Tab::Thread { id, .. } => TabState::Thread { id: id.clone() },
            Tab::Terminal(term) => {
                TabState::Terminal { cwd: term.read(cx).cwd().unwrap_or_else(|| project_dir.to_path_buf()) }
            }
            Tab::Review(_) => TabState::Review,
        }
    }

    fn thread_id(&self) -> Option<&str> {
        match self {
            Tab::Thread { id, .. } => Some(id),
            _ => None,
        }
    }
}

struct Pane {
    id: usize,
    tabs: Vec<Tab>,
    active: usize,
    /// Tabs in the order they were last shown, most recent last; closing a tab goes back to
    /// the one used before it.
    recent: Vec<EntityId>,
}

impl Pane {
    fn new(id: usize, tabs: Vec<Tab>, active: usize) -> Self {
        let mut pane = Self { id, tabs, active, recent: Vec::new() };
        pane.touch();
        pane
    }

    /// Record the visible tab as the most recently used.
    fn touch(&mut self) {
        if let Some(id) = self.tabs.get(self.active).map(Tab::entity_id) {
            self.recent.retain(|r| *r != id);
            self.recent.push(id);
        }
    }

    fn set_active(&mut self, ix: usize) {
        self.active = ix.min(self.tabs.len().saturating_sub(1));
        self.touch();
    }
}

enum Node {
    Pane(Pane),
    Split { direction: SplitDirection, ratio: f32, first: Box<Node>, second: Box<Node> },
}

/// A pane's place in the area, as fractions of its width and height.
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

    fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        match self {
            Node::Pane(pane) => (pane.id == id).then_some(pane),
            Node::Split { first, second, .. } => match first.pane_mut(id) {
                Some(pane) => Some(pane),
                None => second.pane_mut(id),
            },
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

    /// Replace pane `id` with a split of it and `new`.
    fn split(self, id: usize, direction: SplitDirection, new: Pane) -> (Node, Option<Pane>) {
        match self {
            Node::Pane(pane) if pane.id == id => (
                Node::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane)),
                    second: Box::new(Node::Pane(new)),
                },
                None,
            ),
            Node::Pane(pane) => (Node::Pane(pane), Some(new)),
            Node::Split { direction: d, ratio, first, second } => {
                let (first, rest) = first.split(id, direction, new);
                let (second, rest) = match rest {
                    Some(new) => second.split(id, direction, new),
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

    fn snapshot(&self, project_dir: &std::path::Path, cx: &App) -> PaneLayout {
        match self {
            Node::Pane(pane) => PaneLayout::Pane {
                tabs: pane.tabs.iter().map(|t| t.state(project_dir, cx)).collect(),
                active: pane.active,
            },
            Node::Split { direction, ratio, first, second } => PaneLayout::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(first.snapshot(project_dir, cx)),
                second: Box::new(second.snapshot(project_dir, cx)),
            },
        }
    }
}

/// Something the workspace acts on.
pub enum PaneEvent {
    /// Panes or tabs opened, closed or moved; worth saving.
    LayoutChanged,
    /// A conversation tab got focus; the header and overview follow it.
    ThreadFocused(String),
    /// The last review tab closed.
    ReviewClosed,
}

impl EventEmitter<PaneEvent> for Panes {}

/// What a new area starts with, when there's no saved layout.
pub struct Start {
    pub thread: Option<(String, Entity<ThreadView>)>,
    pub terminal: bool,
}

pub struct Panes {
    root: Option<Node>,
    focused: usize,
    next_pane: usize,
    project_dir: PathBuf,
    /// The project's review, shared by any review tab.
    review: Entity<ReviewView>,
    /// The shell to run; the user's login shell when `None` (tests use `/bin/sh`).
    shell: Option<Shell>,
    /// The conversation last reported as focused.
    reported_thread: Option<String>,
    focus: FocusHandle,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl Panes {
    /// Restore `layout` (threads come from `threads`; ones not there are dropped), or start
    /// with `start` when there's nothing to restore.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_dir: PathBuf,
        layout: Option<&PaneLayout>,
        threads: &HashMap<String, Entity<ThreadView>>,
        review: Entity<ReviewView>,
        start: Start,
        shell: Option<Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            root: None,
            focused: 0,
            next_pane: 0,
            project_dir,
            review,
            shell,
            reported_thread: None,
            focus: cx.focus_handle(),
            error: None,
            _subscriptions: Vec::new(),
        };
        this.root = layout.and_then(|layout| this.restore(layout, threads, window, cx));
        if this.root.is_none() {
            let mut tabs: Vec<Tab> = start.thread.into_iter().map(|(id, view)| Tab::Thread { id, view }).collect();
            if start.terminal || tabs.is_empty() {
                tabs.extend(this.spawn(None, window, cx).map(Tab::Terminal));
            }
            if !tabs.is_empty() {
                let id = this.next_id();
                this.root = Some(Node::Pane(Pane::new(id, tabs, 0)));
            }
        }
        this.focused = this.pane_ids().first().copied().unwrap_or(0);
        this
    }

    fn restore(
        &mut self,
        layout: &PaneLayout,
        threads: &HashMap<String, Entity<ThreadView>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Node> {
        match layout {
            PaneLayout::Pane { tabs, active } => {
                let id = self.next_id();
                let tabs: Vec<Tab> = tabs
                    .iter()
                    .filter_map(|tab| match tab {
                        TabState::Thread { id } => {
                            threads.get(id).map(|view| Tab::Thread { id: id.clone(), view: view.clone() })
                        }
                        // A directory that no longer exists falls back to the project folder.
                        TabState::Terminal { cwd } => {
                            self.spawn(Some(cwd.clone()).filter(|d| d.is_dir()), window, cx).map(Tab::Terminal)
                        }
                        TabState::Review => Some(Tab::Review(self.review.clone())),
                    })
                    .collect();
                (!tabs.is_empty()).then(|| {
                    let active = (*active).min(tabs.len() - 1);
                    Node::Pane(Pane::new(id, tabs, active))
                })
            }
            PaneLayout::Split { direction, ratio, first, second } => {
                match (self.restore(first, threads, window, cx), self.restore(second, threads, window, cx)) {
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
        self.next_pane += 1;
        self.next_pane
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

    pub fn pane_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        if let Some(root) = &self.root {
            root.pane_ids(&mut ids);
        }
        ids
    }

    fn active_tab(&self) -> Option<&Tab> {
        let pane = self.root.as_ref()?.pane(self.focused)?;
        pane.tabs.get(pane.active)
    }

    /// The focused pane's terminal, if its visible tab is one.
    pub fn active_terminal(&self) -> Option<Entity<TerminalView>> {
        match self.active_tab()? {
            Tab::Terminal(term) => Some(term.clone()),
            _ => None,
        }
    }

    /// A terminal to type into: the focused one, else the first open one.
    pub fn any_terminal(&self) -> Option<Entity<TerminalView>> {
        self.active_terminal().or_else(|| {
            let root = self.root.as_ref()?;
            self.pane_ids().into_iter().find_map(|id| {
                root.pane(id)?.tabs.iter().find_map(|t| match t {
                    Tab::Terminal(term) => Some(term.clone()),
                    _ => None,
                })
            })
        })
    }

    pub fn snapshot(&self, cx: &App) -> Option<PaneLayout> {
        self.root.as_ref().map(|root| root.snapshot(&self.project_dir, cx))
    }

    /// Where a tab is: (pane id, index).
    fn find(&self, matches: impl Fn(&Tab) -> bool) -> Option<(usize, usize)> {
        let root = self.root.as_ref()?;
        self.pane_ids().into_iter().find_map(|id| root.pane(id)?.tabs.iter().position(&matches).map(|ix| (id, ix)))
    }

    /// The tab count of each pane, in order.
    #[cfg(test)]
    pub fn shape(&self) -> Vec<usize> {
        self.pane_ids().iter().filter_map(|id| self.root.as_ref()?.pane(*id).map(|p| p.tabs.len())).collect()
    }

    #[cfg(test)]
    pub fn focused_pane(&self) -> usize {
        self.focused
    }

    #[cfg(test)]
    pub fn review_is_showing(&self) -> bool {
        matches!(self.active_tab(), Some(Tab::Review(_)))
    }

    #[cfg(test)]
    pub fn active_thread_id(&self) -> Option<String> {
        self.active_tab().and_then(|t| t.thread_id().map(str::to_owned))
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_active(window, cx);
        cx.emit(PaneEvent::LayoutChanged);
        cx.notify();
    }

    /// Focus the focused pane's visible tab, and tell the workspace if it's a conversation.
    pub fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_tab().cloned() {
            Some(Tab::Thread { id, view }) => {
                let focus = view.read(cx).prompt_focus(cx);
                window.focus(&focus, cx);
                if self.reported_thread.as_deref() != Some(&id) {
                    self.reported_thread = Some(id.clone());
                    cx.emit(PaneEvent::ThreadFocused(id));
                }
            }
            Some(Tab::Terminal(term)) => {
                let focus = Focusable::focus_handle(term.read(cx), cx);
                window.focus(&focus, cx);
            }
            Some(Tab::Review(_)) | None => self.focus.focus(window, cx),
        }
    }

    /// The new terminal starts where the focused one is.
    fn focused_cwd(&self, cx: &App) -> Option<PathBuf> {
        self.active_terminal().and_then(|t| t.read(cx).cwd())
    }

    fn push_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        match self.root.as_mut().and_then(|r| r.pane_mut(self.focused)) {
            Some(pane) => {
                pane.tabs.push(tab);
                pane.set_active(pane.tabs.len() - 1);
            }
            None => {
                let id = self.next_id();
                let pane = Pane::new(id, vec![tab], 0);
                self.root = Some(match self.root.take() {
                    Some(root) => root.split(self.focused, SplitDirection::Right, pane).0,
                    None => Node::Pane(pane),
                });
                self.focused = id;
            }
        }
        self.changed(window, cx);
    }

    fn activate_at(&mut self, (pane, ix): (usize, usize), window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.root.as_mut().and_then(|r| r.pane_mut(pane)) {
            p.set_active(ix);
        }
        self.focused = pane;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Show a conversation: focus its tab if open, else open it in the focused pane.
    pub fn show_thread(&mut self, id: &str, view: Entity<ThreadView>, window: &mut Window, cx: &mut Context<Self>) {
        match self.find(|t| t.thread_id() == Some(id)) {
            Some(at) => self.activate_at(at, window, cx),
            None => self.push_tab(Tab::Thread { id: id.to_string(), view }, window, cx),
        }
    }

    /// Show the review tab, opening it in the focused pane if needed.
    pub fn show_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.find(|t| matches!(t, Tab::Review(_))) {
            Some(at) => self.activate_at(at, window, cx),
            None => self.push_tab(Tab::Review(self.review.clone()), window, cx),
        }
    }

    pub fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.focused_cwd(cx);
        if let Some(term) = self.spawn(cwd, window, cx) {
            self.push_tab(Tab::Terminal(term), window, cx);
        }
    }

    /// Split the focused pane; the new pane starts with a terminal.
    pub fn split(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.focused_cwd(cx);
        let Some(term) = self.spawn(cwd, window, cx) else { return };
        let id = self.next_id();
        let pane = Pane::new(id, vec![Tab::Terminal(term)], 0);
        self.root = Some(match self.root.take() {
            Some(root) => root.split(self.focused, direction, pane).0,
            None => Node::Pane(pane),
        });
        self.focused = id;
        self.changed(window, cx);
    }

    pub fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_tab().map(Tab::entity_id) {
            self.close_entity(id, window, cx);
        }
    }

    /// Close the tab showing `id`. A closed conversation stays in the sidebar; a closed
    /// terminal's shell ends. An emptied pane is removed and its split folds.
    fn close_entity(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane_id, ix)) = self.find(|t| t.entity_id() == id) else { return };
        let Some(root) = self.root.as_mut() else { return };
        let pane = root.pane_mut(pane_id).expect("pane exists");
        let closed = pane.tabs.remove(ix);
        pane.recent.retain(|r| *r != id);
        // Go back to the tab used before; failing that, the neighbour.
        let back = pane.recent.iter().rev().find_map(|r| pane.tabs.iter().position(|t| t.entity_id() == *r));
        let next = back.unwrap_or(pane.active.min(pane.tabs.len().saturating_sub(1)));
        pane.set_active(next);
        if pane.tabs.is_empty() {
            let order = self.pane_ids();
            let at = order.iter().position(|p| *p == pane_id).unwrap_or(0);
            self.root = self.root.take().and_then(|root| root.remove(pane_id));
            let remaining = self.pane_ids();
            if self.focused == pane_id {
                self.focused = remaining.get(at.min(remaining.len().saturating_sub(1))).copied().unwrap_or(0);
            }
        }
        if let Tab::Thread { id, .. } = &closed
            && self.reported_thread.as_deref() == Some(id)
        {
            self.reported_thread = None;
        }
        if matches!(closed, Tab::Review(_)) && self.find(|t| matches!(t, Tab::Review(_))).is_none() {
            cx.emit(PaneEvent::ReviewClosed);
        }
        self.changed(window, cx);
    }

    pub fn focus_next_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.pane_ids();
        if let Some(ix) = ids.iter().position(|id| *id == self.focused) {
            self.focused = ids[(ix + 1) % ids.len()];
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Move focus to the nearest pane in a direction (dx, dy are -1, 0 or 1).
    pub fn focus_direction(&mut self, dx: f32, dy: f32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = &self.root else { return };
        let mut rects = Vec::new();
        root.rects(Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, &mut rects);
        let Some((_, from)) = rects.iter().find(|(id, _)| *id == self.focused).copied() else { return };
        let center = |r: &Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let (fx, fy) = center(&from);
        let best = rects
            .iter()
            .filter(|(id, r)| {
                *id != self.focused
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
            self.focused = *id;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    pub fn resize(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.focused;
        if self.root.as_mut().is_some_and(|root| root.resize(focused, delta)) {
            self.changed(window, cx);
        }
    }

    pub fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.root.as_mut().and_then(|r| r.pane_mut(self.focused))
            && !pane.tabs.is_empty()
        {
            let len = pane.tabs.len() as isize;
            pane.set_active((pane.active as isize + step).rem_euclid(len) as usize);
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    // ---------- rendering ----------

    fn render_node(&self, node: &Node, cx: &mut Context<Self>) -> AnyElement {
        match node {
            Node::Pane(pane) => self.render_pane(pane, cx),
            Node::Split { direction, ratio, first, second } => {
                let border = cx.theme().border;
                let (a, b) = (self.render_node(first, cx), self.render_node(second, cx));
                match direction {
                    SplitDirection::Right => h_flex()
                        .size_full()
                        .child(div().h_full().w(relative(*ratio)).min_w_0().child(a))
                        .child(div().h_full().flex_1().min_w_0().border_l_1().border_color(border).child(b))
                        .into_any_element(),
                    SplitDirection::Down => v_flex()
                        .size_full()
                        .child(div().w_full().h(relative(*ratio)).min_h_0().child(a))
                        .child(div().w_full().flex_1().min_h_0().border_t_1().border_color(border).child(b))
                        .into_any_element(),
                }
            }
        }
    }

    fn render_pane(&self, pane: &Pane, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg, accent, hover, border) =
            (theme.muted_foreground, theme.foreground, theme.primary, theme.secondary, theme.border);
        let (running, waiting) = (theme.warning, theme.primary);
        let focused = pane.id == self.focused;
        let pane_id = pane.id;
        let tabs = pane.tabs.iter().enumerate().map(|(ix, tab)| {
            let (title, dot) = match tab {
                Tab::Thread { view, .. } => {
                    let t = view.read(cx);
                    let dot = match t.status {
                        Status::Idle => None,
                        Status::Running => Some(running),
                        Status::AwaitingApproval => Some(waiting),
                    };
                    (t.title.clone().unwrap_or_else(|| "New thread".into()), dot)
                }
                Tab::Terminal(term) => {
                    let t = term.read(cx);
                    (tab_title(t.title(), t.cwd()), None)
                }
                Tab::Review(review) => (format!("Review · {}", review.read(cx).file_count(cx)), None),
            };
            let active = ix == pane.active;
            let id = tab.entity_id();
            h_flex()
                .id(SharedString::from(format!("tab-{pane_id}-{ix}")))
                .test_support()
                .gap_1()
                .max_w(px(220.))
                .px_2()
                .h_full()
                .cursor_pointer()
                .text_xs()
                .when(active, |el| el.border_b_2().border_color(if focused { accent } else { muted }))
                .text_color(if active { fg } else { muted })
                .hover(|el| el.bg(hover))
                .on_click(cx.listener(move |this, _, window, cx| this.activate_at((pane_id, ix), window, cx)))
                .when_some(dot, |el, color| el.child(div().flex_none().size_1p5().rounded_full().bg(color)))
                .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
                .child(
                    div()
                        .id(SharedString::from(format!("close-tab-{pane_id}-{ix}")))
                        .px_0p5()
                        .text_color(muted)
                        .hover(|el| el.text_color(fg))
                        .child("×")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_entity(id, window, cx);
                        })),
                )
        });
        let content: Option<AnyElement> = pane.tabs.get(pane.active).map(|tab| match tab {
            Tab::Thread { view, .. } => view.clone().into_any_element(),
            Tab::Terminal(term) => term.clone().into_any_element(),
            Tab::Review(review) => review.clone().into_any_element(),
        });
        v_flex()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.focused != pane_id {
                        this.focused = pane_id;
                        if let Some(id) = this.active_tab().and_then(|t| t.thread_id().map(str::to_owned))
                            && this.reported_thread.as_deref() != Some(&id)
                        {
                            this.reported_thread = Some(id.clone());
                            cx.emit(PaneEvent::ThreadFocused(id));
                        }
                        cx.notify();
                    }
                }),
            )
            // The tab strip only shows when there's more than one tab to switch between.
            .when(pane.tabs.len() > 1, |el| {
                el.child(
                    h_flex()
                        .h(px(30.))
                        .flex_none()
                        .px_1()
                        .border_b_1()
                        .border_color(border)
                        .child(h_flex().flex_1().min_w_0().h_full().overflow_hidden().children(tabs)),
                )
            })
            .child(div().flex_1().min_h_0().children(content))
            .into_any_element()
    }
}

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
        let body = match &self.root {
            Some(root) => self.render_node(root, cx),
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
            .when_some(self.error.clone(), |el, error| el.child(div().p_2().text_xs().child(error)))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneEvent, Panes, Start};
    use crate::changes::{ProjectChanges, ReviewView};
    use crate::store::{PaneLayout, SplitDirection, TabState};
    use crate::thread::{Backend, ThreadView};
    use alacritty_terminal::tty::Shell;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, bounds, point, px,
        size,
    };
    use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

    struct Setup {
        window: gpui_kit::AnyWindowHandle,
        panes: Entity<Panes>,
        dir: PathBuf,
        thread: Entity<ThreadView>,
        events: Rc<RefCell<Vec<String>>>,
    }

    /// Panes for a temp project with one known thread ("t1"), running `/bin/sh` terminals.
    fn open(cx: &mut TestAppContext, layout: Option<PaneLayout>, start_thread: bool) -> Setup {
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
                let review = cx.new(|cx| ReviewView::new(changes, true, cx));
                let threads = HashMap::from([("t1".to_string(), thread.clone())]);
                let start = Start { thread: start_thread.then(|| ("t1".to_string(), thread.clone())), terminal: true };
                let panes = cx.new(|cx| {
                    let shell = Some(Shell::new("/bin/sh".into(), vec![]));
                    Panes::new(project, layout.as_ref(), &threads, review, start, shell, window, cx)
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
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2]);
        assert_eq!(s.panes.read_with(cx, |p, _| p.active_thread_id()), Some("t1".into()));
        let layout = s.panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        let PaneLayout::Pane { tabs, .. } = layout else { panic!() };
        assert_eq!(tabs[0], TabState::Thread { id: "t1".into() });
        assert!(matches!(tabs[1], TabState::Terminal { .. }));
    }

    #[gpui_kit::test]
    fn showing_a_thread_focuses_its_tab_instead_of_opening_another(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.cycle_tab(1, window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.active_thread_id()), None, "the terminal tab is showing");
        let thread = s.thread.clone();
        s.panes.update_in(cx, |p, window, cx| p.show_thread("t1", thread, window, cx));
        cx.run_until_parked();
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2], "no second tab");
        assert_eq!(s.panes.read_with(cx, |p, _| p.active_thread_id()), Some("t1".into()));
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
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1, 2]);
        let layout = s.panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        let PaneLayout::Split { direction: SplitDirection::Right, first, second, .. } = layout else {
            panic!("expected a right split: {layout:?}");
        };
        let cwd = s.dir.canonicalize().unwrap();
        assert!(matches!(*first, PaneLayout::Pane { ref tabs, .. } if tabs == &vec![TabState::Terminal { cwd }]));
        assert!(matches!(*second, PaneLayout::Split { direction: SplitDirection::Down, .. }));
    }

    #[gpui_kit::test]
    fn vmux_shortcuts_split_tab_move_and_close(cx: &mut TestAppContext) {
        let s = open(cx, None, false);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        focus_active(&s.panes, cx);
        let first = s.panes.read_with(cx, |p, _| p.focused_pane());

        cx.update(|window, cx| window.press("ctrl-shift-e", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 1], "Ctrl+Shift+E splits right");
        let right = s.panes.read_with(cx, |p, _| p.focused_pane());
        assert_ne!(right, first, "focus moves to the new pane");

        cx.update(|window, cx| window.press("ctrl-shift-t", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1, 2], "Ctrl+Shift+T opens a tab in the focused pane");

        cx.update(|window, cx| window.press("ctrl-alt-left", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), first, "Ctrl+Alt+Left moves to the left pane");

        cx.update(|window, cx| window.press("ctrl-shift-w", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2], "closing a pane's last tab folds the split");
        assert_eq!(s.panes.read_with(cx, |p, _| p.focused_pane()), right);
    }

    #[gpui_kit::test]
    fn shortcuts_work_from_the_conversation_too(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        focus_active(&s.panes, cx);
        cx.update(|window, cx| window.press("ctrl-shift-o", cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1], "Ctrl+Shift+O from the prompt splits down");
    }

    #[gpui_kit::test]
    fn a_saved_layout_restores_every_kind_of_tab(cx: &mut TestAppContext) {
        let layout = PaneLayout::Split {
            direction: SplitDirection::Right,
            ratio: 0.6,
            first: Box::new(PaneLayout::Pane {
                tabs: vec![
                    TabState::Thread { id: "t1".into() },
                    TabState::Thread { id: "gone".into() },
                    TabState::Review,
                ],
                active: 1,
            }),
            second: Box::new(PaneLayout::Pane {
                tabs: vec![TabState::Terminal { cwd: "/no/such/dir".into() }],
                active: 0,
            }),
        };
        let s = open(cx, Some(layout), false);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2, 1], "an unknown thread is dropped");
        let restored = s.panes.read_with(cx, |p, cx| p.snapshot(cx)).unwrap();
        let PaneLayout::Split { ratio, first, second, .. } = restored else { panic!("{restored:?}") };
        assert_eq!(ratio, 0.6);
        let expected = vec![TabState::Thread { id: "t1".into() }, TabState::Review];
        assert!(matches!(*first, PaneLayout::Pane { ref tabs, .. } if *tabs == expected));
        let PaneLayout::Pane { tabs, .. } = *second else { panic!() };
        assert_eq!(
            tabs,
            vec![TabState::Terminal { cwd: s.dir.canonicalize().unwrap() }],
            "a missing folder opens in the project"
        );
    }

    #[gpui_kit::test]
    fn closing_the_review_tab_reports_it(cx: &mut TestAppContext) {
        let s = open(cx, None, true);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.show_review(window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![3]);
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
    fn a_shell_that_exits_closes_its_tab(cx: &mut TestAppContext) {
        let s = open(cx, None, false);
        let cx = &mut VisualTestContext::from_window(s.window, cx);
        s.panes.update_in(cx, |p, window, cx| p.new_terminal(window, cx));
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![2]);
        let term = s.panes.read_with(cx, |p, _| p.active_terminal().unwrap());
        term.update(cx, |t, _| t.type_text("exit\r"));
        for _ in 0..300 {
            cx.run_until_parked();
            if s.panes.read_with(cx, |p, _| p.shape()) == vec![1] {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(s.panes.read_with(cx, |p, _| p.shape()), vec![1]);
    }
}
