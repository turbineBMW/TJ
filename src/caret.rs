//! The smooth cursor, shared by the terminal and the text fields. It moves the way WordCraft's
//! does (which measured it from Word for Windows):
//!
//! - **A key moves the caret:** it glides, as a solid bar, from where it was to its new place,
//!   fast at first and settling ([`ease`]). The time is the same for one character and a whole
//!   line.
//! - **Another key before it arrives:** it snaps to where it was heading and glides from there,
//!   so it never falls behind the text.
//! - **A click moves it:** no glide; it is drawn at its new place.
//! - **Anything else moves it** (a terminal program, text set by the app): no glide, no fade.
//! - **The blink is a fade:** on, then off, each change fading over the glide time. A key or a
//!   click turns the caret on, fading in from however visible it was.
//!
//! What never glides: a caret that moved because its view changed (the field moved, grew or
//! scrolled; the terminal resized or scrolled back), one that wasn't drawn the frame before,
//! and moves that start or end off screen. A glide time of zero, or the system's reduce-motion,
//! makes the caret jump and blink on and off.
//!
//! To tell a key's move from the rest, the window's root reports keys and mouse buttons
//! ([`watch`]) and each draw ([`begin_frame`]); every caret drawn in a frame reads the same time.
//!
//! Text fields keep gpui-kit's own caret but draw it invisible ([`hide_native_caret`]); an
//! [`overlay`] paints this one where theirs would be.

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use gpui_kit::*;

/// The caret is on for this long, then off for this long.
const BLINK: Duration = Duration::from_millis(530);
/// Moves shorter than this (pixels) aren't worth a glide.
const MIN_MOVE: f32 = 0.5;
/// How far outside its viewport a glide may start or end (pixels).
const OFFSCREEN_SLACK: f32 = 48.0;
/// A key or click counts for the moves drawn this many frames after it, and this soon: a
/// field shows the move it makes in the next frame.
const INPUT_FRAMES: u64 = 2;
const INPUT_AGE: Duration = Duration::from_millis(250);
/// A terminal's program echoes a key some time after it: moves within this long of a key
/// count as that key's.
pub const TERMINAL_ECHO: Duration = Duration::from_millis(200);

/// How the caret moves, from the settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    /// How long a glide, and each fade of the blink, takes; zero jumps and blinks hard.
    pub glide: Duration,
}

impl Motion {
    pub fn from_settings(cx: &App) -> Self {
        let settings = cx.try_global::<crate::settings::Settings>().cloned().unwrap_or_default();
        let glide = if cx.reduce_motion() { 0 } else { settings.cursor_glide_ms as u64 };
        Self { glide: Duration::from_millis(glide) }
    }
}

// ---------- the window's clock ----------

/// A key or a mouse button: the frame it came after, and when.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stamp {
    pub frame: u64,
    pub at: Instant,
}

/// The window's draws and its last key and click, which every caret reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clock {
    /// Counts the window's draws: a caret that missed one doesn't glide from its stale place.
    pub frame: u64,
    /// When this draw began.
    pub now: Option<Instant>,
    pub key: Option<Stamp>,
    /// The last mouse press or release.
    pub pointer: Option<Stamp>,
    /// A mouse button is held (a drag moves the caret without gliding).
    pub pointer_down: bool,
}

impl Global for Clock {}

impl Clock {
    /// What a move drawn `now` was caused by: a key (unless a click or a held button came
    /// since), the pointer, and when the latest of them was. A key or click counts for the next
    /// few frames, or for `echo` after it.
    fn cause(&self, now: Instant, echo: Duration) -> (bool, bool, Option<Instant>) {
        let recent = |s: Option<Stamp>| {
            s.filter(|s| {
                let age = now.saturating_duration_since(s.at);
                (self.frame.wrapping_sub(s.frame) <= INPUT_FRAMES && age <= INPUT_AGE) || age <= echo
            })
        };
        let (key, pointer) = (recent(self.key), recent(self.pointer));
        let by_pointer = self.pointer_down || pointer.is_some_and(|p| key.is_none_or(|k| p.at >= k.at));
        let latest = key.into_iter().chain(pointer).map(|s| s.at).max();
        (key.is_some() && !by_pointer, by_pointer, latest)
    }

    /// A key, just now, is behind what this frame shows.
    pub fn typing(&self, now: Instant) -> bool {
        self.cause(now, Duration::ZERO).0
    }
}

/// A new draw of the window: called first thing in the root view's render.
pub fn begin_frame(cx: &mut App) {
    let clock = cx.default_global::<Clock>();
    clock.frame += 1;
    clock.now = Some(Instant::now());
}

fn note_key(cx: &mut App) {
    let clock = cx.default_global::<Clock>();
    clock.key = Some(Stamp { frame: clock.frame, at: Instant::now() });
}

fn note_pointer(down: bool, cx: &mut App) {
    let clock = cx.default_global::<Clock>();
    clock.pointer = Some(Stamp { frame: clock.frame, at: Instant::now() });
    clock.pointer_down = down;
}

/// Keys bound to actions (arrows in a field) only reach keystroke observers.
pub fn init(cx: &mut App) {
    cx.observe_keystrokes(|_, _, cx| note_key(cx)).detach();
}

/// Report the window's keys and mouse buttons to the carets: on the root element, in the
/// capture phase, so views that stop a key (the terminal) don't hide it.
pub fn watch(root: Div) -> Div {
    let buttons = canvas(
        |_, _, _| {},
        |_, _, window, _| {
            window.on_mouse_event(|_: &MouseDownEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture {
                    note_pointer(true, cx);
                }
            });
            window.on_mouse_event(|_: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture {
                    note_pointer(false, cx);
                }
            });
        },
    )
    .absolute()
    .size_0();
    root.capture_key_down(|_, _, cx| note_key(cx)).child(buttons)
}

// ---------- one caret ----------

/// Where a caret belongs this frame, in its owner's space.
#[derive(Clone, Copy, Debug)]
pub struct Aim {
    pub target: Bounds<Pixels>,
    /// A fingerprint of how its owner is laid out ([`fingerprint`]): a move that comes with a
    /// changed view doesn't glide.
    pub view: u64,
    /// The visible part of the owner, in the same space: moves off it don't glide.
    pub viewport: Bounds<Pixels>,
    /// How long after a key a move still counts as the key's (besides the next frames).
    pub echo: Duration,
    pub blinking: bool,
}

/// How to draw the caret this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub bounds: Bounds<Pixels>,
    /// 0 (not at all) to 1.
    pub opacity: f32,
    pub gliding: bool,
    /// When it next changes: zero means the next frame; `None`, not until something moves it.
    pub next: Option<Duration>,
}

#[derive(Clone, Copy, Debug)]
struct Glide {
    from: Bounds<Pixels>,
    to: Bounds<Pixels>,
    started: Instant,
}

/// The caret turning on after a key or click, from however visible it was.
#[derive(Clone, Copy, Debug)]
struct FadeIn {
    from: f32,
    started: Instant,
}

/// A caret's motion and blink state.
pub struct Caret {
    /// Where it belonged last frame; `None` when it wasn't drawn.
    target: Option<Bounds<Pixels>>,
    frame: u64,
    view: u64,
    opacity: f32,
    glide: Option<Glide>,
    fade: Option<FadeIn>,
    /// The last key or click this caret answered.
    input: Option<Instant>,
    /// When the blink cycle (re)started.
    blink_from: Instant,
    /// The pending wake-up for the next blink change, so only one is scheduled.
    wake: Option<(Instant, Task<()>)>,
}

impl Default for Caret {
    fn default() -> Self {
        Self {
            target: None,
            frame: 0,
            view: 0,
            opacity: 0.0,
            glide: None,
            fade: None,
            input: None,
            blink_from: Instant::now(),
            wake: None,
        }
    }
}

impl Caret {
    /// Forget the caret (it isn't drawn): it appears wherever it belongs next time.
    pub fn reset(&mut self) {
        self.target = None;
        self.glide = None;
        self.fade = None;
        self.opacity = 0.0;
    }

    /// Step the caret for this frame: where and how strongly to draw it.
    pub fn frame(&mut self, aim: Aim, motion: Motion, clock: &Clock) -> Look {
        let now = clock.now.unwrap_or_else(Instant::now);
        let continuous = clock.frame.wrapping_sub(self.frame) <= 1;
        let was = self.target.filter(|_| continuous);
        let was_opacity = if was.is_some() { self.opacity } else { 0.0 };
        let same_view = aim.view == self.view;
        self.frame = clock.frame;
        self.view = aim.view;

        // What moved it: a key, unless a click (or a held button) came since.
        let (by_keys, by_pointer, latest) = clock.cause(now, aim.echo);
        let by_user = by_keys || by_pointer;
        let fresh = by_user && latest != self.input;
        if by_user {
            self.input = latest;
        }

        let animate = !motion.glide.is_zero();
        if !same_view || !animate {
            self.glide = None;
        }
        // Where the caret was heading: the end of the glide under way, or its place last frame.
        let heading = self.glide.map(|g| g.to).or(was);
        let moved = heading.is_none_or(|h| distance(h, aim.target) >= MIN_MOVE);
        if fresh {
            // A key or a click turns the caret on, from however visible it was.
            self.blink_from = now;
            self.fade = (animate && was_opacity < 1.0).then_some(FadeIn { from: was_opacity, started: now });
        }
        if moved {
            let seen = aim.viewport.dilate(px(OFFSCREEN_SLACK));
            self.glide = match heading {
                Some(from)
                    if animate && by_keys && same_view && seen.intersects(&from) && seen.intersects(&aim.target) =>
                {
                    Some(Glide { from, to: aim.target, started: now })
                }
                _ => None,
            };
            if !by_user {
                // Something else put it there: it is simply there.
                self.fade = None;
            }
        }

        let bounds = match self.glide {
            Some(g) => match progress(now, g.started, motion.glide) {
                Some(s) => lerp(g.from, g.to, ease(s)),
                None => {
                    self.glide = None;
                    aim.target
                }
            },
            None => aim.target,
        };
        let (blink_opacity, blink_next) = blink(now.saturating_duration_since(self.blink_from), motion.glide);
        let (opacity, next) = match self.fade {
            Some(fade) => match progress(now, fade.started, motion.glide) {
                Some(s) => (fade.from + (1.0 - fade.from) * s, Some(Duration::ZERO)),
                None => {
                    self.fade = None;
                    (1.0, aim.blinking.then_some(blink_next))
                }
            },
            None if !aim.blinking => (1.0, None),
            // A gliding caret stays lit; a resting one blinks.
            None if self.glide.is_some() => (1.0, Some(Duration::ZERO)),
            None => (blink_opacity, Some(blink_next)),
        };
        let next = if self.glide.is_some() { Some(Duration::ZERO) } else { next };
        self.target = Some(aim.target);
        self.opacity = opacity;
        Look { bounds, opacity, gliding: self.glide.is_some(), next }
    }

    /// [`Self::frame`] for a real window: reads the clock and settings, and asks for the frames
    /// or the wake-up the next change needs.
    pub fn look(&mut self, aim: Aim, window: &mut Window, cx: &mut App) -> Look {
        let clock = cx.try_global::<Clock>().copied().unwrap_or_default();
        let look = self.frame(aim, Motion::from_settings(cx), &clock);
        match look.next {
            Some(wait) if wait.is_zero() => window.request_animation_frame(),
            Some(wait) => {
                let at = clock.now.unwrap_or_else(Instant::now) + wait;
                if self.wake.as_ref().is_none_or(|(when, _)| *when != at) {
                    let handle = window.window_handle();
                    let task = cx.spawn(async move |cx| {
                        cx.background_executor().timer(wait).await;
                        let _ = handle.update(cx, |_, window, _| window.refresh());
                    });
                    self.wake = Some((at, task));
                }
            }
            None => self.wake = None,
        }
        look
    }
}

/// How far along an animation that started at `started` and lasts `length` is at `now`: 0 to
/// 1, or `None` once it is over.
pub(crate) fn progress(now: Instant, started: Instant, length: Duration) -> Option<f32> {
    if length.is_zero() {
        return None;
    }
    let s = now.saturating_duration_since(started).as_secs_f32() / length.as_secs_f32();
    (s < 1.0).then_some(s)
}

/// Word's curve for the glide, `cubic-bezier(0, 0, 0, 1)`: more than a third of the way after
/// 7% of the time, nine tenths after half of it. For these control points it has a closed form:
/// time is `u³` and distance `3u² − 2u³`.
pub fn ease(s: f32) -> f32 {
    let s = if s.is_finite() { s.clamp(0.0, 1.0) } else { 1.0 };
    3.0 * s.powf(2.0 / 3.0) - 2.0 * s
}

/// A resting caret's opacity `since` its blink restarted: on for [`BLINK`], off for as long,
/// each change taking `fade`. Also how long until it next changes (zero while fading).
pub fn blink(since: Duration, fade: Duration) -> (f32, Duration) {
    if since < BLINK {
        return (1.0, BLINK - since);
    }
    let phase = since.as_nanos() % (2 * BLINK.as_nanos());
    let (into, turning_on) = if phase < BLINK.as_nanos() {
        (Duration::from_nanos(phase as u64), true)
    } else {
        (Duration::from_nanos((phase - BLINK.as_nanos()) as u64), false)
    };
    let (done, next) =
        if into < fade { (into.as_secs_f32() / fade.as_secs_f32(), Duration::ZERO) } else { (1.0, BLINK - into) };
    (if turning_on { done } else { 1.0 - done }, next)
}

fn lerp(a: Bounds<Pixels>, b: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    let size =
        size(a.size.width + (b.size.width - a.size.width) * t, a.size.height + (b.size.height - a.size.height) * t);
    Bounds::new(a.origin + (b.origin - a.origin) * t, size)
}

/// How far the caret's farthest corner moves.
fn distance(a: Bounds<Pixels>, b: Bounds<Pixels>) -> f32 {
    let d = |p: Point<Pixels>, q: Point<Pixels>| f32::from((q.x - p.x).abs()).max(f32::from((q.y - p.y).abs()));
    d(a.origin, b.origin).max(d(a.bottom_right(), b.bottom_right()))
}

/// A fingerprint of an owner's layout, for [`Aim::view`].
pub fn fingerprint(values: &[f32]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in values {
        v.to_bits().hash(&mut h);
    }
    h.finish()
}

/// Fill the caret.
pub fn paint(bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    window.paint_quad(fill(bounds, color));
}

/// gpui-kit's inputs paint their caret in the theme's caret colour, which can't be fully
/// transparent (that reads as "unset"), so it gets an invisible sliver of alpha instead.
pub fn hide_native_caret(cx: &mut App) {
    gpui_kit::component::Theme::global_mut(cx).caret = hsla(0.0, 0.0, 0.5, 0.002);
}

/// Each focused text field's caret and growing letters, by the field's entity (a few at most;
/// old ones drop out).
#[derive(Default)]
struct FieldCarets(Vec<(EntityId, (Caret, crate::grow::Grow))>);

impl Global for FieldCarets {}

const KEPT_FIELD_CARETS: usize = 32;

/// How many letters are growing in across the fields.
#[cfg(test)]
pub fn growing_letters(cx: &App) -> usize {
    cx.try_global::<FieldCarets>().map_or(0, |f| f.0.iter().map(|(_, (_, grow))| grow.growing()).sum())
}

/// gpui-kit's text size in a default (medium) field, `text_sm`.
const FIELD_TEXT_SIZE: f32 = 0.875;

/// The caret for a text field, drawn over it where its own (hidden) caret is while it has
/// focus. Deferred, so it paints after the field and from the field's current layout.
pub fn overlay<M: gpui_kit::base::input::InputModeKind + 'static>(
    input: &Entity<gpui_kit::base::input::InputBaseState<M>>,
) -> impl IntoElement {
    overlay_with(input, None)
}

/// [`overlay`], and typed letters grow in ([`crate::grow`]). `behind` is the colour behind the
/// field's text, which must be opaque (a see-through one leaves the letters as they are); the
/// field must be default-sized, in the font of the element this sits in.
pub fn overlay_growing<M: gpui_kit::base::input::InputModeKind + 'static>(
    input: &Entity<gpui_kit::base::input::InputBaseState<M>>,
    behind: Hsla,
) -> impl IntoElement {
    overlay_with(input, Some(behind).filter(|c| c.a >= 1.0))
}

fn overlay_with<M: gpui_kit::base::input::InputModeKind + 'static>(
    input: &Entity<gpui_kit::base::input::InputBaseState<M>>,
    behind: Option<Hsla>,
) -> impl IntoElement {
    let input = input.downgrade();
    deferred(
        canvas(
            |_, _, _| {},
            move |_, _, window, cx| {
                let Some(input) = input.upgrade() else { return };
                let id = input.entity_id();
                let (geometry, scroll, focused, text, cursor) = {
                    let state = input.read(cx);
                    let focused = state.focus_handle(cx).is_focused(window);
                    // Only read (and copied) where letters may grow.
                    let text = if behind.is_some() && focused { state.value() } else { SharedString::default() };
                    (state.prepaint_caret_geometry(), state.scroll_offset(), focused, text, state.cursor())
                };
                // Taken out of the map while it's used, since it needs the app too; an
                // unfocused field's caret isn't drawn, so it is forgotten.
                let carets = cx.default_global::<FieldCarets>();
                let kept = carets.0.iter().position(|(e, _)| *e == id).map(|ix| carets.0.remove(ix).1);
                let (Some(geometry), true) = (geometry, focused) else { return };
                let (mut caret, mut grow) = kept.unwrap_or_default();
                let mut target = geometry.cursor_bounds();
                target.size.width = target.size.width.max(px(2.));
                let field = geometry.input_bounds();
                let view = fingerprint(&[
                    field.origin.x.into(),
                    field.origin.y.into(),
                    field.size.width.into(),
                    field.size.height.into(),
                    scroll.x.into(),
                    scroll.y.into(),
                ]);
                let ink = {
                    use gpui_kit::component::ActiveTheme as _;
                    cx.theme().foreground
                };
                if let Some(behind) = behind {
                    let clock = cx.try_global::<Clock>().copied().unwrap_or_default();
                    let length = Motion::from_settings(cx).glide;
                    let (font, font_size) =
                        (window.text_style().font(), rems(FIELD_TEXT_SIZE).to_pixels(window.rem_size()));
                    let line = geometry.line_height();
                    // The caret's x at the top of its line, the line's height.
                    let spot = Bounds::new(
                        point(target.origin.x, target.origin.y - (line - target.size.height) / 2.),
                        size(px(0.), line),
                    );
                    let seen = crate::grow::Seen { text, cursor, caret: spot, view };
                    grow.step(seen, &clock, length, |letter| {
                        Some(crate::grow::shape(&letter.to_string().into(), &font, font_size, ink, window).width)
                    });
                    if grow.is_growing() {
                        let now = clock.now.unwrap_or_else(Instant::now);
                        grow.paint(now, length, behind, ink, &font, font_size, window, cx);
                        window.request_animation_frame();
                    }
                }
                let aim = Aim { target, view, viewport: field, echo: Duration::ZERO, blinking: true };
                let look = caret.look(aim, window, cx);
                if look.opacity > 0.0 {
                    paint(look.bounds, ink.opacity(look.opacity), window);
                }
                let carets = cx.default_global::<FieldCarets>();
                carets.0.push((id, (caret, grow)));
                if carets.0.len() > KEPT_FIELD_CARETS {
                    carets.0.remove(0);
                }
            },
        )
        .absolute()
        .size_0(),
    )
}

#[cfg(test)]
mod tests {
    use super::{Aim, BLINK, Caret, Clock, Motion, Stamp, blink, ease};
    use gpui_kit::{Bounds, Pixels, point, px, size};
    use std::time::{Duration, Instant};

    const MOTION: Motion = Motion { glide: Duration::from_millis(100) };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn cell(x: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(0.)), size(px(2.), px(20.)))
    }

    fn aim(x: f32) -> Aim {
        Aim {
            target: cell(x),
            view: 1,
            viewport: Bounds::new(point(px(0.), px(0.)), size(px(500.), px(100.))),
            echo: Duration::ZERO,
            blinking: true,
        }
    }

    /// Draws frames of one window, with keys and clicks between them.
    struct Window {
        clock: Clock,
        start: Instant,
    }

    impl Window {
        fn new() -> Self {
            Self { clock: Clock::default(), start: Instant::now() }
        }
        fn at(&mut self, t: Duration) {
            self.clock.frame += 1;
            self.clock.now = Some(self.start + t);
        }
        fn key(&mut self, t: Duration) {
            self.clock.key = Some(Stamp { frame: self.clock.frame, at: self.start + t });
        }
        fn click(&mut self, t: Duration) {
            self.clock.pointer = Some(Stamp { frame: self.clock.frame, at: self.start + t });
        }
        fn draw(&mut self, caret: &mut Caret, aim: Aim, t: Duration) -> super::Look {
            self.at(t);
            caret.frame(aim, MOTION, &self.clock)
        }
    }

    #[test]
    fn a_key_glides_the_caret_as_a_solid_bar() {
        let (mut w, mut caret) = (Window::new(), Caret::default());
        assert_eq!(w.draw(&mut caret, aim(0.), ms(0)).bounds, cell(0.), "the first place is taken as is");
        w.key(ms(10));
        let start = w.draw(&mut caret, aim(100.), ms(16));
        assert!(start.gliding && start.bounds == cell(0.), "starts where it was: {start:?}");
        let half = w.draw(&mut caret, aim(100.), ms(66));
        let x = f32::from(half.bounds.origin.x);
        assert!(x > 80. && x < 100., "fast first, settling: {x}");
        assert_eq!(half.bounds.size, cell(0.).size, "keeps its shape");
        assert_eq!(half.opacity, 1.0, "lit while it glides");
        assert_eq!(half.next, Some(Duration::ZERO), "and wants the next frame");
        let end = w.draw(&mut caret, aim(100.), ms(120));
        assert!(!end.gliding && end.bounds == cell(100.), "settled: {end:?}");
    }

    #[test]
    fn another_key_snaps_to_where_it_was_heading() {
        let (mut w, mut caret) = (Window::new(), Caret::default());
        w.draw(&mut caret, aim(0.), ms(0));
        w.key(ms(5));
        w.draw(&mut caret, aim(100.), ms(16));
        w.draw(&mut caret, aim(100.), ms(32));
        w.key(ms(40));
        let look = w.draw(&mut caret, aim(200.), ms(48));
        assert!(look.gliding && look.bounds == cell(100.), "from 100, not from part-way: {look:?}");
    }

    #[test]
    fn a_click_jumps_and_a_program_move_jumps_without_relighting() {
        let (mut w, mut caret) = (Window::new(), Caret::default());
        w.draw(&mut caret, aim(0.), ms(0));
        w.key(ms(5));
        w.click(ms(8));
        let clicked = w.draw(&mut caret, aim(100.), ms(16));
        assert!(!clicked.gliding && clicked.bounds == cell(100.), "a click wins over the key: {clicked:?}");

        // Long after any input, into the blink's off half: the program moves it.
        w.draw(&mut caret, aim(100.), ms(1000));
        let moved = w.draw(&mut caret, aim(300.), ms(1016));
        assert!(!moved.gliding && moved.bounds == cell(300.), "{moved:?}");
        assert_eq!(moved.opacity, 0.0, "the blink carries on");
    }

    #[test]
    fn a_terminal_echo_after_a_key_still_glides() {
        let (mut w, mut caret) = (Window::new(), Caret::default());
        let echoed = |x| Aim { echo: super::TERMINAL_ECHO, ..aim(x) };
        w.draw(&mut caret, echoed(0.), ms(0));
        w.key(ms(5));
        for t in [16, 32, 48, 64] {
            w.draw(&mut caret, echoed(0.), ms(t));
        }
        assert!(w.draw(&mut caret, echoed(10.), ms(80)).gliding, "the echo, five frames on");
        let (mut w, mut caret) = (Window::new(), Caret::default());
        w.draw(&mut caret, aim(0.), ms(0));
        w.key(ms(5));
        for t in [16, 32, 48, 64] {
            w.draw(&mut caret, aim(0.), ms(t));
        }
        assert!(!w.draw(&mut caret, aim(10.), ms(80)).gliding, "a field's move comes the next frame");
    }

    #[test]
    fn no_glide_for_a_changed_view_a_missed_frame_or_off_screen() {
        let (mut w, mut caret) = (Window::new(), Caret::default());
        w.draw(&mut caret, aim(0.), ms(0));
        w.key(ms(5));
        assert!(!w.draw(&mut caret, Aim { view: 2, ..aim(100.) }, ms(16)).gliding, "the view changed");

        let (mut w, mut caret) = (Window::new(), Caret::default());
        w.draw(&mut caret, aim(0.), ms(0));
        w.at(ms(8)); // a draw without this caret
        w.key(ms(10));
        assert!(!w.draw(&mut caret, aim(100.), ms(16)).gliding, "wasn't drawn the frame before");

        let (mut w, mut caret) = (Window::new(), Caret::default());
        w.draw(&mut caret, aim(0.), ms(0));
        w.key(ms(5));
        assert!(!w.draw(&mut caret, aim(900.), ms(16)).gliding, "ends off screen");
    }

    #[test]
    fn a_key_fades_a_dark_caret_back_in() {
        let (mut w, mut caret) = (Window::new(), Caret::default());
        w.draw(&mut caret, aim(0.), ms(0));
        assert_eq!(w.draw(&mut caret, aim(0.), ms(800)).opacity, 0.0, "blinked off");
        w.key(ms(805));
        let first = w.draw(&mut caret, aim(0.), ms(810));
        assert!(first.opacity < 0.2 && first.next == Some(Duration::ZERO), "fades in: {first:?}");
        assert_eq!(w.draw(&mut caret, aim(0.), ms(920)).opacity, 1.0, "and stays on");
    }

    #[test]
    fn no_glide_time_jumps_and_blinks_hard() {
        let (mut w, mut caret) = (Window::new(), Caret::default());
        let motion = Motion { glide: Duration::ZERO };
        w.at(ms(0));
        caret.frame(aim(0.), motion, &w.clock);
        w.key(ms(5));
        w.at(ms(16));
        let look = caret.frame(aim(100.), motion, &w.clock);
        assert!(!look.gliding && look.bounds == cell(100.));
        w.at(ms(16) + BLINK + ms(1));
        assert_eq!(caret.frame(aim(100.), motion, &w.clock).opacity, 0.0, "off at once");
    }

    #[test]
    fn it_blinks_with_fades() {
        let fade = ms(100);
        assert_eq!(blink(ms(0), fade), (1.0, BLINK));
        let (alpha, next) = blink(BLINK + ms(50), fade);
        assert!((alpha - 0.5).abs() < 0.01 && next.is_zero(), "half faded out: {alpha}");
        assert_eq!(blink(BLINK + ms(200), fade), (0.0, BLINK - ms(200)));
        assert_eq!(blink(BLINK * 2 + ms(300), fade), (1.0, BLINK - ms(300)), "back on");
        assert_eq!(blink(BLINK * 4 + ms(10), fade), blink(BLINK * 2 + ms(10), fade), "and repeats");
    }

    #[test]
    fn the_curve_is_words() {
        assert!(ease(0.07) > 1.0 / 3.0, "{}", ease(0.07));
        assert!((ease(0.5) - 0.89).abs() < 0.01, "{}", ease(0.5));
        assert_eq!((ease(0.0), ease(1.0)), (0.0, 1.0));
    }

    /// An alpha of 0 would read as "unset" and bring the native caret back in the text colour.
    #[gpui_kit::test]
    fn the_native_caret_is_hidden_but_not_unset(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::hide_native_caret(cx);
            use gpui_kit::component::ActiveTheme as _;
            let caret = cx.theme().caret;
            assert!(caret.a > 0.0 && caret.a < 0.01, "{caret:?}");
        });
    }
}
