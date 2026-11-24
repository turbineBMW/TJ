//! The smooth cursor, shared by the terminal and the text fields.
//!
//! A caret is a quad whose four corners each glide to the new spot. Without a trail they all
//! take the same time, so the caret slides. With a trail, the corners at the front arrive
//! first and the ones at the back catch up, so it stretches along its path and snaps back
//! (like Neovide's cursor). It blinks with a short fade, and shows solid again on every move.
//!
//! Text fields keep gpui-kit's own caret but draw it invisible ([`hide_native_caret`]); an
//! [`overlay`] paints this one where theirs would be.

use std::time::{Duration, Instant};

use gpui_kit::*;

/// One blink: shown, fading out, hidden, fading in (milliseconds from the start of a cycle).
const BLINK_SHOWN: u64 = 600;
const BLINK_FADE: u64 = 110;
const BLINK_CYCLE: u64 = 1200;
/// With a trail, the front corners take this share of the glide and the back ones this much.
const TRAIL_FRONT: f32 = 0.45;
const TRAIL_BACK: f32 = 1.6;

/// How the caret moves, from the settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    /// How long a move takes; zero jumps.
    pub glide: Duration,
    pub trail: bool,
}

impl Motion {
    pub fn from_settings(cx: &App) -> Self {
        let settings = cx.try_global::<crate::settings::Settings>().cloned().unwrap_or_default();
        Self { glide: Duration::from_millis(settings.cursor_glide_ms as u64), trail: settings.cursor_trail }
    }
}

/// Ease out: fast at first, settling into place.
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// A blinking caret's opacity `elapsed` into its cycle, whether it's fading (so frames are
/// needed), and how long until the next fade starts otherwise.
pub fn blink(elapsed: Duration) -> (f32, bool, Duration) {
    let ms = elapsed.as_millis() as u64 % BLINK_CYCLE;
    let hidden_at = BLINK_SHOWN + BLINK_FADE;
    let fade_in_at = BLINK_CYCLE - BLINK_FADE;
    let wait = |until: u64| Duration::from_millis(until - ms);
    if ms < BLINK_SHOWN {
        (1.0, false, wait(BLINK_SHOWN))
    } else if ms < hidden_at {
        (1.0 - (ms - BLINK_SHOWN) as f32 / BLINK_FADE as f32, true, Duration::ZERO)
    } else if ms < fade_in_at {
        (0.0, false, wait(fade_in_at))
    } else {
        ((ms - fade_in_at) as f32 / BLINK_FADE as f32, true, Duration::ZERO)
    }
}

#[derive(Clone, Copy, Debug)]
struct Corner {
    from: Point<Pixels>,
    to: Point<Pixels>,
    started: Instant,
    duration: Duration,
}

impl Corner {
    fn at(&self, now: Instant) -> (Point<Pixels>, bool) {
        let elapsed = now.saturating_duration_since(self.started);
        if self.duration.is_zero() || elapsed >= self.duration {
            return (self.to, true);
        }
        let k = ease_out(elapsed.as_secs_f32() / self.duration.as_secs_f32());
        (self.from + (self.to - self.from) * k, false)
    }
}

/// Top-left, top-right, bottom-right, bottom-left.
fn corners_of(b: Bounds<Pixels>) -> [Point<Pixels>; 4] {
    [b.origin, b.top_right(), b.bottom_right(), b.bottom_left()]
}

fn centre(points: &[Point<Pixels>; 4]) -> (f32, f32) {
    let x = points.iter().map(|p| f32::from(p.x)).sum::<f32>() / 4.0;
    let y = points.iter().map(|p| f32::from(p.y)).sum::<f32>() / 4.0;
    (x, y)
}

/// A caret's motion and blink state.
pub struct Caret {
    corners: Option<[Corner; 4]>,
    target: Option<Bounds<Pixels>>,
    /// When the blink cycle (re)started: moving or typing shows the caret solid again.
    pub blink_from: Instant,
    /// The pending wake-up for the next blink change, so only one is scheduled.
    wake: Option<(Instant, Task<()>)>,
}

impl Default for Caret {
    fn default() -> Self {
        Self { corners: None, target: None, blink_from: Instant::now(), wake: None }
    }
}

impl Caret {
    /// Head for `target`, starting from wherever the caret is drawn now. The first target is
    /// taken as is.
    pub fn aim(&mut self, target: Bounds<Pixels>, motion: Motion, now: Instant) {
        if self.target == Some(target) {
            return;
        }
        self.target = Some(target);
        self.blink_from = now;
        let to = corners_of(target);
        let Some(current) = self.corners.map(|c| c.map(|c| c.at(now).0)) else {
            self.corners = Some(to.map(|p| Corner { from: p, to: p, started: now, duration: Duration::ZERO }));
            return;
        };
        // Which way it's going decides which corners lead.
        let ((cx0, cy0), (cx1, cy1)) = (centre(&current), centre(&to));
        let (dx, dy) = (cx1 - cx0, cy1 - cy0);
        let mut corners = [Corner { from: to[0], to: to[0], started: now, duration: Duration::ZERO }; 4];
        for i in 0..4 {
            let duration = if motion.trail && (dx != 0.0 || dy != 0.0) {
                let (rx, ry) = (f32::from(to[i].x) - cx1, f32::from(to[i].y) - cy1);
                let leading = rx * dx + ry * dy > 0.0;
                motion.glide.mul_f32(if leading { TRAIL_FRONT } else { TRAIL_BACK })
            } else {
                motion.glide
            };
            corners[i] = Corner { from: current[i], to: to[i], started: now, duration };
        }
        self.corners = Some(corners);
    }

    /// The corners as drawn now, and whether any is still moving.
    pub fn corners_at(&self, now: Instant) -> Option<([Point<Pixels>; 4], bool)> {
        let corners = self.corners?;
        let mut moving = false;
        let points = corners.map(|c| {
            let (p, done) = c.at(now);
            moving |= !done;
            p
        });
        Some((points, moving))
    }

    /// Opacity now (1 without blinking), asking for the frames or wake-up the next change needs.
    pub fn alpha(&mut self, blinking: bool, moving: bool, now: Instant, window: &mut Window, cx: &mut App) -> f32 {
        let (alpha, fading, until_fade) =
            if blinking { blink(now - self.blink_from) } else { (1.0, false, Duration::MAX) };
        if moving || fading {
            window.request_animation_frame();
        } else if blinking {
            let at = now + until_fade;
            if self.wake.as_ref().is_none_or(|(when, _)| *when != at) {
                let handle = window.window_handle();
                let task = cx.spawn(async move |cx| {
                    cx.background_executor().timer(until_fade).await;
                    let _ = handle.update(cx, |_, window, _| window.refresh());
                });
                self.wake = Some((at, task));
            }
        }
        alpha
    }
}

/// Fill the caret's quad.
pub fn paint(corners: [Point<Pixels>; 4], color: Hsla, window: &mut Window) {
    let mut path = PathBuilder::fill();
    path.move_to(corners[0]);
    for p in &corners[1..] {
        path.line_to(*p);
    }
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// gpui-kit's inputs paint their caret in the theme's caret colour, which can't be fully
/// transparent (that reads as "unset"), so it gets an invisible sliver of alpha instead.
pub fn hide_native_caret(cx: &mut App) {
    gpui_kit::component::Theme::global_mut(cx).caret = hsla(0.0, 0.0, 0.5, 0.002);
}

/// Each text field's caret, by the field's entity (a few dozen at most; old ones drop out).
#[derive(Default)]
struct FieldCarets(Vec<(EntityId, Caret)>);

impl Global for FieldCarets {}

const KEPT_FIELD_CARETS: usize = 32;

/// The caret for a text field, drawn over it where its own (hidden) caret is while it has
/// focus. Deferred, so it paints after the field and from the field's current layout.
pub fn overlay<M: gpui_kit::base::input::InputModeKind + 'static>(
    input: &Entity<gpui_kit::base::input::InputBaseState<M>>,
) -> impl IntoElement {
    let input = input.downgrade();
    deferred(
        canvas(
            |_, _, _| {},
            move |_, _, window, cx| {
                let Some(input) = input.upgrade() else { return };
                let (geometry, focused) = {
                    let state = input.read(cx);
                    (state.prepaint_caret_geometry(), state.focus_handle(cx).is_focused(window))
                };
                let (Some(geometry), true) = (geometry, focused) else { return };
                let mut bounds = geometry.cursor_bounds();
                bounds.origin.y += geometry.scroll_offset().y;
                bounds.size.width = bounds.size.width.max(px(2.));
                let color = {
                    use gpui_kit::component::ActiveTheme as _;
                    cx.theme().foreground
                };
                // Taken out of the map while it's used, since it needs the app too.
                let id = input.entity_id();
                let carets = cx.default_global::<FieldCarets>();
                let mut caret = match carets.0.iter().position(|(e, _)| *e == id) {
                    Some(ix) => carets.0.remove(ix).1,
                    None => Caret::default(),
                };
                let now = Instant::now();
                caret.aim(bounds, Motion::from_settings(cx), now);
                if let Some((corners, moving)) = caret.corners_at(now) {
                    let alpha = caret.alpha(true, moving, now, window, cx);
                    if alpha > 0.0 {
                        paint(corners, color.opacity(alpha), window);
                    }
                }
                let carets = cx.default_global::<FieldCarets>();
                carets.0.push((id, caret));
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
    use super::{Caret, Motion, blink};
    use gpui_kit::{Bounds, point, px, size};
    use std::time::{Duration, Instant};

    fn cell(x: f32) -> Bounds<gpui_kit::Pixels> {
        Bounds::new(point(px(x), px(0.)), size(px(10.), px(20.)))
    }

    #[test]
    fn the_caret_glides_and_settles() {
        let ms = Duration::from_millis;
        let motion = Motion { glide: ms(100), trail: false };
        let (mut caret, start) = (Caret::default(), Instant::now());
        caret.aim(cell(0.), motion, start);
        assert_eq!(
            caret.corners_at(start).unwrap(),
            ([cell(0.).origin, cell(0.).top_right(), cell(0.).bottom_right(), cell(0.).bottom_left()], false),
            "the first target is taken as is"
        );
        caret.aim(cell(100.), motion, start);
        let (half, moving) = caret.corners_at(start + ms(50)).unwrap();
        assert!(moving && half[0].x > px(50.) && half[0].x < px(100.), "eases out: {:?}", half[0]);
        assert_eq!(half[0].x + px(10.), half[1].x, "without a trail it keeps its shape");
        assert!(!caret.corners_at(start + ms(100)).unwrap().1, "settled");
        assert_eq!(caret.blink_from, start, "moving shows it solid again");
    }

    #[test]
    fn with_a_trail_the_front_leads_and_the_back_catches_up() {
        let ms = Duration::from_millis;
        let motion = Motion { glide: ms(100), trail: true };
        let (mut caret, start) = (Caret::default(), Instant::now());
        caret.aim(cell(0.), motion, start);
        caret.aim(cell(100.), motion, start);
        let (mid, _) = caret.corners_at(start + ms(40)).unwrap();
        let width = mid[1].x - mid[0].x;
        assert!(width > px(30.), "stretched along the way: {width:?}");
        let (end, moving) = caret.corners_at(start + ms(170)).unwrap();
        assert!(!moving && end[1].x - end[0].x == px(10.), "and back to a caret");
    }

    #[test]
    fn no_glide_means_a_jump() {
        let motion = Motion { glide: Duration::ZERO, trail: true };
        let (mut caret, start) = (Caret::default(), Instant::now());
        caret.aim(cell(0.), motion, start);
        caret.aim(cell(100.), motion, start);
        assert_eq!(caret.corners_at(start).unwrap().0[0], cell(100.).origin);
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

    #[test]
    fn it_blinks_with_fades() {
        let ms = Duration::from_millis;
        assert_eq!(blink(ms(0)), (1.0, false, ms(600)));
        let (alpha, fading, _) = blink(ms(655));
        assert!(fading && (alpha - 0.5).abs() < 0.01, "half faded out: {alpha}");
        assert_eq!(blink(ms(800)), (0.0, false, ms(290)));
        assert_eq!(blink(ms(1300)), blink(ms(100)), "and repeats");
    }
}
