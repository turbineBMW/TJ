//! Terminal spike: alacritty_terminal does the PTY and VT parsing, and this
//! view draws its grid with gpui text runs and turns key presses into bytes.

use std::{borrow::Cow, collections::HashMap, path::PathBuf, sync::Arc};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, Notify, OnResize, WindowSize},
    event_loop::{EventLoop, Notifier},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point},
    sync::FairMutex,
    term::{Config, Term, TermMode, cell::Flags, color::Colors},
    tty,
    vte::ansi::{Color, CursorShape, NamedColor, Rgb},
};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{prelude::FluentBuilder as _, *};

const FONT_SIZE: f32 = 14.0;
const KEY_CONTEXT: &str = "Terminal";

/// Keys the terminal must receive even though an outer view binds them.
///
/// gpui runs key bindings before key-down listeners, and gpui-kit's root view binds
/// Tab/Shift-Tab to focus traversal and Ctrl-C to Copy. Unbinding them in the
/// terminal's key context lets them fall through to [`TerminalView::on_key`].
const PASSTHROUGH_KEYS: [&str; 3] = ["tab", "shift-tab", "ctrl-c"];

/// Register the terminal's key bindings. Call once after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    cx.bind_keys(PASSTHROUGH_KEYS.map(|key| KeyBinding::new(key, NoAction, Some(KEY_CONTEXT))));
}
const LINE_HEIGHT: f32 = 1.3;

#[derive(Clone)]
struct Listener(smol::channel::Sender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.try_send(event);
    }
}

#[derive(Clone, Copy, PartialEq)]
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The shell ended; the terminal area closes its tab.
pub struct TerminalExited;

impl EventEmitter<TerminalExited> for TerminalView {}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    focus: FocusHandle,
    size: GridSize,
    cell: Size<Pixels>,
    title: Option<String>,
    exited: bool,
    /// The shell's process id, for reading its working directory.
    pid: Option<u32>,
    _pump: Task<()>,
    /// Bytes produced by key presses, so tests can check what the shell received.
    #[cfg(test)]
    keys_sent: Vec<u8>,
}

impl TerminalView {
    /// Start `shell` (the user's login shell when `None`) in a PTY and return the view that shows it.
    pub fn spawn(
        cwd: Option<PathBuf>,
        shell: Option<tty::Shell>,
        window: &mut Window,
        cx: &mut App,
    ) -> anyhow::Result<Entity<Self>> {
        let size = GridSize { cols: 80, rows: 24 };
        let cell = cell_size(window, cx);
        let (tx, rx) = smol::channel::unbounded();
        let listener = Listener(tx);

        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, listener.clone())));

        let mut env = HashMap::new();
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        env.insert("TERM_PROGRAM".into(), "tj".into());
        let options = tty::Options { shell, working_directory: cwd, drain_on_exit: false, env };
        let pty = tty::new(&options, window_size(size, cell), 0)?;
        let pid = Some(pty.child().id());

        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();

        Ok(cx.new(|cx| {
            // Drain terminal events on the UI thread; a burst of output becomes one repaint.
            let pump = cx.spawn(async move |this, cx| {
                while let Ok(first) = rx.recv().await {
                    let mut events = vec![first];
                    while let Ok(more) = rx.try_recv() {
                        events.push(more);
                    }
                    if this.update(cx, |this: &mut TerminalView, cx| this.handle_events(events, cx)).is_err() {
                        break;
                    }
                }
            });
            Self {
                term,
                notifier,
                focus: cx.focus_handle(),
                size,
                cell,
                title: None,
                exited: false,
                pid,
                _pump: pump,
                #[cfg(test)]
                keys_sent: Vec::new(),
            }
        }))
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The shell's current directory (Linux: `/proc/<pid>/cwd`).
    pub fn cwd(&self) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", self.pid?)).ok()
    }

    fn handle_events(&mut self, events: Vec<TermEvent>, cx: &mut Context<Self>) {
        for event in events {
            match event {
                TermEvent::Title(title) => self.title = Some(title),
                TermEvent::ResetTitle => self.title = None,
                TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
                TermEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                TermEvent::ClipboardLoad(_, format) => {
                    let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
                    self.write(format(&text).into_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    self.write(format(window_size(self.size, self.cell)).into_bytes())
                }
                TermEvent::ColorRequest(index, format) => {
                    let (ansi, _, _) = terminal_colors(cx);
                    let rgb = self.term.lock().colors()[index].unwrap_or_else(|| default_rgb(index, &ansi));
                    self.write(format(rgb).into_bytes());
                }
                TermEvent::Exit | TermEvent::ChildExit(_) => {
                    if !self.exited {
                        self.exited = true;
                        cx.emit(TerminalExited);
                    }
                }
                TermEvent::Wakeup | TermEvent::Bell | TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {
                }
            }
        }
        cx.notify();
    }

    /// Send text to the shell as if typed (used by the spike's `--type` hook).
    pub fn type_text(&self, text: &str) {
        self.write(text.as_bytes().to_vec());
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    fn resize(&mut self, size: GridSize, cx: &mut Context<Self>) {
        if size == self.size || size.cols < 2 || size.rows < 1 {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        self.notifier.on_resize(window_size(size, self.cell));
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mode = *self.term.lock().mode();
        let m = &ks.modifiers;

        // Ctrl+Shift+V pastes, honouring bracketed paste.
        if m.control && m.shift && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                if mode.contains(TermMode::BRACKETED_PASTE) {
                    self.write(format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes());
                } else {
                    self.write(text.replace("\r\n", "\r").replace('\n', "\r").into_bytes());
                }
            }
            cx.stop_propagation();
            return;
        }

        if let Some(bytes) = key_to_bytes(ks, mode) {
            self.term.lock().scroll_display(Scroll::Bottom);
            #[cfg(test)]
            self.keys_sent.extend_from_slice(&bytes);
            self.write(bytes);
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let lines = (event.delta.pixel_delta(self.cell.height).y / self.cell.height).round() as i32;
        if lines == 0 {
            return;
        }
        let mut term = self.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            // Full-screen apps (less, vim) get arrow keys instead of scrollback.
            let key: &[u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
            let bytes = key.repeat(lines.unsigned_abs() as usize);
            drop(term);
            self.write(bytes);
        } else {
            term.scroll_display(Scroll::Delta(lines));
            drop(term);
            cx.notify();
        }
    }
}

fn mono_font(cx: &App) -> Font {
    font(cx.theme().mono_font_family.clone())
}

fn cell_size(window: &mut Window, cx: &App) -> Size<Pixels> {
    let font_id = window.text_system().resolve_font(&mono_font(cx));
    let width =
        window.text_system().advance(font_id, px(FONT_SIZE), 'm').map(|s| s.width).unwrap_or(px(FONT_SIZE * 0.6));
    size(width, px((FONT_SIZE * LINE_HEIGHT).round()))
}

fn window_size(size: GridSize, cell: Size<Pixels>) -> WindowSize {
    WindowSize {
        num_lines: size.rows as u16,
        num_cols: size.cols as u16,
        cell_width: f32::from(cell.width) as u16,
        cell_height: f32::from(cell.height) as u16,
    }
}

/// Translate a gpui keystroke into the bytes a terminal expects.
fn key_to_bytes(ks: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let arrow = |c: char| -> Vec<u8> {
        let modifier = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.control as u8);
        if modifier > 1 {
            format!("\x1b[1;{modifier}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let bytes: Vec<u8> = match ks.key.as_str() {
        "enter" => b"\r".to_vec(),
        "backspace" if m.alt => b"\x1b\x7f".to_vec(),
        "backspace" if m.control => b"\x08".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => arrow('H'),
        "end" => arrow('F'),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        "insert" => b"\x1b[2~".to_vec(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" => b"\x1b[24~".to_vec(),
        key if m.control && !m.shift && key.len() == 1 => {
            let c = key.as_bytes()[0];
            let code = match c {
                b'a'..=b'z' => c - b'a' + 1,
                b'@' | b' ' | b'2' => 0,
                b'[' | b'3' => 27,
                b'\\' | b'4' => 28,
                b']' | b'5' => 29,
                b'^' | b'6' => 30,
                b'_' | b'7' | b'/' => 31,
                _ => return None,
            };
            let mut out = Vec::new();
            if m.alt {
                out.push(0x1b);
            }
            out.push(code);
            out
        }
        _ => {
            if m.platform || (m.control && m.shift) {
                return None;
            }
            let text = ks.key_char.as_deref().or(if ks.key == "space" { Some(" ") } else { None })?;
            let mut out = Vec::new();
            if m.alt {
                out.push(0x1b);
            }
            out.extend_from_slice(text.as_bytes());
            out
        }
    };
    Some(bytes)
}

// ---------- colours ----------

/// Tokyo Night-ish defaults for the 16 ANSI colours, used when no Omarchy theme is followed.
const ANSI: [u32; 16] = [
    0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6, //
    0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
];

/// The 16 ANSI colours and the default foreground and background: the Omarchy theme's when
/// TJ follows it ([`crate::omarchy::TerminalColors`]), else the built-in defaults and the UI theme.
fn terminal_colors(cx: &App) -> ([Rgb; 16], Hsla, Hsla) {
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches('#'), 16).ok();
    let rgb_of = |v: u32| Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 };
    match cx.try_global::<crate::omarchy::TerminalColors>() {
        Some(colors) => {
            let ansi = std::array::from_fn(|i| rgb_of(hex(&colors.ansi[i]).unwrap_or(ANSI[i])));
            let fg = hex(&colors.foreground).map(|v| rgb_to_hsla(rgb_of(v))).unwrap_or(cx.theme().foreground);
            let bg = hex(&colors.background).map(|v| rgb_to_hsla(rgb_of(v))).unwrap_or(cx.theme().background);
            (ansi, fg, bg)
        }
        None => (std::array::from_fn(|i| rgb_of(ANSI[i])), cx.theme().foreground, cx.theme().background),
    }
}

fn default_rgb(index: usize, ansi: &[Rgb; 16]) -> Rgb {
    let hex = match index {
        0..=15 => return ansi[index],
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { 55 + v as u32 * 40 };
            (level(i / 36) << 16) | (level((i / 6) % 6) << 8) | level(i % 6)
        }
        232..=255 => {
            let v = 8 + (index as u32 - 232) * 10;
            (v << 16) | (v << 8) | v
        }
        _ => 0xc0caf5,
    };
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

fn rgb_to_hsla(c: Rgb) -> Hsla {
    Rgba { r: c.r as f32 / 255.0, g: c.g as f32 / 255.0, b: c.b as f32 / 255.0, a: 1.0 }.into()
}

struct Palette<'a> {
    colors: &'a Colors,
    ansi: [Rgb; 16],
    fg: Hsla,
    bg: Hsla,
}

impl Palette<'_> {
    /// `None` means "the default background", which is left unpainted.
    fn resolve(&self, color: Color, flags: Flags, is_fg: bool) -> Option<Hsla> {
        let indexed = |i: usize| Some(rgb_to_hsla(self.colors[i].unwrap_or_else(|| default_rgb(i, &self.ansi))));
        match color {
            Color::Spec(rgb) => Some(rgb_to_hsla(rgb)),
            Color::Indexed(i) => {
                let i = i as usize;
                // Bold text in one of the first 8 colours brightens, as in most terminals.
                if is_fg && flags.contains(Flags::BOLD) && i < 8 { indexed(i + 8) } else { indexed(i) }
            }
            Color::Named(NamedColor::Foreground | NamedColor::BrightForeground) => Some(self.fg),
            Color::Named(NamedColor::Background) => {
                if is_fg {
                    Some(self.bg)
                } else {
                    None
                }
            }
            Color::Named(NamedColor::DimForeground) => Some(self.fg.opacity(0.7)),
            Color::Named(NamedColor::Cursor) => Some(self.fg),
            Color::Named(named) => {
                let i = named as usize;
                if i < 16 { indexed(i) } else { Some(self.fg) }
            }
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (ansi, fg, bg) = terminal_colors(cx);
        let base_font = mono_font(cx);
        let focused = self.focus.is_focused(window);

        let term = self.term.lock();
        let content = term.renderable_content();
        let palette = Palette { colors: content.colors, ansi, fg, bg };
        let offset = content.display_offset as i32;
        let cursor = content.cursor;

        let rows = self.size.rows;
        let mut lines: Vec<(String, Vec<TextRun>)> = (0..rows).map(|_| (String::new(), Vec::new())).collect();

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let cell = indexed.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let (text, runs) = &mut lines[row as usize];

            let mut cell_fg = palette.resolve(cell.fg, cell.flags, true).unwrap_or(fg);
            let mut cell_bg = palette.resolve(cell.bg, cell.flags, false);
            if cell.flags.contains(Flags::INVERSE) {
                let old_fg = cell_fg;
                cell_fg = cell_bg.unwrap_or(bg);
                cell_bg = Some(old_fg);
            }
            if cell.flags.contains(Flags::DIM) {
                cell_fg = cell_fg.opacity(0.7);
            }
            let is_cursor = focused
                && cursor.shape != CursorShape::Hidden
                && indexed.point == Point::new(Line(cursor.point.line.0), Column(cursor.point.column.0));
            if is_cursor {
                cell_bg = Some(fg);
                cell_fg = bg;
            }

            let start = text.len();
            let ch = if cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
            text.push(if ch == '\0' || ch == '\t' { ' ' } else { ch });
            if let Some(extra) = cell.zerowidth() {
                text.extend(extra.iter());
            }

            let mut run_font = base_font.clone();
            if cell.flags.contains(Flags::BOLD) {
                run_font.weight = FontWeight::BOLD;
            }
            if cell.flags.contains(Flags::ITALIC) {
                run_font.style = FontStyle::Italic;
            }
            let underline = cell.flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(cell_fg),
                wavy: cell.flags.contains(Flags::UNDERCURL),
            });
            let strikethrough = cell
                .flags
                .contains(Flags::STRIKEOUT)
                .then(|| StrikethroughStyle { thickness: px(1.0), color: Some(cell_fg) });

            let len = text.len() - start;
            match runs.last_mut() {
                Some(last)
                    if last.color == cell_fg
                        && last.background_color == cell_bg
                        && last.font == run_font
                        && last.underline == underline
                        && last.strikethrough == strikethrough =>
                {
                    last.len += len
                }
                _ => runs.push(TextRun {
                    len,
                    font: run_font,
                    color: cell_fg,
                    background_color: cell_bg,
                    underline,
                    strikethrough,
                }),
            }
        }
        drop(term);

        let cell = self.cell;
        let this = cx.entity();
        let measure = canvas(
            move |bounds, _window, cx| {
                let size = GridSize {
                    cols: (bounds.size.width / cell.width).floor() as usize,
                    rows: (bounds.size.height / cell.height).floor() as usize,
                };
                cx.defer(move |cx| this.update(cx, |t, cx| t.resize(size, cx)));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        div()
            .id("terminal")
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.focus.focus(window, cx)))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(bg)
            .p_2()
            .child(
                div()
                    .relative()
                    .size_full()
                    .font(base_font)
                    .text_size(px(FONT_SIZE))
                    .line_height(cell.height)
                    .children(lines.into_iter().map(|(text, runs)| {
                        div().h(cell.height).whitespace_nowrap().child(StyledText::new(text).with_runs(runs))
                    }))
                    .child(measure),
            )
            .when(self.exited, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .right_2()
                        .text_xs()
                        .text_color(fg.opacity(0.6))
                        .child("[process exited]"),
                )
            })
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{TermMode, TerminalView, key_to_bytes};
    use alacritty_terminal::tty::Shell;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, Keystroke, TestAppContext, WindowBounds, WindowOptions, bounds, point, px, size,
    };

    /// Open a real gpui-kit window (with its Root, which binds Tab and Ctrl-C) holding a
    /// terminal running /bin/sh, focus the terminal, press `keys`, and return what the
    /// key handler sent plus whether the terminal still has focus.
    fn press_in_terminal(cx: &mut TestAppContext, register_terminal_keys: bool, keys: &[&str]) -> (Vec<u8>, bool) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            if register_terminal_keys {
                super::init(cx);
            }
        });
        let (window, term): (_, Entity<TerminalView>) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds(point(px(0.), px(0.)), size(px(800.), px(500.))))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let shell = Some(Shell::new("/bin/sh".into(), vec![]));
                TerminalView::spawn(None, shell, window, cx).expect("spawn /bin/sh")
            })
            .expect("open test window")
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            term.update(cx, |t, cx| t.focus.focus(window, cx));
            window.render_frame(cx);
            for key in keys {
                window.press(key, cx);
            }
            let focused = term.read(cx).focus.is_focused(window);
            (term.read(cx).keys_sent.clone(), focused)
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn tab_and_ctrl_c_reach_the_shell(cx: &mut TestAppContext) {
        let (sent, focused) = press_in_terminal(cx, true, &["tab", "shift-tab", "ctrl-c"]);
        assert_eq!(sent, b"\t\x1b[Z\x03");
        assert!(focused, "Tab must not move focus out of the terminal");
    }

    /// Without `terminal::init`, gpui-kit's Root takes Tab for focus traversal. This shows
    /// the test above exercises the real conflict rather than passing by accident.
    /// (Root's Ctrl-C Copy lets the key through when nothing is selected, so it is
    /// unbound defensively rather than because this test can show it being lost.)
    #[gpui_kit::test]
    fn root_takes_tab_without_init(cx: &mut TestAppContext) {
        let (sent, _) = press_in_terminal(cx, false, &["tab"]);
        assert!(!sent.contains(&b'\t'), "expected Root to consume Tab, got {sent:?}");
    }

    fn bytes(keys: &'static str, mode: TermMode) -> Option<Vec<u8>> {
        let mut ks = Keystroke::parse(keys).unwrap();
        // parse() leaves key_char empty; a real printable keypress carries it.
        if ks.key.chars().count() == 1 && !ks.modifiers.control && !ks.modifiers.platform {
            ks.key_char = Some(if ks.modifiers.shift { ks.key.to_uppercase() } else { ks.key.clone() });
        }
        key_to_bytes(&ks, mode)
    }

    #[test]
    fn control_keys() {
        assert_eq!(bytes("ctrl-c", TermMode::empty()), Some(vec![3]));
        assert_eq!(bytes("ctrl-d", TermMode::empty()), Some(vec![4]));
        assert_eq!(bytes("ctrl-[", TermMode::empty()), Some(vec![27]));
        assert_eq!(bytes("ctrl-alt-b", TermMode::empty()), Some(vec![0x1b, 2]));
    }

    #[test]
    fn printable_and_alt() {
        assert_eq!(bytes("a", TermMode::empty()), Some(b"a".to_vec()));
        assert_eq!(bytes("alt-b", TermMode::empty()), Some(b"\x1bb".to_vec()));
        assert_eq!(bytes("space", TermMode::empty()), Some(b" ".to_vec()));
    }

    #[test]
    fn arrows_follow_cursor_mode() {
        assert_eq!(bytes("up", TermMode::empty()), Some(b"\x1b[A".to_vec()));
        assert_eq!(bytes("up", TermMode::APP_CURSOR), Some(b"\x1bOA".to_vec()));
        assert_eq!(bytes("ctrl-right", TermMode::empty()), Some(b"\x1b[1;5C".to_vec()));
    }

    #[test]
    fn editing_keys() {
        assert_eq!(bytes("enter", TermMode::empty()), Some(b"\r".to_vec()));
        assert_eq!(bytes("backspace", TermMode::empty()), Some(b"\x7f".to_vec()));
        assert_eq!(bytes("shift-tab", TermMode::empty()), Some(b"\x1b[Z".to_vec()));
        assert_eq!(bytes("delete", TermMode::empty()), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn app_shortcuts_are_not_sent() {
        assert_eq!(bytes("ctrl-shift-c", TermMode::empty()), None);
        assert_eq!(bytes("super-c", TermMode::empty()), None);
    }
}
