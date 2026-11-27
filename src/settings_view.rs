//! The settings modal: General, Appearance, Keybindings and About, as tabs.
//!
//! It works from the keyboard (ROADMAP.md, "Keyboard rules"): Ctrl+PageUp/PageDown switch
//! tabs, Up/Down pick a row, Enter or Space toggles a switch, cycles a choice or starts
//! recording a shortcut, Left/Right turn a switch off or on, step a slider or a choice, and
//! Escape closes. On the Keybindings tab, Enter records a new key (the next key pressed, with
//! Escape cancelling), Backspace leaves the command without one, and R gives back its default.
//! A key given to one command is taken from any other that had it.
//!
//! The font rows open a filtered list of installed families in place of the tab's rows:
//! typing filters it, Up/Down pick, Enter uses the family picked and Escape goes back. On the
//! row itself, Left/Right step to the previous or next family and Backspace gives back the
//! default.

use std::path::Path;
use std::sync::{Arc, LazyLock};

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState, MoveDown, MoveUp},
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    switch::Switch,
    tab::{Tab, TabBar},
};
use gpui_kit::{TestSupportExt as _, prelude::FluentBuilder as _, *};

use crate::keybindings::{self, COMMANDS, Group};
use crate::settings::Settings;
use crate::workspace::Workspace;

const CONTEXT: &str = "SettingsModal";
const BODY_HEIGHT: f32 = 430.;
const OPACITY_STEP: f32 = 0.05;
const GLIDE_STEP: f32 = 10.;
const FONT_ROW_HEIGHT: f32 = 32.;
/// The font list's height: the body less the filter above it.
const FONT_LIST_HEIGHT: f32 = BODY_HEIGHT - 44.;

gpui_kit::actions!(settings_view, [Up, Down, Left, Right, Activate, Clear, Reset, NextTab, PreviousTab]);

pub fn bindings() -> Vec<KeyBinding> {
    let c = Some(CONTEXT);
    vec![
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("enter", Activate, c),
        KeyBinding::new("space", Activate, c),
        KeyBinding::new("backspace", Clear, c),
        KeyBinding::new("delete", Clear, c),
        KeyBinding::new("r", Reset, c),
        KeyBinding::new("ctrl-pagedown", NextTab, c),
        KeyBinding::new("ctrl-pageup", PreviousTab, c),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Appearance,
    Keybindings,
    About,
}

impl SettingsTab {
    const ALL: [SettingsTab; 4] =
        [SettingsTab::General, SettingsTab::Appearance, SettingsTab::Keybindings, SettingsTab::About];

    fn title(self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Keybindings => "Keybindings",
            SettingsTab::About => "About",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// The tab's rows, top to bottom.
    fn rows(self) -> Vec<RowKind> {
        match self {
            SettingsTab::General => vec![RowKind::DefaultAccount, RowKind::EightyTwenty],
            SettingsTab::Appearance => {
                vec![
                    RowKind::FollowTheme,
                    RowKind::UiFont,
                    RowKind::MonoFont,
                    RowKind::TransparentSidebar,
                    RowKind::Opacity,
                    RowKind::CursorAnimation,
                ]
            }
            SettingsTab::Keybindings => Group::ALL
                .iter()
                .flat_map(|g| COMMANDS.iter().enumerate().filter(move |(_, c)| c.group == *g).map(|(ix, _)| ix))
                .map(RowKind::Key)
                .collect(),
            SettingsTab::About => Vec::new(),
        }
    }
}

/// A row that can be picked from the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    DefaultAccount,
    EightyTwenty,
    FollowTheme,
    UiFont,
    MonoFont,
    TransparentSidebar,
    Opacity,
    CursorAnimation,
    /// A command, by its index in [`COMMANDS`].
    Key(usize),
}

impl RowKind {
    fn font(self) -> Option<FontKind> {
        match self {
            RowKind::UiFont => Some(FontKind::Ui),
            RowKind::MonoFont => Some(FontKind::Mono),
            _ => None,
        }
    }
}

/// Which of the two fonts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontKind {
    Ui,
    Mono,
}

impl FontKind {
    fn label(self) -> &'static str {
        match self {
            FontKind::Ui => "Interface font",
            FontKind::Mono => "Monospace font",
        }
    }

    fn row(self) -> RowKind {
        match self {
            FontKind::Ui => RowKind::UiFont,
            FontKind::Mono => RowKind::MonoFont,
        }
    }

    /// The family settings pick; `None` for the default.
    fn picked(self, settings: &Settings) -> Option<String> {
        match self {
            FontKind::Ui => settings.ui_font.clone(),
            FontKind::Mono => settings.mono_font.clone(),
        }
    }

    fn set(self, settings: &mut Settings, family: Option<String>) {
        match self {
            FontKind::Ui => settings.ui_font = family,
            FontKind::Mono => settings.mono_font = family,
        }
    }

    fn default_name(self, cx: &mut App) -> SharedString {
        let defaults = crate::fonts::defaults(cx);
        match self {
            FontKind::Ui => defaults.ui,
            FontKind::Mono => defaults.mono,
        }
    }

    /// The families it can be, with `None` (the default) first.
    fn choices(self, cx: &mut App) -> Vec<Option<String>> {
        let families = match self {
            FontKind::Ui => crate::fonts::ui_choices(cx),
            FontKind::Mono => crate::fonts::mono_choices(cx),
        };
        std::iter::once(None).chain(families.iter().cloned().map(Some)).collect()
    }
}

/// The font list shown in place of the Appearance rows.
struct FontPicker {
    kind: FontKind,
    filter: Entity<InputState>,
    /// The row picked, in the filtered list.
    row: usize,
    scroll: UniformListScrollHandle,
    _events: Subscription,
}

pub struct SettingsView {
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    tab: SettingsTab,
    /// The picked row, in the tab's [`SettingsTab::rows`].
    row: usize,
    /// The command whose new key is being waited for.
    recording: Option<&'static str>,
    /// What the last change did, or why it couldn't ("Ctrl+Alt+N was taken from ...").
    note: Option<SharedString>,
    fonts: Option<FontPicker>,
    opacity_slider: Entity<SliderState>,
    glide_slider: Entity<SliderState>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(workspace: WeakEntity<Workspace>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
        let opacity_slider = cx.new(|_| {
            SliderState::new()
                .min(Settings::MIN_OPACITY)
                .max(1.0)
                .step(OPACITY_STEP)
                .default_value(settings.sidebar_opacity)
        });
        let glide_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(Settings::MAX_GLIDE_MS as f32)
                .step(GLIDE_STEP)
                .default_value(settings.cursor_glide_ms as f32)
        });
        let opacity_events = cx.subscribe(&opacity_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
            if let SliderValue::Single(opacity) = *value {
                this.change(cx, |s| s.sidebar_opacity = opacity);
            }
        });
        let glide_events = cx.subscribe(&glide_slider, |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
            if let SliderValue::Single(ms) = *value {
                let ms = ms.round().max(0.) as u32;
                this.change(cx, |s| s.cursor_glide_ms = ms);
            }
        });
        // While a shortcut is being recorded, the next key goes to it, before any binding (or
        // the dialog's Escape) can act on it. While a font list shows, Enter uses the family
        // picked and Escape goes back to the rows, where the dialog would take either to close.
        let view = cx.entity().downgrade();
        let recorder = cx.intercept_keystrokes(move |event, window, cx| {
            let Some(view) = view.upgrade() else { return };
            if view.read(cx).fonts.is_some() {
                let ks = &event.keystroke;
                if ks.modifiers == Modifiers::none() && matches!(ks.key.as_str(), "escape" | "enter") {
                    cx.stop_propagation();
                    let enter = ks.key == "enter";
                    view.update(
                        cx,
                        |v, cx| if enter { v.use_picked_font(window, cx) } else { v.close_fonts(window, cx) },
                    );
                }
                return;
            }
            if view.read(cx).recording.is_none() {
                return;
            }
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            view.update(cx, |v, cx| v.record(&keystroke, window, cx));
        });
        Self {
            workspace,
            focus: cx.focus_handle(),
            tab: SettingsTab::General,
            row: 0,
            recording: None,
            note: None,
            fonts: None,
            opacity_slider,
            glide_slider,
            scroll: ScrollHandle::new(),
            _subscriptions: vec![opacity_events, glide_events, recorder],
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    #[cfg(test)]
    pub fn recording(&self) -> Option<&'static str> {
        self.recording
    }

    /// The font whose list shows.
    #[cfg(test)]
    pub fn picking_font(&self) -> Option<FontKind> {
        self.fonts.as_ref().map(|f| f.kind)
    }

    /// Change the settings through the workspace, which saves them and applies what changed.
    fn change(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Settings)) {
        let _ = self.workspace.update(cx, |w, cx| w.update_settings(cx, change));
        cx.notify();
    }

    pub fn select_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.row = 0;
            self.recording = None;
            self.note = None;
            self.fonts = None;
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }

    fn step_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = SettingsTab::ALL.len() as isize;
        let next = (self.tab.index() as isize + step).rem_euclid(count) as usize;
        self.select_tab(SettingsTab::ALL[next], cx);
    }

    fn step_row(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.tab.rows().len();
        if count == 0 {
            return;
        }
        self.row = (self.row as isize + step).clamp(0, count as isize - 1) as usize;
        self.scroll.scroll_to_item(self.child_index(self.row));
        cx.notify();
    }

    /// Where row `row` sits among the scrolled body's children: the Keybindings tab has a
    /// heading before each group.
    fn child_index(&self, row: usize) -> usize {
        match self.tab {
            SettingsTab::Keybindings => {
                let rows = self.tab.rows();
                let group_of = |r: &RowKind| match r {
                    RowKind::Key(ix) => Some(COMMANDS[*ix].group),
                    _ => None,
                };
                let Some(group) = rows.get(row).and_then(group_of) else { return row };
                let headings = Group::ALL.iter().position(|g| *g == group).unwrap_or(0) + 1;
                row + headings
            }
            _ => row,
        }
    }

    fn current(&self) -> Option<RowKind> {
        self.tab.rows().get(self.row).copied()
    }

    fn pick(&mut self, row: RowKind, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab.rows().iter().position(|r| *r == row) {
            self.row = ix;
            cx.notify();
        }
    }

    // ---------- keys ----------

    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.current() {
            Some(RowKind::DefaultAccount) => self.step_account(1, cx),
            Some(RowKind::EightyTwenty) => self.change(cx, |s| s.eighty_twenty = !s.eighty_twenty),
            Some(RowKind::FollowTheme) => self.change(cx, |s| s.follow_omarchy_theme = !s.follow_omarchy_theme),
            Some(RowKind::TransparentSidebar) => self.change(cx, |s| s.transparent_sidebar = !s.transparent_sidebar),
            Some(RowKind::UiFont) => self.open_fonts(FontKind::Ui, window, cx),
            Some(RowKind::MonoFont) => self.open_fonts(FontKind::Mono, window, cx),
            Some(RowKind::Opacity | RowKind::CursorAnimation) => self.step_value(1., window, cx),
            Some(RowKind::Key(ix)) => self.start_recording(COMMANDS[ix].id, cx),
            None => {}
        }
    }

    /// Left (-1) or Right (1) on the picked row.
    fn horizontal(&mut self, step: f32, window: &mut Window, cx: &mut Context<Self>) {
        let on = step > 0.;
        match self.current() {
            Some(RowKind::DefaultAccount) => self.step_account(step as isize, cx),
            Some(RowKind::EightyTwenty) => self.change(cx, |s| s.eighty_twenty = on),
            Some(RowKind::FollowTheme) => self.change(cx, |s| s.follow_omarchy_theme = on),
            Some(RowKind::TransparentSidebar) => self.change(cx, |s| s.transparent_sidebar = on),
            Some(RowKind::UiFont) => self.step_font(FontKind::Ui, step as isize, cx),
            Some(RowKind::MonoFont) => self.step_font(FontKind::Mono, step as isize, cx),
            Some(RowKind::Opacity | RowKind::CursorAnimation) => self.step_value(step, window, cx),
            Some(RowKind::Key(_)) | None => {}
        }
    }

    /// Move a slider by one step, keeping its thumb in step with the setting.
    fn step_value(&mut self, step: f32, window: &mut Window, cx: &mut Context<Self>) {
        let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
        match self.current() {
            Some(RowKind::Opacity) => {
                let value = (settings.sidebar_opacity + step * OPACITY_STEP).clamp(Settings::MIN_OPACITY, 1.0);
                let value = (value / OPACITY_STEP).round() * OPACITY_STEP;
                self.opacity_slider.update(cx, |s, cx| s.set_value(value, window, cx));
                self.change(cx, |s| s.sidebar_opacity = value);
            }
            Some(RowKind::CursorAnimation) => {
                let ms = (settings.cursor_glide_ms as f32 + step * GLIDE_STEP).clamp(0., Settings::MAX_GLIDE_MS as f32);
                self.glide_slider.update(cx, |s, cx| s.set_value(ms, window, cx));
                self.change(cx, |s| s.cursor_glide_ms = ms as u32);
            }
            _ => {}
        }
    }

    fn step_account(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let (ids, current) = {
            let accounts = workspace.read(cx).accounts();
            (accounts.accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>(), accounts.default.clone())
        };
        if ids.is_empty() {
            return;
        }
        let at = ids.iter().position(|id| *id == current).unwrap_or(0) as isize;
        let next = ids[(at + step).rem_euclid(ids.len() as isize) as usize].clone();
        workspace.update(cx, |w, cx| w.set_default_account(&next, cx));
        cx.notify();
    }

    fn start_recording(&mut self, id: &'static str, cx: &mut Context<Self>) {
        self.recording = Some(id);
        self.note = None;
        cx.notify();
    }

    /// The key pressed while recording: Escape gives up, a modifier on its own waits for the
    /// rest, and anything else becomes the command's key if it can be one.
    fn record(&mut self, keystroke: &Keystroke, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.recording else { return };
        if keystroke.key == "escape" && keystroke.modifiers == Modifiers::none() {
            self.recording = None;
            cx.notify();
            return;
        }
        let Some(key) = keybindings::from_keystroke(keystroke) else { return };
        if let Some(why) = keybindings::unusable(&key) {
            self.note = Some(format!("{} can't be a shortcut. {why}", keybindings::display(&key)).into());
            cx.notify();
            return;
        }
        self.recording = None;
        let mut taken = Vec::new();
        self.change(cx, |s| taken = keybindings::assign(&mut s.keybindings, id, &key));
        let shown = keybindings::display(&key);
        let mut note = match taken.first().and_then(|t| keybindings::command(t)) {
            Some(other) => format!("{shown} was taken from “{}”, which has no key now.", other.label),
            None => format!("{shown} is set."),
        };
        if keybindings::shadows_shell(&key) {
            note.push_str(&format!(" The terminal's shell no longer gets {shown}."));
        }
        self.note = Some(note.into());
        cx.notify();
    }

    // ---------- fonts ----------

    /// Show `kind`'s list, its current family picked, with the filter taking the keyboard.
    fn open_fonts(&mut self, kind: FontKind, window: &mut Window, cx: &mut Context<Self>) {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Find a font"));
        let events = cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
            if let (InputEvent::Change, Some(fonts)) = (event, this.fonts.as_mut()) {
                fonts.row = 0;
                fonts.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
        });
        let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
        let row = kind.choices(cx).iter().position(|f| *f == kind.picked(&settings)).unwrap_or(0);
        let scroll = UniformListScrollHandle::new();
        scroll.scroll_to_item(row, ScrollStrategy::Center);
        filter.update(cx, |i, cx| i.focus(window, cx));
        self.recording = None;
        self.note = None;
        self.fonts = Some(FontPicker { kind, filter, row, scroll, _events: events });
        cx.notify();
    }

    /// Back to the rows, the font's row picked.
    fn close_fonts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(fonts) = self.fonts.take() {
            self.pick(fonts.kind.row(), cx);
        }
        self.focus(window, cx);
        cx.notify();
    }

    /// The list's families for the filter's words; `None` is the default, listed while nothing
    /// is typed (or "default" is).
    fn font_entries(&self, cx: &mut App) -> Vec<Option<String>> {
        let Some(fonts) = &self.fonts else { return Vec::new() };
        let query = fonts.filter.read(cx).value().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        fonts
            .kind
            .choices(cx)
            .into_iter()
            .filter(|family| crate::fonts::matches(family.as_deref().unwrap_or("default"), &words))
            .collect()
    }

    fn move_font(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.font_entries(cx).len();
        let Some(fonts) = self.fonts.as_mut() else { return };
        if count == 0 {
            return;
        }
        fonts.row = (fonts.row as isize + step).rem_euclid(count as isize) as usize;
        fonts.scroll.scroll_to_item(fonts.row, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn use_picked_font(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, row)) = self.fonts.as_ref().map(|f| (f.kind, f.row)) else { return };
        if let Some(family) = self.font_entries(cx).get(row).cloned() {
            self.use_font(kind, family, window, cx);
        }
    }

    fn use_font(&mut self, kind: FontKind, family: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_fonts(window, cx);
        self.set_font(kind, family, cx);
    }

    /// Left/Right on a font's row: the family before or after the one used, in the list's order.
    fn step_font(&mut self, kind: FontKind, step: isize, cx: &mut Context<Self>) {
        let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
        let choices = kind.choices(cx);
        let at = choices.iter().position(|f| *f == kind.picked(&settings)).unwrap_or(0) as isize;
        let next = (at + step).clamp(0, choices.len() as isize - 1) as usize;
        if next as isize != at {
            self.set_font(kind, choices[next].clone(), cx);
        }
    }

    fn set_font(&mut self, kind: FontKind, family: Option<String>, cx: &mut Context<Self>) {
        self.change(cx, |s| kind.set(s, family.clone()));
        let shown = family.clone().unwrap_or_else(|| format!("{} (default)", kind.default_name(cx)));
        let mut note = format!("{} is {shown}.", kind.label());
        if kind == FontKind::Ui && family.as_deref().is_some_and(|f| !crate::fonts::has_static_bold(f)) {
            note.push_str(" It has no bold face of its own, so bold text looks regular.");
        }
        self.note = Some(note.into());
        cx.notify();
    }

    /// The filter and the list, in place of the Appearance rows.
    fn font_list(&self, fonts: &FontPicker, cx: &mut Context<Self>) -> AnyElement {
        let entries = std::rc::Rc::new(self.font_entries(cx));
        let selected = fonts.row.min(entries.len().saturating_sub(1));
        let kind = fonts.kind;
        let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
        let current = kind.picked(&settings);
        let default_name = kind.default_name(cx);
        let theme = cx.theme();
        let (muted, hover, picked) = (theme.foreground.opacity(0.6), theme.accent.opacity(0.5), theme.accent);
        let view = cx.entity().downgrade();
        let list = {
            let entries = entries.clone();
            uniform_list("font-list", entries.len(), move |range, _, _| {
                range
                    .map(|ix| {
                        let family = entries[ix].clone();
                        let (view, clicked) = (view.clone(), family.clone());
                        let shown_in: SharedString = family.clone().map_or(default_name.clone(), SharedString::from);
                        let name = div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .font_family(shown_in.clone())
                            .child(shown_in);
                        h_flex()
                            .id(SharedString::from(format!("font-{}", family.as_deref().unwrap_or(""))))
                            .test_support()
                            .w_full()
                            .h(px(FONT_ROW_HEIGHT))
                            .px_2()
                            .gap_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(ix == selected, |el| el.bg(picked))
                            .hover(|el| el.bg(hover))
                            .on_click(move |_, window, cx| {
                                let _ = view.update(cx, |v, cx| v.use_font(kind, clicked.clone(), window, cx));
                            })
                            .when(family.is_none(), |el| {
                                el.child(div().flex_none().text_sm().child("Default"))
                                    .child(div().flex_none().text_sm().text_color(muted).child("·"))
                            })
                            .child(name)
                            .child(div().flex_1())
                            .when(family == current, |el| {
                                el.child(Icon::new(IconName::Check).small().text_color(muted).flex_none())
                            })
                    })
                    .collect()
            })
            .track_scroll(&fonts.scroll)
            .w_full()
            .h(px(FONT_LIST_HEIGHT))
        };
        v_flex()
            .h(px(BODY_HEIGHT))
            .gap_2()
            // The filter keeps the keyboard; Up and Down move through the list instead of its text.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_font(-1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_font(1, cx);
                cx.stop_propagation();
            }))
            .child(Input::new(&fonts.filter).cleanable(true))
            .map(|el| {
                if entries.is_empty() {
                    el.child(div().py_4().text_sm().text_color(muted).text_center().child("No fonts match."))
                } else {
                    el.child(list)
                }
            })
            .into_any_element()
    }

    fn clear_key(&mut self, cx: &mut Context<Self>) {
        if let Some(kind) = self.current().and_then(RowKind::font) {
            self.set_font(kind, None, cx);
            return;
        }
        let Some(RowKind::Key(ix)) = self.current() else { return };
        let command = &COMMANDS[ix];
        self.recording = None;
        self.change(cx, |s| keybindings::clear(&mut s.keybindings, command.id));
        self.note = Some(format!("“{}” has no key now.", command.label).into());
        cx.notify();
    }

    fn reset_key(&mut self, ix: usize, cx: &mut Context<Self>) {
        let command = &COMMANDS[ix];
        self.recording = None;
        let mut taken = Vec::new();
        self.change(cx, |s| taken = keybindings::reset(&mut s.keybindings, command.id));
        self.note = Some(
            match taken.first().and_then(|t| keybindings::command(t)) {
                Some(other) => format!("“{}” has its default key again, taken from “{}”.", command.label, other.label),
                None => format!("“{}” has its default key again.", command.label),
            }
            .into(),
        );
        cx.notify();
    }

    // ---------- tabs ----------

    fn general(&self, picked: Option<RowKind>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(workspace) = self.workspace.upgrade() else { return Vec::new() };
        let accounts = workspace.read(cx).accounts().clone();
        let choices = h_flex().gap_1().flex_wrap().children(accounts.accounts.iter().map(|account| {
            let id = account.id.clone();
            let chosen = account.id == accounts.default;
            Button::new(SharedString::from(format!("default-account-{}", account.id)))
                .small()
                .label(account.name.clone())
                .when(chosen, |b| b.primary())
                .when(!chosen, |b| b.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pick(RowKind::DefaultAccount, cx);
                    let _ = this.workspace.update(cx, |w, cx| w.set_default_account(&id, cx));
                }))
        }));
        let eighty_twenty = Switch::new("eighty-twenty")
            .checked(cx.try_global::<Settings>().is_none_or(|s| s.eighty_twenty))
            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                let checked = *checked;
                this.pick(RowKind::EightyTwenty, cx);
                this.change(cx, |s| s.eighty_twenty = checked);
            }))
            .into_any_element();
        vec![
            self.row(
                RowKind::DefaultAccount,
                picked,
                "Default account",
                Some("For threads in projects that don't pick their own. Threads already open keep theirs.".into()),
                choices.into_any_element(),
                cx,
            ),
            div()
                .px_2()
                .pt_2()
                .text_xs()
                .text_color(cx.theme().foreground.opacity(0.6))
                .child("A project or thread can switch account from the menus under the prompt box. Accounts are listed in accounts.toml (see About).")
                .into_any_element(),
            self.row(
                RowKind::EightyTwenty,
                picked,
                "80/20 summaries",
                Some("Claude ends finished work with its result, your next steps and any decision, shown as a card. New threads pick up a change.".into()),
                eighty_twenty,
                cx,
            ),
        ]
    }

    fn appearance(&self, picked: Option<RowKind>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
        let muted = cx.theme().foreground.opacity(0.6);
        let theme_name = self.workspace.upgrade().and_then(|w| crate::omarchy::theme_name(w.read(cx).home()));
        let glide = match settings.cursor_glide_ms {
            0 => "Off".to_string(),
            ms => format!("{ms} ms"),
        };
        let switch =
            |id: &'static str, checked: bool, row: RowKind, set: fn(&mut Settings, bool), cx: &mut Context<Self>| {
                Switch::new(id)
                    .checked(checked)
                    .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                        let checked = *checked;
                        this.pick(row, cx);
                        this.change(cx, |s| set(s, checked));
                    }))
                    .into_any_element()
            };
        let slider = |value: String, state: &Entity<SliderState>, disabled: bool| {
            h_flex()
                .w(px(240.))
                .gap_3()
                .child(div().flex_1().child(Slider::new(state).disabled(disabled)))
                .child(div().w(px(48.)).text_sm().text_color(muted).text_right().child(value))
                .into_any_element()
        };
        let follow = switch(
            "follow-theme",
            settings.follow_omarchy_theme,
            RowKind::FollowTheme,
            |s, v| s.follow_omarchy_theme = v,
            cx,
        );
        let transparent = switch(
            "transparent-sidebar",
            settings.transparent_sidebar,
            RowKind::TransparentSidebar,
            |s, v| s.transparent_sidebar = v,
            cx,
        );
        let ui_font = self.font_control(FontKind::Ui, &settings, cx);
        let mono_font = self.font_control(FontKind::Mono, &settings, cx);
        vec![
            self.row(
                RowKind::FollowTheme,
                picked,
                "Follow Omarchy theme",
                Some(theme_name.unwrap_or_else(|| "No Omarchy theme found".into())),
                follow,
                cx,
            ),
            self.row(
                RowKind::UiFont,
                picked,
                FontKind::Ui.label(),
                Some("Menus, threads and everything that isn't code.".into()),
                ui_font,
                cx,
            ),
            self.row(
                RowKind::MonoFont,
                picked,
                FontKind::Mono.label(),
                Some("Code, tool output and the terminal.".into()),
                mono_font,
                cx,
            ),
            self.row(
                RowKind::TransparentSidebar,
                picked,
                "Transparent sidebar",
                Some("See the desktop through the sidebar and the top bar.".into()),
                transparent,
                cx,
            ),
            self.row(
                RowKind::Opacity,
                picked,
                "Sidebar opacity",
                Some("While the sidebar is transparent.".into()),
                slider(
                    format!("{:.0}%", settings.sidebar_opacity * 100.0),
                    &self.opacity_slider,
                    !settings.transparent_sidebar,
                ),
                cx,
            ),
            self.row(
                RowKind::CursorAnimation,
                picked,
                "Cursor animation",
                Some("How long the cursor takes to glide to a new place, in the terminal and text fields.".into()),
                slider(glide, &self.glide_slider, false),
                cx,
            ),
        ]
    }

    /// A font row's control: the family used, drawn in itself, and buttons to change it or
    /// give back the default.
    fn font_control(&self, kind: FontKind, settings: &Settings, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().foreground.opacity(0.6);
        let family = kind.picked(settings);
        let name: SharedString = family.clone().map_or_else(|| kind.default_name(cx), SharedString::from);
        let row = kind.row();
        h_flex()
            .gap_1()
            .child(
                div()
                    .max_w(px(220.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_sm()
                    .font_family(name.clone())
                    .child(name),
            )
            .when(family.is_none(), |el| el.child(div().text_xs().text_color(muted).child("Default")))
            .child(
                Button::new(SharedString::from(format!("change-font-{kind:?}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Pencil)
                    .tooltip("Change (Enter)")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick(row, cx);
                        this.open_fonts(kind, window, cx);
                    })),
            )
            .when(family.is_some(), |el| {
                el.child(
                    Button::new(SharedString::from(format!("reset-font-{kind:?}")))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Undo2)
                        .tooltip("Use the default (Backspace)")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick(row, cx);
                            this.set_font(kind, None, cx);
                        })),
                )
            })
            .into_any_element()
    }

    fn keybindings(&self, picked: Option<RowKind>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let overrides = cx.try_global::<Settings>().map(|s| s.keybindings.clone()).unwrap_or_default();
        let theme = cx.theme();
        let (muted, chip_bg, border) = (theme.foreground.opacity(0.6), theme.secondary, theme.border);
        let mut children = Vec::new();
        for group in Group::ALL {
            children.push(
                div()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(group.title())
                    .into_any_element(),
            );
            for (ix, command) in COMMANDS.iter().enumerate().filter(|(_, c)| c.group == group) {
                let keys = command.keys(&overrides);
                let recording = self.recording == Some(command.id);
                let chip = |text: String| {
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .border_1()
                        .border_color(border)
                        .bg(chip_bg)
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(text)
                };
                let shown: AnyElement = if recording {
                    div()
                        .text_xs()
                        .text_color(cx.theme().primary)
                        .child("Press a key… (Esc cancels)")
                        .into_any_element()
                } else if keys.is_empty() {
                    div().text_xs().text_color(muted).child("None").into_any_element()
                } else {
                    h_flex().gap_1().children(keys.iter().map(|k| chip(keybindings::display(k)))).into_any_element()
                };
                let id = command.id;
                let control = h_flex()
                    .gap_1()
                    .child(shown)
                    .child(
                        Button::new(SharedString::from(format!("change-key-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Pencil)
                            .tooltip("Change (Enter)")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick(RowKind::Key(ix), cx);
                                this.start_recording(id, cx);
                            })),
                    )
                    .when(!command.is_default(&overrides), |el| {
                        el.child(
                            Button::new(SharedString::from(format!("reset-key-{id}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Undo2)
                                .tooltip("Reset to default (R)")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick(RowKind::Key(ix), cx);
                                    this.reset_key(ix, cx);
                                })),
                        )
                    })
                    .into_any_element();
                children.push(self.row(RowKind::Key(ix), picked, command.label, None, control, cx));
            }
        }
        children
    }

    fn about(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        static ICON: LazyLock<Arc<Image>> = LazyLock::new(|| {
            Arc::new(Image::from_bytes(ImageFormat::Png, include_bytes!("../data/icons/tj-128.png").to_vec()))
        });
        let muted = cx.theme().foreground.opacity(0.6);
        let Some(workspace) = self.workspace.upgrade() else { return Vec::new() };
        let (paths, home) = {
            let w = workspace.read(cx);
            (w.paths().clone(), w.home().to_path_buf())
        };
        let theme_name = crate::omarchy::theme_name(&home);
        let file =
            |label: &'static str, path: &Path| {
                h_flex().gap_3().text_sm().child(div().w(px(110.)).flex_none().text_color(muted).child(label)).child(
                    div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(tilde(path, &home)),
                )
            };
        vec![
            h_flex()
                .gap_4()
                .p_2()
                .child(img(ICON.clone()).size(px(72.)).flex_none())
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_xl().font_weight(FontWeight::BOLD).child("TJ"))
                        .child(
                            div().text_sm().text_color(muted).child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                        )
                        .child(div().text_sm().child("A native GUI for coding agents, built in Rust on gpui."))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Named after Tony Jay, who voiced Judge Claude Frollo."),
                        ),
                )
                .into_any_element(),
            v_flex()
                .gap_1p5()
                .p_2()
                .pt_4()
                .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child("Files"))
                .child(file("Settings", &paths.config_dir.join("settings.toml")))
                .child(file("Accounts", &paths.accounts_file()))
                .child(file("Projects", &paths.state_file()))
                .child(file("Threads", &paths.data_dir))
                .child(
                    h_flex()
                        .gap_3()
                        .text_sm()
                        .pt_2()
                        .child(div().w(px(110.)).flex_none().text_color(muted).child("Omarchy theme"))
                        .child(theme_name.unwrap_or_else(|| "None found".into())),
                )
                .into_any_element(),
        ]
    }

    /// A row that can be picked: its label (and hint) on the left, its control on the right.
    fn row(
        &self,
        kind: RowKind,
        picked: Option<RowKind>,
        label: &'static str,
        hint: Option<String>,
        control: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, accent) = (theme.foreground.opacity(0.6), theme.accent);
        let id = match kind {
            RowKind::Key(ix) => SharedString::from(format!("settings-row-{}", COMMANDS[ix].id)),
            other => SharedString::from(format!("settings-row-{other:?}")),
        };
        h_flex()
            .id(id)
            .test_support()
            .gap_4()
            .justify_between()
            .px_2()
            .py_1p5()
            .rounded_md()
            .when(picked == Some(kind), |el| el.bg(accent))
            .on_click(cx.listener(move |this, _, _, cx| this.pick(kind, cx)))
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(div().text_sm().child(label))
                    .children(hint.map(|h| div().text_xs().text_color(muted).child(h))),
            )
            .child(div().flex_none().child(control))
            .into_any_element()
    }
}

/// `path` with the home folder written `~`.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picked = self.current();
        let muted = cx.theme().foreground.opacity(0.6);
        let font_list = self.fonts.as_ref().map(|fonts| self.font_list(fonts, cx));
        let body = match self.tab {
            SettingsTab::General => self.general(picked, cx),
            SettingsTab::Appearance => self.appearance(picked, cx),
            SettingsTab::Keybindings => self.keybindings(picked, cx),
            SettingsTab::About => self.about(cx),
        };
        let hint = match self.tab {
            _ if font_list.is_some() => "Type to filter · ↑↓ pick · Enter uses it · Esc goes back",
            SettingsTab::Keybindings => {
                "↑↓ pick · Enter records a key · Backspace removes it · R resets · Ctrl+PgUp/PgDn tabs · Esc closes"
            }
            SettingsTab::About => "Ctrl+PgUp/PgDn tabs · Esc closes",
            SettingsTab::Appearance => {
                "↑↓ pick · Enter or ←→ changes · Backspace gives a font its default · Ctrl+PgUp/PgDn tabs · Esc closes"
            }
            _ => "↑↓ pick · Enter or ←→ changes · Ctrl+PgUp/PgDn tabs · Esc closes",
        };
        let view = cx.entity().downgrade();
        v_flex()
            // Not while a font list shows: its filter takes every key, R and Space too.
            .when(font_list.is_none(), |el| el.key_context(CONTEXT))
            .track_focus(&self.focus)
            .gap_2()
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step_row(-1, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step_row(1, cx)))
            .on_action(cx.listener(|this, _: &Left, window, cx| this.horizontal(-1., window, cx)))
            .on_action(cx.listener(|this, _: &Right, window, cx| this.horizontal(1., window, cx)))
            .on_action(cx.listener(|this, _: &Activate, window, cx| this.activate(window, cx)))
            .on_action(cx.listener(|this, _: &Clear, _, cx| this.clear_key(cx)))
            .on_action(cx.listener(|this, _: &Reset, _, cx| {
                if let Some(RowKind::Key(ix)) = this.current() {
                    this.reset_key(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.step_tab(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| this.step_tab(-1, cx)))
            .child(
                TabBar::new("settings-tabs")
                    .underline()
                    .selected_index(self.tab.index())
                    .on_click(move |ix: &usize, window, cx| {
                        let _ = view.update(cx, |v, cx| {
                            v.select_tab(SettingsTab::ALL[*ix], cx);
                            v.focus(window, cx);
                        });
                    })
                    .children(SettingsTab::ALL.iter().map(|t| Tab::new().label(t.title()))),
            )
            .map(|el| match font_list {
                Some(list) => el.child(list),
                None => el.child(
                    // The rows are its own children, so scrolling to a picked row finds it.
                    v_flex()
                        .id("settings-body")
                        .h(px(BODY_HEIGHT))
                        .gap_0p5()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .children(body),
                ),
            })
            .when_some(self.note.clone(), |el, note| {
                el.child(
                    h_flex().gap_1().text_xs().child(Icon::new(IconName::Info).xsmall().text_color(muted)).child(note),
                )
            })
            .child(div().text_xs().text_color(muted).child(hint))
    }
}
