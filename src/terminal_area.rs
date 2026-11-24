//! A project's terminals, laid out like Vmux: panes split right or down, each with its own
//! tabs. Closing a pane's last tab folds its split back up. Shortcuts match Vmux's.

use std::path::PathBuf;

use alacritty_terminal::tty::Shell;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::store::{SplitDirection, TerminalLayout};
use crate::terminal::{TerminalExited, TerminalView};

pub const CONTEXT: &str = "TerminalArea";
const MIN_RATIO: f32 = 0.15;
const RESIZE_STEP: f32 = 0.05;

gpui_kit::actions!(
    terminal_area,
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

/// Vmux's terminal shortcuts, active while a terminal has focus.
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

struct Pane {
    id: usize,
    tabs: Vec<Entity<TerminalView>>,
    active: usize,
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

    fn snapshot(&self, cx: &App) -> TerminalLayout {
        match self {
            Node::Pane(pane) => TerminalLayout::Pane {
                tabs: pane.tabs.iter().filter_map(|t| t.read(cx).cwd()).collect(),
                active: pane.active,
            },
            Node::Split { direction, ratio, first, second } => TerminalLayout::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(first.snapshot(cx)),
                second: Box::new(second.snapshot(cx)),
            },
        }
    }
}

/// The layout changed (a pane or tab opened or closed, or a split moved); worth saving.
pub struct LayoutChanged;

impl EventEmitter<LayoutChanged> for TerminalArea {}

pub struct TerminalArea {
    root: Option<Node>,
    focused: usize,
    next_pane: usize,
    project_dir: PathBuf,
    /// The shell to run; the user's login shell when `None` (tests use `/bin/sh`).
    shell: Option<Shell>,
    focus: FocusHandle,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalArea {
    /// Restore `layout`, or start one pane with one terminal in `project_dir`.
    pub fn new(
        project_dir: PathBuf,
        layout: Option<&TerminalLayout>,
        shell: Option<Shell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            root: None,
            focused: 0,
            next_pane: 0,
            project_dir,
            shell,
            focus: cx.focus_handle(),
            error: None,
            _subscriptions: Vec::new(),
        };
        this.root = match layout {
            Some(layout) => this.restore(layout, window, cx),
            None => None,
        };
        if this.root.is_none() {
            this.root = this.new_pane(None, window, cx).map(Node::Pane);
        }
        this.focused = this.pane_ids().first().copied().unwrap_or(0);
        this
    }

    fn restore(&mut self, layout: &TerminalLayout, window: &mut Window, cx: &mut Context<Self>) -> Option<Node> {
        match layout {
            TerminalLayout::Pane { tabs, active } => {
                let id = self.next_id();
                let tabs: Vec<_> = tabs
                    .iter()
                    // A directory that no longer exists falls back to the project folder.
                    .filter_map(|dir| self.spawn(Some(dir.clone()).filter(|d| d.is_dir()), window, cx))
                    .collect();
                (!tabs.is_empty()).then(|| Node::Pane(Pane { id, active: (*active).min(tabs.len() - 1), tabs }))
            }
            TerminalLayout::Split { direction, ratio, first, second } => {
                match (self.restore(first, window, cx), self.restore(second, window, cx)) {
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
                        this.close_terminal(id, window, cx);
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

    fn new_pane(&mut self, cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Option<Pane> {
        let term = self.spawn(cwd, window, cx)?;
        let id = self.next_id();
        Some(Pane { id, tabs: vec![term], active: 0 })
    }

    pub fn pane_ids(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        if let Some(root) = &self.root {
            root.pane_ids(&mut ids);
        }
        ids
    }

    /// The focused pane's visible terminal.
    pub fn active_terminal(&self) -> Option<Entity<TerminalView>> {
        let pane = self.root.as_ref()?.pane(self.focused)?;
        pane.tabs.get(pane.active).cloned()
    }

    pub fn snapshot(&self, cx: &App) -> Option<TerminalLayout> {
        self.root.as_ref().map(|root| root.snapshot(cx))
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

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_active(window, cx);
        cx.emit(LayoutChanged);
        cx.notify();
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        match self.active_terminal() {
            Some(term) => term.read(cx).focus_handle(cx).focus(window, cx),
            None => self.focus.focus(window, cx),
        }
    }

    /// The new terminal starts where the focused one is.
    fn focused_cwd(&self, cx: &App) -> Option<PathBuf> {
        self.active_terminal().and_then(|t| t.read(cx).cwd())
    }

    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.root.is_none() {
            self.root = self.new_pane(None, window, cx).map(Node::Pane);
            self.focused = self.pane_ids().first().copied().unwrap_or(0);
            return self.changed(window, cx);
        }
        let cwd = self.focused_cwd(cx);
        let Some(term) = self.spawn(cwd, window, cx) else { return };
        if let Some(pane) = self.root.as_mut().and_then(|r| r.pane_mut(self.focused)) {
            pane.tabs.push(term);
            pane.active = pane.tabs.len() - 1;
        }
        self.changed(window, cx);
    }

    pub fn split(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.focused_cwd(cx);
        let Some(new) = self.new_pane(cwd, window, cx) else { return };
        let new_id = new.id;
        self.root = match self.root.take() {
            Some(root) => Some(root.split(self.focused, direction, new).0),
            None => Some(Node::Pane(new)),
        };
        self.focused = new_id;
        self.changed(window, cx);
    }

    pub fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(term) = self.active_terminal() {
            self.close_terminal(term.entity_id(), window, cx);
        }
    }

    /// Drop a terminal (its shell ends with it); an emptied pane is removed and its split folds.
    fn close_terminal(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.pane_ids();
        let Some(root) = self.root.as_mut() else { return };
        let Some(pane_id) =
            ids.into_iter().find(|p| root.pane(*p).is_some_and(|pane| pane.tabs.iter().any(|t| t.entity_id() == id)))
        else {
            return;
        };
        let pane = root.pane_mut(pane_id).expect("pane exists");
        pane.tabs.retain(|t| t.entity_id() != id);
        pane.active = pane.active.min(pane.tabs.len().saturating_sub(1));
        if pane.tabs.is_empty() {
            let order = self.pane_ids();
            let ix = order.iter().position(|p| *p == pane_id).unwrap_or(0);
            self.root = self.root.take().and_then(|root| root.remove(pane_id));
            let remaining = self.pane_ids();
            if self.focused == pane_id {
                self.focused = remaining.get(ix.min(remaining.len().saturating_sub(1))).copied().unwrap_or(0);
            }
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
            pane.active = (pane.active as isize + step).rem_euclid(len) as usize;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn activate(&mut self, pane: usize, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.root.as_mut().and_then(|r| r.pane_mut(pane)) {
            p.active = tab.min(p.tabs.len().saturating_sub(1));
        }
        self.focused = pane;
        self.focus_active(window, cx);
        cx.notify();
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
        let (muted, fg, accent, hover) = (theme.muted_foreground, theme.foreground, theme.primary, theme.secondary);
        let focused = pane.id == self.focused;
        let pane_id = pane.id;
        let tabs = pane.tabs.iter().enumerate().map(|(ix, term)| {
            let t = term.read(cx);
            let title = tab_title(t.title(), t.cwd());
            let active = ix == pane.active;
            let id = term.entity_id();
            h_flex()
                .id(SharedString::from(format!("tab-{pane_id}-{ix}")))
                .test_support()
                .gap_1()
                .max_w(px(180.))
                .px_2()
                .h_full()
                .cursor_pointer()
                .text_xs()
                .when(active, |el| el.border_b_2().border_color(if focused { accent } else { muted }))
                .text_color(if active { fg } else { muted })
                .hover(|el| el.bg(hover))
                .on_click(cx.listener(move |this, _, window, cx| this.activate(pane_id, ix, window, cx)))
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
                            this.close_terminal(id, window, cx);
                        })),
                )
        });
        let button = |id: &str, label: &'static str, tip: &'static str| {
            Button::new(SharedString::from(format!("{id}-{pane_id}"))).ghost().xsmall().label(label).tooltip(tip)
        };
        v_flex()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.focused != pane_id {
                        this.focused = pane_id;
                        cx.notify();
                    }
                }),
            )
            .child(
                h_flex()
                    .h(px(28.))
                    .flex_none()
                    .px_1()
                    .gap_0p5()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(h_flex().flex_1().min_w_0().h_full().overflow_hidden().children(tabs))
                    .child(button("new-tab", "+", "New tab (Ctrl+Shift+T)").on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.focused = pane_id;
                            this.new_tab(window, cx)
                        },
                    )))
                    .child(button("split-right", "◫", "Split right (Ctrl+Shift+E)").on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.focused = pane_id;
                            this.split(SplitDirection::Right, window, cx)
                        },
                    )))
                    .child(button("split-down", "⬓", "Split down (Ctrl+Shift+O)").on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.focused = pane_id;
                            this.split(SplitDirection::Down, window, cx)
                        },
                    ))),
            )
            .child(div().flex_1().min_h_0().children(pane.tabs.get(pane.active).cloned()))
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

impl Render for TerminalArea {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let body = match &self.root {
            Some(root) => self.render_node(root, cx),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(div().text_sm().text_color(muted).child("No terminals open."))
                .child(
                    Button::new("open-terminal")
                        .small()
                        .label("New terminal")
                        .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
                )
                .into_any_element(),
        };
        div()
            .id("terminal-area")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
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
    use super::TerminalArea;
    use crate::store::{SplitDirection, TerminalLayout};
    use alacritty_terminal::tty::Shell;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, bounds, point, px,
        size,
    };
    use std::path::PathBuf;

    fn open(
        cx: &mut TestAppContext,
        layout: Option<TerminalLayout>,
    ) -> (gpui_kit::AnyWindowHandle, Entity<TerminalArea>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tj-area-{}", crate::store::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::keymap::init(cx);
        });
        let project = dir.clone();
        let (window, area) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds(point(px(0.), px(0.)), size(px(1200.), px(800.))))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    let shell = Some(Shell::new("/bin/sh".into(), vec![]));
                    TerminalArea::new(project, layout.as_ref(), shell, window, cx)
                })
            })
            .unwrap()
        });
        (window, area, dir)
    }

    fn focus_terminal(area: &Entity<TerminalArea>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            let term = area.read(cx).active_terminal().unwrap();
            let focus = gpui_kit::Focusable::focus_handle(term.read(cx), cx);
            window.focus(&focus, cx);
            window.render_frame(cx);
        });
    }

    #[gpui_kit::test]
    fn splitting_builds_the_layout_and_snapshots_it(cx: &mut TestAppContext) {
        let (window, area, dir) = open(cx, None);
        let cx = &mut VisualTestContext::from_window(window, cx);
        area.update_in(cx, |a, window, cx| {
            a.split(SplitDirection::Right, window, cx);
            a.split(SplitDirection::Down, window, cx);
            a.new_tab(window, cx);
        });
        assert_eq!(area.read_with(cx, |a, _| a.shape()), vec![1, 1, 2]);
        let layout = area.read_with(cx, |a, cx| a.snapshot(cx)).unwrap();
        let TerminalLayout::Split { direction: SplitDirection::Right, first, second, .. } = layout else {
            panic!("expected a right split: {layout:?}");
        };
        assert!(matches!(*first, TerminalLayout::Pane { ref tabs, .. } if tabs == &vec![dir.canonicalize().unwrap()]));
        assert!(matches!(*second, TerminalLayout::Split { direction: SplitDirection::Down, .. }));
    }

    #[gpui_kit::test]
    fn vmux_shortcuts_split_tab_move_and_close(cx: &mut TestAppContext) {
        let (window, area, _) = open(cx, None);
        let cx = &mut VisualTestContext::from_window(window, cx);
        focus_terminal(&area, cx);
        let first = area.read_with(cx, |a, _| a.focused_pane());

        cx.update(|window, cx| window.press("ctrl-shift-e", cx));
        assert_eq!(area.read_with(cx, |a, _| a.shape()), vec![1, 1], "Ctrl+Shift+E splits right");
        let right = area.read_with(cx, |a, _| a.focused_pane());
        assert_ne!(right, first, "focus moves to the new pane");

        cx.update(|window, cx| window.press("ctrl-shift-t", cx));
        assert_eq!(area.read_with(cx, |a, _| a.shape()), vec![1, 2], "Ctrl+Shift+T opens a tab in the focused pane");

        cx.update(|window, cx| window.press("ctrl-alt-left", cx));
        assert_eq!(area.read_with(cx, |a, _| a.focused_pane()), first, "Ctrl+Alt+Left moves to the left pane");

        cx.update(|window, cx| window.press("ctrl-shift-w", cx));
        assert_eq!(area.read_with(cx, |a, _| a.shape()), vec![2], "closing a pane's last tab folds the split");
        assert_eq!(area.read_with(cx, |a, _| a.focused_pane()), right);
    }

    #[gpui_kit::test]
    fn a_saved_layout_is_restored_and_missing_folders_fall_back(cx: &mut TestAppContext) {
        let layout = TerminalLayout::Split {
            direction: SplitDirection::Down,
            ratio: 0.7,
            first: Box::new(TerminalLayout::Pane { tabs: vec!["/tmp".into(), "/no/such/dir".into()], active: 1 }),
            second: Box::new(TerminalLayout::Pane { tabs: vec!["/".into()], active: 0 }),
        };
        let (window, area, dir) = open(cx, Some(layout));
        let cx = &mut VisualTestContext::from_window(window, cx);
        assert_eq!(area.read_with(cx, |a, _| a.shape()), vec![2, 1]);
        let restored = area.read_with(cx, |a, cx| a.snapshot(cx)).unwrap();
        let TerminalLayout::Split { ratio, first, .. } = restored else { panic!("{restored:?}") };
        assert_eq!(ratio, 0.7);
        let TerminalLayout::Pane { tabs, active } = *first else { panic!() };
        assert_eq!(active, 1);
        assert_eq!(tabs[1], dir.canonicalize().unwrap(), "a missing folder opens in the project");
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
        let (window, area, _) = open(cx, None);
        let cx = &mut VisualTestContext::from_window(window, cx);
        area.update_in(cx, |a, window, cx| a.new_tab(window, cx));
        assert_eq!(area.read_with(cx, |a, _| a.shape()), vec![2]);
        let term = area.read_with(cx, |a, _| a.active_terminal().unwrap());
        term.update(cx, |t, _| t.type_text("exit\r"));
        for _ in 0..300 {
            cx.run_until_parked();
            if area.read_with(cx, |a, _| a.shape()) == vec![1] {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(area.read_with(cx, |a, _| a.shape()), vec![1]);
    }
}
