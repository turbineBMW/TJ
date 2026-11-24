//! Inline images in the terminal: the kitty graphics protocol, SIXEL and iTerm2's OSC 1337.
//!
//! alacritty_terminal drops these sequences, so the PTY's output passes through [`Filter`]
//! first. It takes the image sequences out, decodes them, and writes *placeholder cells* in
//! their place: a private-use character whose colours say which image, and which row and
//! column of it, belongs there (kitty's "Unicode placeholder" idea). The grid then carries
//! the images like text: they scroll, are cleared, and leave with the alternate screen
//! without any bookkeeping, and [`crate::terminal`] draws each run of placeholder cells as
//! the matching slice of its image. Text written over an image replaces those cells, so that
//! part of the image goes, as in most terminals.
//!
//! The filter also answers what apps ask before sending images: the kitty support query,
//! the primary device attributes (with SIXEL) and the cell size in pixels.

mod iterm;
pub mod kitty;
mod sixel;

use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, Read},
    sync::{Arc, Mutex},
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{ChildEvent, EventedPty, EventedReadWrite, Pty},
    vte::ansi::{Color, Rgb},
};
use gpui_kit::RenderImage;
use polling::{Event, PollMode, Poller};

/// The character a placeholder cell holds (kitty uses the same one).
pub const PLACEHOLDER: char = '\u{10EEEE}';
/// The red of a placeholder cell's background, so stray uses of the character don't match.
const MARK: u8 = 0xA5;
/// Rows are kept in one byte of the foreground colour.
const MAX_ROWS: u16 = 255;
/// Placements kept; the oldest go first (their cells then show nothing).
const MAX_PLACEMENTS: usize = 512;
/// kitty images kept for later placement.
const MAX_KITTY_IMAGES: usize = 64;
/// An image sequence longer than this is dropped rather than held in memory.
const MAX_SEQUENCE: usize = 256 << 20;
/// The largest image accepted, in pixels on a side.
pub const MAX_SIDE: u32 = 10_000;

/// The terminal's size as the filter needs it: cells in device pixels (what the PTY reports
/// to apps), the display scale, and the grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub cell_width: f32,
    pub cell_height: f32,
    pub scale: f32,
    pub cols: u16,
    pub rows: u16,
}

impl Default for Geometry {
    fn default() -> Self {
        Self { cell_width: 8.0, cell_height: 18.0, scale: 1.0, cols: 80, rows: 24 }
    }
}

pub type SharedGeometry = Arc<Mutex<Geometry>>;

/// A decoded image, ready to draw.
pub struct Picture {
    pub image: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
}

impl Picture {
    /// gpui draws BGRA; the decoders produce RGBA.
    pub fn new(rgba: image::RgbaImage) -> Arc<Self> {
        let (width, height) = rgba.dimensions();
        let mut bgra = rgba;
        for pixel in bgra.pixels_mut() {
            pixel.0.swap(0, 2);
        }
        let frame = image::Frame::new(bgra);
        Arc::new(Self { image: Arc::new(RenderImage::new(vec![frame])), width, height })
    }
}

/// How an image fills the cells it was given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fit {
    /// Its own size (one image pixel per device pixel); the cells were rounded up around it.
    Natural,
    /// As large as fits in the cells, keeping its shape.
    Contain,
    /// Exactly the cells.
    Stretch,
}

/// One image placed on the grid.
pub struct Placement {
    pub picture: Arc<Picture>,
    pub cols: u16,
    pub rows: u16,
    pub fit: Fit,
    /// The kitty image it shows, for kitty's delete commands.
    kitty: Option<u32>,
}

/// The images and placements of one terminal, shared by its PTY reader and its view.
#[derive(Default)]
pub struct Store {
    placements: HashMap<u16, Placement>,
    order: VecDeque<u16>,
    next: u16,
    kitty: HashMap<u32, Arc<Picture>>,
    kitty_order: VecDeque<u32>,
    /// kitty image numbers (`I=`) to the ids TJ gave them.
    numbers: HashMap<u32, u32>,
    next_kitty: u32,
}

pub type SharedStore = Arc<Mutex<Store>>;

impl Store {
    pub fn placement(&self, id: u16) -> Option<&Placement> {
        self.placements.get(&id)
    }

    fn place(&mut self, placement: Placement) -> u16 {
        loop {
            self.next = self.next.wrapping_add(1);
            if self.next != 0 && !self.placements.contains_key(&self.next) {
                break;
            }
        }
        let id = self.next;
        self.placements.insert(id, placement);
        self.order.push_back(id);
        while self.order.len() > MAX_PLACEMENTS {
            if let Some(old) = self.order.pop_front() {
                self.placements.remove(&old);
            }
        }
        id
    }

    fn keep_kitty(&mut self, id: u32, picture: Arc<Picture>) {
        if self.kitty.insert(id, picture).is_none() {
            self.kitty_order.push_back(id);
        }
        while self.kitty_order.len() > MAX_KITTY_IMAGES {
            if let Some(old) = self.kitty_order.pop_front() {
                self.kitty.remove(&old);
            }
        }
    }

    /// A fresh id for a kitty image that came with only a number (or nothing).
    fn new_kitty_id(&mut self) -> u32 {
        self.next_kitty = self.next_kitty.wrapping_add(1).max(1);
        // Ids clients choose are usually small; count down from the top to stay clear.
        u32::MAX - self.next_kitty
    }

    /// kitty's delete: the placements of one image (or of every kitty image), and with
    /// `free` the image data too.
    fn delete_kitty(&mut self, image: Option<u32>, free: bool) {
        let gone: Vec<u16> = self
            .placements
            .iter()
            .filter(|(_, p)| p.kitty.is_some() && (image.is_none() || p.kitty == image))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.placements.remove(&id);
        }
        self.order.retain(|id| self.placements.contains_key(id));
        if free {
            match image {
                Some(id) => {
                    self.kitty.remove(&id);
                }
                None => self.kitty.clear(),
            }
            self.kitty_order.retain(|id| self.kitty.contains_key(id));
        }
    }
}

/// The cells an image covers, and how it fills them. `cols`/`rows` are what the app asked
/// for; with one of them, the other keeps the image's shape.
pub fn cells(width: u32, height: u32, cols: Option<u16>, rows: Option<u16>, g: &Geometry) -> (u16, u16, Fit) {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let clamp = |n: f32| (n.ceil() as u32).clamp(1, MAX_ROWS as u32) as u16;
    let clamp_cols = |n: f32| (n.ceil() as u32).clamp(1, u16::MAX as u32) as u16;
    match (cols.filter(|c| *c > 0), rows.filter(|r| *r > 0)) {
        (Some(c), Some(r)) => (c, r.min(MAX_ROWS), Fit::Stretch),
        (Some(c), None) => (c, clamp(c as f32 * g.cell_width * h / w / g.cell_height), Fit::Contain),
        (None, Some(r)) => (clamp_cols(r as f32 * g.cell_height * w / h / g.cell_width), r.min(MAX_ROWS), Fit::Contain),
        (None, None) => (clamp_cols(w / g.cell_width), clamp(h / g.cell_height), Fit::Natural),
    }
}

/// Where the cursor goes once an image is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum After {
    /// To the right of the image, on its last row (kitty's default).
    Right,
    /// To the line below it, in its first column (SIXEL, iTerm2).
    Below,
    /// Back where it started.
    Stay,
}

/// The bytes that fill `cols` x `rows` cells from the cursor with placement `id`. The app's
/// colours and the cursor are saved and restored around each row, and autowrap is off while
/// they're written, so an image past the right edge is cut off rather than wrapped.
pub fn placeholder_bytes(id: u16, cols: u16, rows: u16, after: After) -> Vec<u8> {
    let mut s = String::from("\x1b[?7l");
    for row in 0..rows.min(MAX_ROWS) {
        s.push_str("\x1b7");
        s.push_str(&format!("\x1b[38;2;{};{};{}m", id >> 8, id & 0xff, row));
        for col in 0..cols {
            s.push_str(&format!("\x1b[48;2;{MARK};{};{}m{PLACEHOLDER}", col >> 8, col & 0xff));
        }
        s.push_str("\x1b8");
        if row + 1 < rows {
            s.push_str("\x1bD");
        }
    }
    match after {
        After::Right => s.push_str(&format!("\x1b[{cols}C")),
        After::Below => s.push_str("\x1bD"),
        After::Stay if rows > 1 => s.push_str(&format!("\x1b[{}A", rows - 1)),
        After::Stay => {}
    }
    s.push_str("\x1b[?7h");
    s.into_bytes()
}

/// Which placement, and which row and column of it, a cell shows, if it's a placeholder.
pub fn decode_cell(c: char, fg: Color, bg: Color) -> Option<(u16, u16, u16)> {
    if c != PLACEHOLDER {
        return None;
    }
    match (fg, bg) {
        (Color::Spec(Rgb { r, g, b }), Color::Spec(Rgb { r: MARK, g: hi, b: lo })) => {
            Some(((r as u16) << 8 | g as u16, b as u16, (hi as u16) << 8 | lo as u16))
        }
        _ => None,
    }
}

// ---------- the filter ----------

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Ground,
    Esc,
    /// `ESC _`: an APC string (kitty's are `ESC _ G ... ESC \`).
    Apc,
    ApcEsc,
    /// `ESC P` and its parameters, until the final byte says whether it's SIXEL.
    DcsParams,
    Sixel,
    SixelEsc,
    /// `ESC ]` until it's clear whether this is iTerm2's `1337;File=`.
    OscPrefix,
    OscFile,
    OscFileEsc,
    /// `ESC [`: a few queries are answered here.
    Csi,
}

const OSC_FILE: &[u8] = b"1337;File=";

/// Takes image sequences out of the PTY's output, draws them as placeholder cells, and
/// answers image-related queries. Everything else passes through unchanged.
pub struct Filter {
    state: State,
    /// The sequence being collected (without its introducer).
    buf: Vec<u8>,
    /// The bytes as they came, to pass on if the sequence isn't one of ours.
    raw: Vec<u8>,
    geometry: SharedGeometry,
    store: SharedStore,
    respond: Box<dyn FnMut(String) + Send>,
    kitty: kitty::Uploads,
}

impl Filter {
    pub fn new(geometry: SharedGeometry, store: SharedStore, respond: impl FnMut(String) + Send + 'static) -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            raw: Vec::new(),
            geometry,
            store,
            respond: Box::new(respond),
            kitty: Default::default(),
        }
    }

    /// Filter `input`, appending what the terminal should see to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < input.len() {
            if self.state == State::Ground {
                // Copy up to the next ESC in one go.
                match input[i..].iter().position(|b| *b == 0x1b) {
                    Some(n) => {
                        out.extend_from_slice(&input[i..i + n]);
                        i += n + 1;
                        self.state = State::Esc;
                        self.raw.clear();
                        self.raw.push(0x1b);
                        self.buf.clear();
                    }
                    None => {
                        out.extend_from_slice(&input[i..]);
                        return;
                    }
                }
                continue;
            }
            let b = input[i];
            i += 1;
            self.step(b, out);
        }
    }

    /// Give up on the sequence being collected: the terminal gets it as it came.
    fn pass(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.raw);
        self.raw.clear();
        self.buf.clear();
        self.state = State::Ground;
    }

    fn done(&mut self) {
        self.raw.clear();
        self.buf.clear();
        self.state = State::Ground;
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        self.raw.push(b);
        if self.raw.len() > MAX_SEQUENCE {
            // Too big to be anything worth keeping: drop it.
            self.done();
            return;
        }
        match self.state {
            State::Ground => unreachable!(),
            State::Esc => match b {
                b'_' => self.state = State::Apc,
                b'P' => self.state = State::DcsParams,
                b']' => self.state = State::OscPrefix,
                b'[' => self.state = State::Csi,
                _ => self.pass(out),
            },
            State::Apc => match b {
                0x1b => self.state = State::ApcEsc,
                _ => self.buf.push(b),
            },
            State::ApcEsc => match b {
                b'\\' => {
                    if self.buf.first() == Some(&b'G') {
                        let body = std::mem::take(&mut self.buf);
                        self.done();
                        self.kitty_command(&body[1..], out);
                    } else {
                        self.pass(out);
                    }
                }
                0x1b => self.buf.push(0x1b),
                _ => {
                    self.buf.extend_from_slice(&[0x1b, b]);
                    self.state = State::Apc;
                }
            },
            State::DcsParams => match b {
                b'0'..=b'9' | b';' => self.buf.push(b),
                // SIXEL is `ESC P params q`; anything else (DECRQSS, tmux, ...) passes.
                b'q' => {
                    self.buf.push(b'q');
                    self.state = State::Sixel;
                }
                _ => self.pass(out),
            },
            State::Sixel => match b {
                0x1b => self.state = State::SixelEsc,
                _ => self.buf.push(b),
            },
            State::SixelEsc => match b {
                b'\\' => {
                    let body = std::mem::take(&mut self.buf);
                    self.done();
                    self.sixel(&body, out);
                }
                _ => {
                    self.buf.extend_from_slice(&[0x1b, b]);
                    self.state = State::Sixel;
                }
            },
            State::OscPrefix => {
                self.buf.push(b);
                if self.buf.as_slice() == OSC_FILE {
                    self.buf.clear();
                    self.state = State::OscFile;
                } else if !OSC_FILE.starts_with(&self.buf) {
                    self.pass(out);
                }
            }
            State::OscFile => match b {
                0x07 => {
                    let body = std::mem::take(&mut self.buf);
                    self.done();
                    self.iterm(&body, out);
                }
                0x1b => self.state = State::OscFileEsc,
                _ => self.buf.push(b),
            },
            State::OscFileEsc => match b {
                b'\\' => {
                    let body = std::mem::take(&mut self.buf);
                    self.done();
                    self.iterm(&body, out);
                }
                _ => {
                    self.buf.extend_from_slice(&[0x1b, b]);
                    self.state = State::OscFile;
                }
            },
            State::Csi => {
                self.buf.push(b);
                if (0x40..=0x7e).contains(&b) {
                    self.csi(out);
                } else if self.buf.len() > 32 || !(0x20..=0x3f).contains(&b) {
                    self.pass(out);
                }
            }
        }
    }

    fn csi(&mut self, out: &mut Vec<u8>) {
        let reply = match self.buf.as_slice() {
            // Primary device attributes, with 4 (SIXEL); alacritty would leave it out.
            b"c" | b"0c" => Some("\x1b[?62;4;22c".to_string()),
            // The cell size in pixels (what image tools size pictures by).
            b"16t" => {
                let g = *self.geometry.lock().unwrap();
                Some(format!("\x1b[6;{};{}t", g.cell_height.round(), g.cell_width.round()))
            }
            _ => None,
        };
        match reply {
            Some(reply) => {
                self.done();
                (self.respond)(reply);
            }
            None => self.pass(out),
        }
    }

    /// Put `picture` at the cursor over the cells `cells` works out, and write its placeholders.
    fn place(
        &mut self,
        picture: Arc<Picture>,
        size: (Option<u16>, Option<u16>),
        after: After,
        kitty: Option<u32>,
        out: &mut Vec<u8>,
    ) {
        let geometry = *self.geometry.lock().unwrap();
        let (cols, rows, fit) = cells(picture.width, picture.height, size.0, size.1, &geometry);
        let id = self.store.lock().unwrap().place(Placement { picture, cols, rows, fit, kitty });
        out.extend_from_slice(&placeholder_bytes(id, cols, rows, after));
    }

    fn sixel(&mut self, body: &[u8], out: &mut Vec<u8>) {
        if let Some(rgba) = sixel::decode(body) {
            self.place(Picture::new(rgba), (None, None), After::Below, None, out);
        }
    }

    fn iterm(&mut self, body: &[u8], out: &mut Vec<u8>) {
        let geometry = *self.geometry.lock().unwrap();
        if let Some(file) = iterm::parse(body, &geometry) {
            let after = if file.stay { After::Stay } else { After::Below };
            let picture = Picture::new(file.image);
            let geometry = *self.geometry.lock().unwrap();
            let (cols, rows, fit) = match (file.cols, file.rows) {
                (None, None) => cells(picture.width, picture.height, None, None, &geometry),
                (c, r) => {
                    let (cols, rows, _) = cells(picture.width, picture.height, c, r, &geometry);
                    (cols, rows, if file.keep_shape { Fit::Contain } else { Fit::Stretch })
                }
            };
            let id = self.store.lock().unwrap().place(Placement { picture, cols, rows, fit, kitty: None });
            out.extend_from_slice(&placeholder_bytes(id, cols, rows, after));
        }
    }

    fn kitty_command(&mut self, body: &[u8], out: &mut Vec<u8>) {
        let Some(command) = self.kitty.take(body) else { return };
        let control = command.control;
        let reply = |result: Result<(), String>| -> Option<String> {
            kitty::reply(&control, result.as_ref().map(|_| ()).map_err(String::as_str))
        };
        match control.action {
            b'q' => {
                let result = kitty::decode(&control, &command.data).map(|_| ());
                if let Some(r) = reply(result) {
                    (self.respond)(r);
                }
            }
            b't' | b'T' => match kitty::decode(&control, &command.data) {
                Ok(rgba) => {
                    let picture = Picture::new(rgba);
                    let id = {
                        let mut store = self.store.lock().unwrap();
                        let id = match (control.id, control.number) {
                            (Some(id), _) => id,
                            (None, number) => {
                                let id = store.new_kitty_id();
                                if let Some(n) = number {
                                    store.numbers.insert(n, id);
                                }
                                id
                            }
                        };
                        store.keep_kitty(id, picture.clone());
                        id
                    };
                    if control.action == b'T' {
                        let after = if control.cursor_stays { After::Stay } else { After::Right };
                        self.place(picture, (control.cols, control.rows), after, Some(id), out);
                    }
                    if let Some(r) = reply(Ok(())) {
                        (self.respond)(r);
                    }
                }
                Err(err) => {
                    if let Some(r) = reply(Err(err)) {
                        (self.respond)(r);
                    }
                }
            },
            b'p' => {
                let found = {
                    let store = self.store.lock().unwrap();
                    let id = control.id.or_else(|| control.number.and_then(|n| store.numbers.get(&n).copied()));
                    id.and_then(|id| store.kitty.get(&id).cloned().map(|p| (id, p)))
                };
                match found {
                    Some((id, picture)) => {
                        let after = if control.cursor_stays { After::Stay } else { After::Right };
                        self.place(picture, (control.cols, control.rows), after, Some(id), out);
                        if let Some(r) = reply(Ok(())) {
                            (self.respond)(r);
                        }
                    }
                    None => {
                        if let Some(r) = reply(Err("ENOENT:no such image".into())) {
                            (self.respond)(r);
                        }
                    }
                }
            }
            b'd' => {
                let mut store = self.store.lock().unwrap();
                let free = control.delete.is_ascii_uppercase();
                match control.delete.to_ascii_lowercase() {
                    b'i' => store.delete_kitty(control.id, free),
                    b'n' => {
                        let id = control.number.and_then(|n| store.numbers.get(&n).copied());
                        if id.is_some() {
                            store.delete_kitty(id, free);
                        }
                    }
                    // All, and the selectors TJ doesn't track (they clear everything kitty drew).
                    _ => store.delete_kitty(None, free),
                }
            }
            _ => {
                if let Some(r) = reply(Err("ENOTSUP:not supported".into())) {
                    (self.respond)(r);
                }
            }
        }
    }
}

// ---------- the PTY with the filter in front ----------

/// alacritty's PTY, read through a [`Filter`].
pub struct FilteredPty {
    pty: Pty,
    reader: FilteredReader,
}

pub struct FilteredReader {
    file: File,
    filter: Filter,
    out: Vec<u8>,
    at: usize,
    scratch: Vec<u8>,
}

impl FilteredPty {
    pub fn new(pty: Pty, filter: Filter) -> io::Result<Self> {
        let file = pty.file().try_clone()?;
        Ok(Self { pty, reader: FilteredReader { file, filter, out: Vec::new(), at: 0, scratch: vec![0; 1 << 16] } })
    }

    pub fn child_id(&self) -> u32 {
        self.pty.child().id()
    }
}

impl Read for FilteredReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.at < self.out.len() {
                let n = buf.len().min(self.out.len() - self.at);
                buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
                self.at += n;
                if self.at == self.out.len() {
                    self.out.clear();
                    self.at = 0;
                }
                return Ok(n);
            }
            // Nothing waiting: read more (an image may swallow a whole read, so loop until
            // there's output, the PTY would block, or it ends).
            let n = self.file.read(&mut self.scratch)?;
            if n == 0 {
                return Ok(0);
            }
            let (filter, scratch, out) = (&mut self.filter, &self.scratch, &mut self.out);
            filter.feed(&scratch[..n], out);
        }
    }
}

impl EventedReadWrite for FilteredPty {
    type Reader = FilteredReader;
    type Writer = File;

    unsafe fn register(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        unsafe { self.pty.register(poll, interest, mode) }
    }
    fn reregister(&mut self, poll: &Arc<Poller>, interest: Event, mode: PollMode) -> io::Result<()> {
        self.pty.reregister(poll, interest, mode)
    }
    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.pty.deregister(poll)
    }
    fn reader(&mut self) -> &mut FilteredReader {
        &mut self.reader
    }
    fn writer(&mut self) -> &mut File {
        self.pty.writer()
    }
}

impl EventedPty for FilteredPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.pty.next_child_event()
    }
}

impl OnResize for FilteredPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.pty.on_resize(window_size);
        let mut g = self.reader.filter.geometry.lock().unwrap();
        g.cols = window_size.num_cols;
        g.rows = window_size.num_lines;
    }
}

#[cfg(test)]
mod tests;
