//! Terminal spike: alacritty_terminal does the PTY and VT parsing, and this
//! view draws its grid with gpui text runs and turns key presses into bytes.

use std::{borrow::Cow, collections::HashMap, ops::Range, path::PathBuf, sync::Arc};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, Notify, OnResize, WindowSize},
    event_loop::{EventLoop, Notifier},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, Term, TermMode, cell::Flags, color::Colors},
    tty,
    vte::ansi::{Color, CursorShape, CursorStyle, NamedColor, Rgb},
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

/// A grid cell showing part of an image.
#[derive(Clone, Copy, Debug)]
struct ImageCell {
    row: usize,
    col: usize,
    id: u16,
    image_row: u16,
    image_col: u16,
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
    /// The monospace family `cell` was measured in; another one (Settings) measures again.
    cell_font: SharedString,
    title: Option<String>,
    exited: bool,
    /// The shell's process id, for reading its working directory.
    pid: Option<u32>,
    _pump: Task<()>,
    /// Where the grid's top-left corner is in the window, for turning mouse positions into cells.
    origin: gpui_kit::Point<Pixels>,
    /// A drag is selecting text.
    selecting: bool,
    /// The button an app in mouse mode saw pressed (for drag motion and the release).
    reported_button: Option<u8>,
    /// The last cell reported to the app, so motion within one cell isn't repeated.
    last_reported: Option<(usize, usize)>,
    caret: crate::caret::Caret,
    /// The display scale the PTY's pixel sizes were reported at.
    scale: f32,
    /// What the image filter knows of the grid, and the images it decoded.
    geometry: crate::images::SharedGeometry,
    images: crate::images::SharedStore,
    /// Everything written to the shell, so tests can check what it received.
    #[cfg(test)]
    sent: std::cell::RefCell<Vec<u8>>,
    /// Tests that only check `sent` keep writes from the PTY: its echo would wake the view
    /// from the PTY's thread, which the test scheduler rejects.
    #[cfg(test)]
    muted: bool,
    /// How many image slices the last frame drew.
    #[cfg(test)]
    image_slices: usize,
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

        // Blinking unless an app asks for a steady cursor (DECSCUSR); `CSI 0 q` returns here.
        let config = Config {
            default_cursor_style: CursorStyle { shape: CursorShape::Block, blinking: true },
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));

        let mut env = HashMap::new();
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        env.insert("TERM_PROGRAM".into(), "tj".into());
        let options = tty::Options { shell, working_directory: cwd, drain_on_exit: false, env };
        let scale = window.scale_factor();
        let pty = tty::new(&options, window_size(size, cell, scale), 0)?;

        // Images come out of the output before alacritty sees it (see `images`).
        let geometry = Arc::new(std::sync::Mutex::new(crate::images::Geometry {
            cell_width: f32::from(cell.width) * scale,
            cell_height: f32::from(cell.height) * scale,
            scale,
            cols: size.cols as u16,
            rows: size.rows as u16,
        }));
        let images: crate::images::SharedStore = Default::default();
        let replies = listener.clone();
        let filter = crate::images::Filter::new(geometry.clone(), images.clone(), move |reply| {
            replies.send_event(TermEvent::PtyWrite(reply))
        });
        let pty = crate::images::FilteredPty::new(pty, filter)?;
        let pid = Some(pty.child_id());

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
                cell_font: cx.theme().mono_font_family.clone(),
                title: None,
                exited: false,
                pid,
                _pump: pump,
                origin: gpui_kit::Point::default(),
                selecting: false,
                reported_button: None,
                last_reported: None,
                caret: Default::default(),
                scale,
                geometry,
                images,
                #[cfg(test)]
                sent: Default::default(),
                #[cfg(test)]
                muted: false,
                #[cfg(test)]
                image_slices: 0,
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
                    self.write(format(window_size(self.size, self.cell, self.scale)).into_bytes())
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
        let bytes = bytes.into();
        #[cfg(test)]
        {
            self.sent.borrow_mut().extend_from_slice(&bytes);
            if self.muted {
                return;
            }
        }
        self.notifier.notify(bytes);
    }

    /// Another monospace font (Settings) or another display scale (the window moved screens)
    /// gives cells of another size: measure them, and tell apps the new pixel sizes. The grid
    /// gets its new columns from the next layout.
    fn sync_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scale = window.scale_factor();
        if cx.theme().mono_font_family == self.cell_font && scale == self.scale {
            return;
        }
        self.cell_font = cx.theme().mono_font_family.clone();
        self.scale = scale;
        self.cell = cell_size(window, cx);
        {
            let mut g = self.geometry.lock().unwrap();
            g.scale = scale;
            g.cell_width = f32::from(self.cell.width) * scale;
            g.cell_height = f32::from(self.cell.height) * scale;
        }
        self.notifier.on_resize(window_size(self.size, self.cell, scale));
    }

    fn resize(&mut self, size: GridSize, cx: &mut Context<Self>) {
        if size == self.size || size.cols < 2 || size.rows < 1 {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        self.notifier.on_resize(window_size(size, self.cell, self.scale));
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mode = *self.term.lock().mode();
        let m = &ks.modifiers;

        // Ctrl+Shift+V pastes, honouring bracketed paste.
        if m.control && m.shift && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                self.paste(&text);
            }
            cx.stop_propagation();
            return;
        }
        // Ctrl+Shift+C copies the selection.
        if m.control && m.shift && ks.key == "c" {
            if let Some(text) = self.selected_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.stop_propagation();
            return;
        }

        if let Some(bytes) = key_to_bytes(ks, mode) {
            // Typing goes to the prompt at the bottom; a selection there would be stale.
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
            drop(term);
            self.write(bytes);
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let lines = (event.delta.pixel_delta(self.cell.height).y / self.cell.height).round() as i32;
        if lines == 0 {
            return;
        }
        // An app in mouse mode gets the wheel as buttons 64 (up) and 65 (down).
        if self.app_wants_mouse(&event.modifiers) {
            let button = if lines > 0 { 64 } else { 65 };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(button, event.position, &event.modifiers, true);
            }
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

/// Modifier bits in a mouse report: Shift 4, Alt 8, Ctrl 16.
fn modifier_bits(modifiers: &Modifiers) -> u8 {
    (if modifiers.shift { 4 } else { 0 })
        | (if modifiers.alt { 8 } else { 0 })
        | (if modifiers.control { 16 } else { 0 })
}

/// The bytes that report a mouse event to an app in mouse mode, at a 0-based cell. SGR
/// (`CSI < b ; x ; y M/m`) when the app asked for it; otherwise the classic `CSI M b x y`,
/// where a release is button 3 and positions past 223 can't be sent (UTF-8 mode stretches
/// that to 2015).
fn mouse_report(code: u8, col: usize, row: usize, pressed: bool, mode: TermMode) -> Option<Vec<u8>> {
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, row + 1).into_bytes());
    }
    let code = if pressed { code } else { 3 | (code & 0b1_1100) };
    let values = [32 + code as usize, 33 + col, 33 + row];
    let mut bytes = b"\x1b[M".to_vec();
    for value in values {
        if mode.contains(TermMode::UTF8_MOUSE) {
            let c = char::from_u32(value as u32).filter(|_| value <= 2047)?;
            let mut buf = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        } else {
            bytes.push(u8::try_from(value).ok()?);
        }
    }
    Some(bytes)
}

fn mono_font(cx: &App) -> Font {
    font(cx.theme().mono_font_family.clone())
}

/// A cell is a whole number of device pixels each way, as in VTE and Alacritty, so every
/// glyph starts on a pixel and its stems stay sharp (a fractional width puts each letter at
/// another fraction of a pixel, and the rasterizer smears it across two).
fn cell_size(window: &mut Window, cx: &App) -> Size<Pixels> {
    let font_id = window.text_system().resolve_font(&mono_font(cx));
    let width =
        window.text_system().advance(font_id, px(FONT_SIZE), 'm').map(|s| s.width.into()).unwrap_or(FONT_SIZE * 0.6);
    let scale = window.scale_factor();
    let device = |v: f32| px((v * scale).round().max(1.0) / scale);
    size(device(width), device(FONT_SIZE * LINE_HEIGHT))
}

/// `v` moved to the nearest device pixel.
fn snap(v: Pixels, scale: f32) -> Pixels {
    px((f32::from(v) * scale).round() / scale)
}

fn snap_point(p: gpui_kit::Point<Pixels>, scale: f32) -> gpui_kit::Point<Pixels> {
    point(snap(p.x, scale), snap(p.y, scale))
}

/// The size the PTY reports to apps. Cells are in device pixels, so image tools send pictures
/// sharp enough for the screen (they're drawn at one image pixel per device pixel).
fn window_size(size: GridSize, cell: Size<Pixels>, scale: f32) -> WindowSize {
    WindowSize {
        num_lines: size.rows as u16,
        num_cols: size.cols as u16,
        cell_width: (f32::from(cell.width) * scale).round() as u16,
        cell_height: (f32::from(cell.height) * scale).round() as u16,
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

/// The built-in ANSI colours (Tokyo Night), for dark surfaces when the theme's own terminal
/// palette is made for a light background.
pub fn default_ansi() -> [Hsla; 16] {
    ANSI.map(|v| rgb_to_hsla(Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 }))
}

/// The terminal palette as UI colours: the 16 ANSI colours, foreground and background. Tool
/// cards use it so their colours follow the Omarchy theme like the terminal does.
pub fn palette(cx: &App) -> ([Hsla; 16], Hsla, Hsla) {
    let (ansi, fg, bg) = terminal_colors(cx);
    (ansi.map(rgb_to_hsla), fg, bg)
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

/// One row of the grid, ready to paint. Its text is shaped as a line, so the font's shaping
/// still applies, but each cell's glyphs are drawn at the cell's own column rather than where
/// the font's advances would put them: cells are whole device pixels, so every glyph lands on
/// a pixel (sharp, as in VTE), and wide characters keep the columns after them in place.
#[derive(Default)]
struct GridLine {
    text: String,
    /// The fonts to shape `text` in (bold and italic runs); their colours aren't used.
    runs: Vec<TextRun>,
    /// Each cell's first byte in `text`, its column and its colour, in order.
    cells: Vec<(usize, usize, Hsla)>,
    backgrounds: Vec<(Range<usize>, Hsla)>,
    /// Colour, and whether it's a curly underline.
    underlines: Vec<(Range<usize>, (Hsla, bool))>,
    strikethroughs: Vec<(Range<usize>, Hsla)>,
}

/// Where text sits in a cell, measured once for the monospace font.
#[derive(Clone, Copy)]
struct LineMetrics {
    ascent: Pixels,
    descent: Pixels,
}

impl LineMetrics {
    fn of(font: &Font, window: &Window) -> Self {
        let text = window.text_system();
        let id = text.resolve_font(font);
        // The font's descent is below the baseline, so negative; lines measure it as a distance.
        Self { ascent: text.ascent(id, px(FONT_SIZE)), descent: text.descent(id, px(FONT_SIZE)).abs() }
    }
}

/// Add `cols` to the last span when it carries on from it with the same value.
fn push_span<T: PartialEq>(spans: &mut Vec<(Range<usize>, T)>, cols: Range<usize>, value: T) {
    match spans.last_mut() {
        Some((last, v)) if last.end == cols.start && *v == value => last.end = cols.end,
        _ => spans.push((cols, value)),
    }
}

impl GridLine {
    fn push(&mut self, col: usize, ch: char, zerowidth: Option<&[char]>, color: Hsla, font: Font) {
        let start = self.text.len();
        self.text.push(ch);
        if let Some(extra) = zerowidth {
            self.text.extend(extra.iter());
        }
        self.cells.push((start, col, color));
        let len = self.text.len() - start;
        match self.runs.last_mut() {
            Some(last) if last.font == font => last.len += len,
            _ => self.runs.push(TextRun {
                len,
                font,
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }),
        }
    }

    /// Paint the row with its top-left corner at `origin`, which must be on a device pixel.
    fn paint(&self, origin: gpui_kit::Point<Pixels>, cell: Size<Pixels>, m: LineMetrics, window: &mut Window) {
        let span = |cols: &Range<usize>| (origin.x + cell.width * cols.start as f32, cell.width * cols.len() as f32);
        for (cols, color) in &self.backgrounds {
            let (x, width) = span(cols);
            window.paint_quad(fill(Bounds::new(point(x, origin.y), size(width, cell.height)), *color));
        }

        let scale = window.scale_factor();
        let baseline = snap(origin.y + (cell.height - m.ascent - m.descent) / 2. + m.ascent, scale);
        if !self.text.trim().is_empty() {
            let font_size = px(FONT_SIZE);
            let shaped = window.text_system().shape_line(self.text.clone().into(), font_size, &self.runs, None);
            // A cell's first glyph goes at its column; any more (combining marks) keep their
            // offset from it.
            let (mut current, mut first_x) = (usize::MAX, px(0.));
            for run in &shaped.runs {
                for glyph in &run.glyphs {
                    let ix = self.cells.partition_point(|c| c.0 <= glyph.index).saturating_sub(1);
                    let (_, col, color) = self.cells[ix];
                    if ix != current {
                        (current, first_x) = (ix, glyph.position.x);
                    }
                    let x = origin.x + cell.width * col as f32 + (glyph.position.x - first_x);
                    let at = point(x, baseline + glyph.position.y);
                    let _ = if glyph.is_emoji {
                        window.paint_emoji(at, run.font_id, glyph.id, font_size)
                    } else {
                        window.paint_glyph(at, run.font_id, glyph.id, font_size, color)
                    };
                }
            }
        }

        let padding = (cell.height - m.ascent - m.descent) / 2.;
        let underline_y = origin.y + padding + m.ascent + m.descent * 0.618;
        for (cols, (color, wavy)) in &self.underlines {
            let (x, width) = span(cols);
            let style = UnderlineStyle { thickness: px(1.0), color: Some(*color), wavy: *wavy };
            window.paint_underline(point(x, underline_y), width, &style);
        }
        for (cols, color) in &self.strikethroughs {
            let (x, width) = span(cols);
            let style = StrikethroughStyle { thickness: px(1.0), color: Some(*color) };
            window.paint_strikethrough(point(x, baseline - m.ascent * 0.3), width, &style);
        }
    }
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

impl TerminalView {
    // ---------- cursor ----------

    /// The cursor, painted over the grid by [`crate::caret`]: it glides between cells when a
    /// key moved it (the program's echo) and fades as it blinks. A block shows the character under it in the background
    /// colour once it has arrived; an unfocused terminal shows a hollow block.
    #[allow(clippy::too_many_arguments)]
    fn render_cursor(
        &mut self,
        cursor: alacritty_terminal::term::RenderableCursor,
        blinking: bool,
        under: Option<(char, bool)>,
        offset: i32,
        focused: bool,
        fg: Hsla,
        bg: Hsla,
        font: &Font,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let row = cursor.point.line.0 + offset;
        if cursor.shape == CursorShape::Hidden || row < 0 || row as usize >= self.size.rows {
            // Not drawn: when it shows again it appears there, rather than gliding from here.
            self.caret.reset();
            return Vec::new();
        }
        let (c, wide) = under.unwrap_or((' ', false));
        let cell = self.cell;
        let width = if wide { cell.width * 2.0 } else { cell.width };
        let (x, y) = (cell.width * cursor.point.column.0 as f32, cell.height * row as f32);
        if !focused || cursor.shape == CursorShape::HollowBlock {
            self.caret.reset();
            let hollow =
                div().absolute().left(x).top(y).w(width).h(cell.height).border_1().border_color(fg.opacity(0.8));
            return vec![hollow.into_any_element()];
        }
        // Grid-relative, so a pane resize doesn't send the cursor gliding.
        let target = match cursor.shape {
            CursorShape::Beam => Bounds::new(point(x, y), size(px(2.), cell.height)),
            CursorShape::Underline => Bounds::new(point(x, y + cell.height - px(2.)), size(width, px(2.))),
            _ => Bounds::new(point(x, y), size(width, cell.height)),
        };
        let (rows, cols) = (self.size.rows as f32, self.size.cols as f32);
        let aim = crate::caret::Aim {
            target,
            // A resize or scrolling back moves the cursor without a glide.
            view: crate::caret::fingerprint(&[cell.width.into(), cell.height.into(), rows, cols, offset as f32]),
            viewport: Bounds::new(point(px(0.), px(0.)), size(cell.width * cols, cell.height * rows)),
            echo: crate::caret::TERMINAL_ECHO,
            blinking,
        };
        let look = self.caret.look(aim, window, cx);
        let alpha = look.opacity;
        if alpha <= 0.0 {
            return Vec::new();
        }
        let ink = fg.opacity(alpha);
        let quad = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let origin = snap_point(bounds.origin, window.scale_factor());
                let at = Bounds::new(look.bounds.origin + origin, look.bounds.size);
                crate::caret::paint(at, ink, window)
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let mut out = vec![quad.into_any_element()];
        let block = !matches!(cursor.shape, CursorShape::Beam | CursorShape::Underline);
        if block && !look.gliding && c != ' ' && c != '\0' {
            let mut line = GridLine::default();
            line.push(cursor.point.column.0, c, None, bg.opacity(alpha), font.clone());
            let metrics = LineMetrics::of(font, window);
            let char_under = canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let origin = snap_point(bounds.origin, window.scale_factor());
                    line.paint(point(origin.x, origin.y + y), cell, metrics, window);
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full();
            out.push(char_under.into_any_element());
        }
        out
    }

    // ---------- images ----------

    /// Each run of placeholder cells (side by side, same image and row, in order) becomes
    /// that slice of its image: the image drawn at its place on the grid, clipped to the run.
    fn render_images(&self, cells: &[ImageCell]) -> Vec<AnyElement> {
        if cells.is_empty() {
            return Vec::new();
        }
        let store = self.images.lock().unwrap();
        let cell = self.cell;
        let mut out = Vec::new();
        let mut i = 0;
        while i < cells.len() {
            let first = cells[i];
            let mut n = 1;
            while let Some(next) = cells.get(i + n) {
                let follows = next.row == first.row
                    && next.col == first.col + n
                    && next.id == first.id
                    && next.image_row == first.image_row
                    && next.image_col as usize == first.image_col as usize + n;
                if !follows {
                    break;
                }
                n += 1;
            }
            i += n;
            let Some(placement) = store.placement(first.id) else { continue };
            let (bw, bh) = (cell.width * placement.cols as f32, cell.height * placement.rows as f32);
            let natural =
                size(px(placement.picture.width as f32 / self.scale), px(placement.picture.height as f32 / self.scale));
            let shown = match placement.fit {
                crate::images::Fit::Natural => natural,
                crate::images::Fit::Stretch => size(bw, bh),
                crate::images::Fit::Contain => {
                    let k = (bw / natural.width).min(bh / natural.height);
                    size(natural.width * k, natural.height * k)
                }
            };
            out.push(
                div()
                    .absolute()
                    .left(cell.width * first.col as f32)
                    .top(cell.height * first.row as f32)
                    .w(cell.width * n as f32)
                    .h(cell.height)
                    .overflow_hidden()
                    .child(
                        img(gpui_kit::ImageSource::Render(placement.picture.image.clone()))
                            .absolute()
                            .left(-(cell.width * first.image_col as f32))
                            .top(-(cell.height * first.image_row as f32))
                            .w(shown.width)
                            .h(shown.height),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    // ---------- mouse ----------

    /// The grid cell under a window position, clamped to the grid, and which half of it.
    fn cell_at(&self, position: gpui_kit::Point<Pixels>) -> (usize, usize, Side) {
        let x = (position.x - self.origin.x) / self.cell.width;
        let y = (position.y - self.origin.y) / self.cell.height;
        let col = (x.max(0.0).floor() as usize).min(self.size.cols.saturating_sub(1));
        let row = (y.max(0.0).floor() as usize).min(self.size.rows.saturating_sub(1));
        let side = if x < 0.0 {
            Side::Left
        } else if x >= self.size.cols as f32 || x - x.floor() >= 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        (col, row, side)
    }

    /// Whether the app asked for mouse events. Shift keeps the mouse for selecting, as in
    /// other terminals.
    fn app_wants_mouse(&self, modifiers: &Modifiers) -> bool {
        self.term.lock().mode().intersects(TermMode::MOUSE_MODE) && !modifiers.shift
    }

    fn report_mouse(&mut self, button: u8, position: gpui_kit::Point<Pixels>, modifiers: &Modifiers, pressed: bool) {
        let (col, row, _) = self.cell_at(position);
        let mode = *self.term.lock().mode();
        let code = button | modifier_bits(modifiers);
        if let Some(bytes) = mouse_report(code, col, row, pressed, mode) {
            self.write(bytes);
        }
        self.last_reported = Some((col, row));
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let button = match event.button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        if self.app_wants_mouse(&event.modifiers) {
            self.report_mouse(button, event.position, &event.modifiers, true);
            self.reported_button = Some(button);
            cx.stop_propagation();
            return;
        }
        match event.button {
            // One click starts a selection; two select a word, three a line.
            MouseButton::Left => {
                let (col, row, side) = self.cell_at(event.position);
                let kind = match event.click_count {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                let mut term = self.term.lock();
                let point = Point::new(Line(row as i32 - term.grid().display_offset() as i32), Column(col));
                term.selection = Some(Selection::new(kind, point, side));
                drop(term);
                self.selecting = true;
                if event.click_count > 1 {
                    self.copy_selection_to_primary(cx);
                }
                cx.notify();
            }
            // Middle click pastes the primary selection, as in other Linux terminals.
            MouseButton::Middle => {
                if let Some(text) = cx.read_from_primary().and_then(|c| c.text()) {
                    self.paste(&text);
                }
            }
            _ => {}
        }
    }

    /// Mouse moves anywhere in the window (a drag can leave the terminal).
    fn on_window_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.selecting {
            if event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let (col, row, side) = self.cell_at(event.position);
            let mut term = self.term.lock();
            let point = Point::new(Line(row as i32 - term.grid().display_offset() as i32), Column(col));
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point, side);
            }
            drop(term);
            cx.notify();
            return;
        }
        let mode = *self.term.lock().mode();
        let reporting = match self.reported_button {
            Some(_) => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
            None => mode.contains(TermMode::MOUSE_MOTION) && self.contains(event.position),
        };
        let (col, row, _) = self.cell_at(event.position);
        if reporting && self.last_reported != Some((col, row)) {
            // Motion is reported as button + 32; with no button held, as "button 3".
            let button = self.reported_button.unwrap_or(3) + 32;
            self.report_mouse(button, event.position, &event.modifiers, true);
        }
    }

    fn on_window_mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.selecting && event.button == MouseButton::Left {
            self.selecting = false;
            self.copy_selection_to_primary(cx);
        }
        if let Some(button) = self.reported_button.take() {
            self.report_mouse(button, event.position, &event.modifiers, false);
        }
    }

    fn contains(&self, position: gpui_kit::Point<Pixels>) -> bool {
        let size = size(self.cell.width * self.size.cols as f32, self.cell.height * self.size.rows as f32);
        Bounds::new(self.origin, size).contains(&position)
    }

    /// Linux terminals put whatever is selected in the primary selection.
    fn copy_selection_to_primary(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().filter(|t| !t.is_empty())
    }

    fn paste(&self, text: &str) {
        let mode = *self.term.lock().mode();
        if mode.contains(TermMode::BRACKETED_PASTE) {
            self.write(format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes());
        } else {
            self.write(text.replace("\r\n", "\r").replace('\n', "\r").into_bytes());
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_cell(window, cx);
        let (ansi, fg, bg) = terminal_colors(cx);
        let base_font = mono_font(cx);
        let focused = self.focus.is_focused(window);

        let term = self.term.lock();
        let content = term.renderable_content();
        let palette = Palette { colors: content.colors, ansi, fg, bg };
        let offset = content.display_offset as i32;
        let cursor = content.cursor;
        let selection = content.selection;
        let cursor_style = term.cursor_style();
        let mut cursor_cell: Option<(char, bool)> = None;
        let selection_bg = cx.theme().selection;
        let metrics = LineMetrics::of(&base_font, window);

        let rows = self.size.rows;
        let mut lines: Vec<GridLine> = (0..rows).map(|_| GridLine::default()).collect();
        let mut image_cells: Vec<ImageCell> = Vec::new();

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let cell = indexed.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let col = indexed.point.column.0;
            let line = &mut lines[row as usize];

            // An image's cell: drawn below as part of its image, so no text or background.
            if let Some((id, image_row, image_col)) = crate::images::decode_cell(cell.c, cell.fg, cell.bg) {
                image_cells.push(ImageCell { row: row as usize, col, id, image_row, image_col });
                line.push(col, ' ', None, fg, base_font.clone());
                continue;
            }

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
            if selection.is_some_and(|s| s.contains(indexed.point)) {
                cell_bg = Some(selection_bg);
            }
            if indexed.point == cursor.point {
                cursor_cell = Some((cell.c, cell.flags.contains(Flags::WIDE_CHAR)));
            }

            let cols = col..col + if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            if let Some(color) = cell_bg {
                push_span(&mut line.backgrounds, cols.clone(), color);
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                push_span(&mut line.underlines, cols.clone(), (cell_fg, cell.flags.contains(Flags::UNDERCURL)));
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                push_span(&mut line.strikethroughs, cols.clone(), cell_fg);
            }

            let ch = if cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
            let ch = if ch == '\0' || ch == '\t' { ' ' } else { ch };
            let zerowidth = cell.zerowidth().filter(|_| ch != ' ');
            let mut run_font = base_font.clone();
            if cell.flags.contains(Flags::BOLD) {
                run_font.weight = FontWeight::BOLD;
            }
            if cell.flags.contains(Flags::ITALIC) {
                run_font.style = FontStyle::Italic;
            }
            line.push(col, ch, zerowidth, cell_fg, run_font);
        }
        drop(term);

        let image_slices = self.render_images(&image_cells);
        #[cfg(test)]
        {
            self.image_slices = image_slices.len();
        }
        let cursor_overlay = self.render_cursor(
            cursor,
            cursor_style.blinking,
            cursor_cell,
            offset,
            focused,
            fg,
            bg,
            &base_font,
            window,
            cx,
        );
        let cell = self.cell;
        let this = cx.entity();
        let painted = this.clone();
        let measure = canvas(
            move |bounds, _window, cx| {
                let size = GridSize {
                    cols: (bounds.size.width / cell.width).floor() as usize,
                    rows: (bounds.size.height / cell.height).floor() as usize,
                };
                cx.defer(move |cx| {
                    this.update(cx, |t, cx| {
                        t.origin = bounds.origin;
                        t.resize(size, cx)
                    })
                });
            },
            // Moves and releases anywhere in the window: a drag may leave the terminal.
            move |_, _, window, _| {
                let (moved, released) = (painted.clone(), painted.clone());
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        moved.update(cx, |t, cx| t.on_window_mouse_move(event, cx));
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        released.update(cx, |t, cx| t.on_window_mouse_up(event, cx));
                    }
                });
            },
        )
        // Pinned to the grid's top-left: an absolute element without an inset is placed after
        // the lines, and its origin is where mouse positions are measured from.
        .absolute()
        .top_0()
        .left_0()
        .size_full();

        div()
            .id("terminal")
            .track_focus(&self.focus)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .relative()
            .size_full()
            .overflow_hidden()
            .rounded_t(px(crate::panes::CONTENT_RADIUS))
            .bg(bg)
            .p_2()
            .child(
                div()
                    .relative()
                    .size_full()
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                let origin = snap_point(bounds.origin, window.scale_factor());
                                for (row, line) in lines.iter().enumerate() {
                                    let top = point(origin.x, origin.y + cell.height * row as f32);
                                    line.paint(top, cell, metrics, window);
                                }
                            },
                        )
                        .size_full(),
                    )
                    .children(image_slices)
                    .children(cursor_overlay)
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
    use super::{TermMode, TerminalView, key_to_bytes, mouse_report};
    use alacritty_terminal::tty::Shell;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Entity, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
        TestAppContext, WindowBounds, WindowOptions, bounds, point, px, size,
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
            (term.read(cx).sent.borrow().clone(), focused)
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

    #[test]
    fn mouse_reports_in_sgr_and_classic_form() {
        assert_eq!(mouse_report(0, 2, 1, true, TermMode::SGR_MOUSE), Some(b"\x1b[<0;3;2M".to_vec()));
        assert_eq!(mouse_report(0, 2, 1, false, TermMode::SGR_MOUSE), Some(b"\x1b[<0;3;2m".to_vec()));
        assert_eq!(mouse_report(64, 0, 0, true, TermMode::SGR_MOUSE), Some(b"\x1b[<64;1;1M".to_vec()));
        // Classic: 32 + button, 33 + column, 33 + row; a release is button 3 (modifiers kept).
        assert_eq!(mouse_report(0, 2, 1, true, TermMode::empty()), Some(vec![0x1b, b'[', b'M', 32, 35, 34]));
        assert_eq!(mouse_report(16, 0, 0, false, TermMode::empty()), Some(vec![0x1b, b'[', b'M', 32 + 19, 33, 33]));
        assert_eq!(mouse_report(0, 300, 0, true, TermMode::empty()), None, "past column 223");
        let utf8 = mouse_report(0, 300, 0, true, TermMode::UTF8_MOUSE).unwrap();
        assert_eq!(String::from_utf8(utf8[3..].to_vec()).unwrap().chars().nth(1), Some(char::from_u32(333).unwrap()));
    }

    #[gpui_kit::test]
    fn an_image_is_drawn_one_slice_per_row(cx: &mut TestAppContext) {
        let (term, mut cx) = terminal_showing(cx, b"");
        let cx = &mut cx;
        term.update(cx, |t, cx| {
            // A 3x2-cell image through the terminal's own filter, as the PTY would deliver it.
            let mut png = Vec::new();
            image::RgbaImage::from_pixel(20, 30, image::Rgba([1, 2, 3, 255]))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            use base64::Engine as _;
            let sequence =
                format!("\x1b_Ga=T,f=100,c=3,r=2;{}\x1b\\", base64::engine::general_purpose::STANDARD.encode(png));
            let mut filter = crate::images::Filter::new(t.geometry.clone(), t.images.clone(), |_| {});
            let mut out = Vec::new();
            filter.feed(sequence.as_bytes(), &mut out);
            let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
            parser.advance(&mut *t.term.lock(), &out);
            cx.notify();
        });
        cx.update(|window, cx| window.render_frame(cx));
        assert_eq!(term.read_with(cx, |t, _| t.image_slices), 2, "one run of three cells per row");
    }

    #[gpui_kit::test]
    fn apps_can_ask_for_a_steady_cursor(cx: &mut TestAppContext) {
        let (term, mut cx) = terminal_showing(cx, b"");
        let blinking = |term: &Entity<TerminalView>, cx: &mut gpui_kit::VisualTestContext| {
            term.read_with(cx, |t, _| t.term.lock().cursor_style().blinking)
        };
        assert!(blinking(&term, &mut cx), "blinking by default");
        let feed = |bytes: &[u8], cx: &mut gpui_kit::VisualTestContext| {
            term.update(cx, |t, _| {
                let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
                parser.advance(&mut *t.term.lock(), bytes);
            })
        };
        feed(b"\x1b[2 q", &mut cx); // steady block, as vi-mode prompts send
        assert!(!blinking(&term, &mut cx));
        feed(b"\x1b[0 q", &mut cx); // back to the default
        assert!(blinking(&term, &mut cx));
        cx.update(|window, cx| window.render_frame(cx));
    }

    /// A terminal running `sleep` (so no prompt arrives) showing `screen`, rendered once.
    fn terminal_showing(cx: &mut TestAppContext, screen: &[u8]) -> (Entity<TerminalView>, gpui_kit::VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::init(cx);
        });
        let (window, term): (_, Entity<TerminalView>) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds(point(px(0.), px(0.)), size(px(800.), px(500.))))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                let shell = Some(Shell::new("sleep".into(), vec!["30".into()]));
                TerminalView::spawn(None, shell, window, cx).expect("spawn sleep")
            })
            .expect("open test window")
        });
        let mut cx = gpui_kit::VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| window.render_frame(cx));
        cx.run_until_parked();
        term.update(&mut cx, |t, cx| {
            t.muted = true;
            let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
            parser.advance(&mut *t.term.lock(), screen);
            cx.notify();
        });
        cx.update(|window, cx| window.render_frame(cx));
        // Mouse positions are measured from the grid's top-left (inside the 8px padding).
        let origin = term.read_with(&cx, |t, _| t.origin);
        assert!(origin.y < px(20.) && origin.x < px(20.), "grid origin {origin:?}");
        (term, cx)
    }

    /// The window position of a cell, a quarter or three quarters across it.
    fn at(
        term: &Entity<TerminalView>,
        cx: &mut gpui_kit::VisualTestContext,
        col: usize,
        row: usize,
        right: bool,
    ) -> gpui_kit::Point<gpui_kit::Pixels> {
        term.read_with(cx, |t, _| {
            let x = t.origin.x + t.cell.width * (col as f32 + if right { 0.75 } else { 0.25 });
            point(x, t.origin.y + t.cell.height * (row as f32 + 0.5))
        })
    }

    fn mouse(
        button: MouseButton,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        clicks: usize,
        shift: bool,
    ) -> MouseDownEvent {
        let modifiers = Modifiers { shift, ..Default::default() };
        MouseDownEvent { button, position, modifiers, click_count: clicks, first_mouse: false }
    }

    fn up(position: gpui_kit::Point<gpui_kit::Pixels>) -> MouseUpEvent {
        MouseUpEvent { button: MouseButton::Left, position, modifiers: Modifiers::default(), click_count: 1 }
    }

    #[gpui_kit::test]
    fn dragging_selects_text_into_the_primary_selection(cx: &mut TestAppContext) {
        let (term, mut cx) = terminal_showing(cx, b"hello world\r\nsecond line");
        let cx = &mut cx;
        let (start, end) = (at(&term, cx, 0, 0, false), at(&term, cx, 4, 0, true));
        cx.simulate_event(mouse(MouseButton::Left, start, 1, false));
        cx.simulate_event(MouseMoveEvent {
            position: end,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        });
        cx.simulate_event(up(end));
        assert_eq!(term.read_with(cx, |t, _| t.selected_text()).as_deref(), Some("hello"));
        assert_eq!(cx.update(|_, cx| cx.read_from_primary().and_then(|c| c.text())).as_deref(), Some("hello"));

        // Ctrl+Shift+C copies it to the clipboard; typing clears it.
        cx.update(|window, cx| window.render_frame(cx));
        cx.simulate_keystrokes("ctrl-shift-c");
        assert_eq!(cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).as_deref(), Some("hello"));
        cx.simulate_keystrokes("x");
        assert_eq!(term.read_with(cx, |t, _| t.selected_text()), None);
    }

    #[gpui_kit::test]
    fn double_and_triple_clicks_select_a_word_and_a_line(cx: &mut TestAppContext) {
        let (term, mut cx) = terminal_showing(cx, b"hello world\r\nsecond line");
        let cx = &mut cx;
        let word = at(&term, cx, 8, 0, false);
        cx.simulate_event(mouse(MouseButton::Left, word, 2, false));
        cx.simulate_event(up(word));
        assert_eq!(term.read_with(cx, |t, _| t.selected_text()).as_deref(), Some("world"));
        let line = at(&term, cx, 2, 1, false);
        cx.simulate_event(mouse(MouseButton::Left, line, 3, false));
        cx.simulate_event(up(line));
        assert_eq!(term.read_with(cx, |t, _| t.selected_text()).as_deref().map(str::trim_end), Some("second line"));
    }

    #[gpui_kit::test]
    fn an_app_in_mouse_mode_gets_clicks_unless_shift_is_held(cx: &mut TestAppContext) {
        // What vim or htop sends to turn on click reporting in SGR form.
        let (term, mut cx) = terminal_showing(cx, b"\x1b[?1000h\x1b[?1006hmenu item");
        let cx = &mut cx;
        let spot = at(&term, cx, 2, 0, false);
        cx.simulate_event(mouse(MouseButton::Left, spot, 1, false));
        cx.simulate_event(up(spot));
        let sent = term.read_with(cx, |t, _| t.sent.borrow().clone());
        assert_eq!(String::from_utf8_lossy(&sent), "\x1b[<0;3;1M\x1b[<0;3;1m");
        assert_eq!(term.read_with(cx, |t, _| t.selected_text()), None);

        term.read_with(cx, |t, _| t.sent.borrow_mut().clear());
        let end = at(&term, cx, 3, 0, true);
        let start = at(&term, cx, 0, 0, false);
        cx.simulate_event(mouse(MouseButton::Left, start, 1, true));
        cx.simulate_event(MouseMoveEvent {
            position: end,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers { shift: true, ..Default::default() },
        });
        cx.simulate_event(up(end));
        assert_eq!(term.read_with(cx, |t, _| t.selected_text()).as_deref(), Some("menu"), "Shift selects locally");
        assert!(term.read_with(cx, |t, _| t.sent.borrow().is_empty()), "and nothing is reported");
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
