//! Typed letters grow in. In the prompt, a letter a key just typed starts at 60% of its size and
//! fades in while it grows to full size, over the caret's glide and on the same curve, so it
//! fills in behind the caret as it slides on.
//!
//! gpui-kit draws the field's text, so the real letter is covered with the colour behind the
//! text and a growing copy is drawn over it until it reaches full size. That only happens when
//! it can't show: one letter typed at the caret by a key, on the caret's line, over an opaque
//! background, with a copy exactly as wide as the real letter (so the font matches and no
//! kerning moved it). Anything else, such as a paste, a deletion, a word wrapping to the next
//! line or the field growing, shows the text as it is.

use std::time::{Duration, Instant};

use gpui_kit::*;

use crate::caret::{Clock, ease, progress};

/// A new letter starts at this share of its size.
const FROM_SCALE: f32 = 0.6;
/// How far (pixels) the copy's width may be from the room the letter took.
const WIDTH_SLACK: f32 = 0.75;

/// What one key typed: `new` is `old` with one visible character put in at the old cursor, and
/// the cursor just after it. Returns where it went and the character.
pub fn typed<'a>(old: &str, old_cursor: usize, new: &'a str, new_cursor: usize) -> Option<(usize, &'a str)> {
    let at = old_cursor;
    let len = new.len().checked_sub(old.len()).filter(|len| *len > 0)?;
    if new_cursor != at + len {
        return None;
    }
    let inserted = new.get(at..new_cursor)?;
    let mut chars = inserted.chars();
    let one = chars.next().filter(|c| !c.is_whitespace() && !c.is_control()).is_some() && chars.next().is_none();
    let rest_kept = new.get(..at)? == old.get(..at)? && new.get(new_cursor..)? == old.get(at..)?;
    (one && rest_kept).then_some((at, inserted))
}

/// A field as drawn this frame.
pub struct Seen {
    pub text: SharedString,
    pub cursor: usize,
    /// The caret's spot: its x, the top of its line, and the line's height (no width).
    pub caret: Bounds<Pixels>,
    /// The field's layout fingerprint, as the caret's ([`crate::caret::Aim::view`]).
    pub view: u64,
}

/// A letter growing in.
#[derive(Clone, Debug, PartialEq)]
struct Pop {
    text: SharedString,
    /// Where in the text it is (bytes).
    at: usize,
    /// The room it takes: from the caret before it to the caret after, the line's height.
    cell: Bounds<Pixels>,
    started: Instant,
}

/// One field's growing letters, and what it looked like last frame.
#[derive(Default)]
pub struct Grow {
    text: SharedString,
    cursor: usize,
    caret: Option<Bounds<Pixels>>,
    view: u64,
    frame: u64,
    pops: Vec<Pop>,
}

impl Grow {
    /// Step to this frame: start a letter growing if a key just typed one, and retire the
    /// grown ones. `measure` is how wide the copy of a letter comes out at full size.
    pub fn step(&mut self, seen: Seen, clock: &Clock, length: Duration, measure: impl FnOnce(&str) -> Option<Pixels>) {
        let now = clock.now.unwrap_or_else(Instant::now);
        let continuous = clock.frame.wrapping_sub(self.frame) <= 1 && seen.view == self.view && !length.is_zero();
        match self.caret.filter(|_| continuous) {
            None => self.pops.clear(),
            Some(_) if seen.text == self.text => {}
            Some(before) => {
                let after = seen.caret;
                let typed = typed(&self.text, self.cursor, &seen.text, seen.cursor)
                    .filter(|_| clock.typing(now))
                    .filter(|_| (f32::from(before.origin.y) - f32::from(after.origin.y)).abs() < 0.5)
                    .filter(|_| after.origin.x > before.origin.x);
                match typed {
                    Some((at, letter)) => {
                        // Letters after it moved along: they are simply there.
                        self.pops.retain(|p| p.at < at);
                        let cell =
                            Bounds::new(before.origin, size(after.origin.x - before.origin.x, after.size.height));
                        let fits =
                            measure(letter).is_some_and(|w| (f32::from(w - cell.size.width)).abs() <= WIDTH_SLACK);
                        if fits {
                            self.pops.push(Pop { text: letter.to_string().into(), at, cell, started: now });
                        }
                    }
                    None => self.pops.clear(),
                }
            }
        }
        self.pops.retain(|p| progress(now, p.started, length).is_some());
        self.text = seen.text;
        self.cursor = seen.cursor;
        self.caret = Some(seen.caret);
        self.view = seen.view;
        self.frame = clock.frame;
    }

    pub fn is_growing(&self) -> bool {
        !self.pops.is_empty()
    }

    #[cfg(test)]
    pub fn growing(&self) -> usize {
        self.pops.len()
    }

    /// Each growing letter now: its room, its scale and its opacity.
    fn shapes(&self, now: Instant, length: Duration) -> impl Iterator<Item = (&Pop, f32, f32)> {
        self.pops.iter().filter_map(move |p| {
            let e = ease(progress(now, p.started, length)?);
            Some((p, FROM_SCALE + (1.0 - FROM_SCALE) * e, e))
        })
    }

    /// Cover each growing letter with `behind` and draw its copy in `ink`, growing about the
    /// middle of its room.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        now: Instant,
        length: Duration,
        behind: Hsla,
        ink: Hsla,
        font: &Font,
        font_size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (pop, scale, alpha) in self.shapes(now, length) {
            window.paint_quad(fill(pop.cell, behind));
            let line = shape(&pop.text, font, font_size * scale, ink.opacity(alpha), window);
            let line_height = pop.cell.size.height * scale;
            let centre = pop.cell.center();
            let origin = point(centre.x - line.width / 2., centre.y - line_height / 2.);
            let _ = line.paint(origin, line_height, TextAlign::Left, None, window, cx);
        }
    }
}

/// `text` laid out on one line.
pub fn shape(text: &SharedString, font: &Font, font_size: Pixels, color: Hsla, window: &Window) -> ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window.text_system().shape_line(text.clone(), font_size, &[run], None)
}

#[cfg(test)]
mod tests {
    use super::{Grow, Seen, typed};
    use crate::caret::{Clock, Stamp};
    use gpui_kit::{Bounds, Pixels, point, px, size};
    use std::time::{Duration, Instant};

    const LENGTH: Duration = Duration::from_millis(100);

    #[test]
    fn only_one_typed_letter_counts() {
        assert_eq!(typed("helo", 3, "hello", 4), Some((3, "l")), "in the middle");
        assert_eq!(typed("", 0, "é", 2), Some((0, "é")));
        assert_eq!(typed("hi", 2, "hi ", 3), None, "a space has nothing to grow");
        assert_eq!(typed("hi", 2, "hi!?", 4), None, "a paste");
        assert_eq!(typed("hello", 5, "hell", 4), None, "a deletion");
        assert_eq!(typed("ab", 2, "xab", 1), None, "not at the caret");
        assert_eq!(typed("ab", 0, "abc", 3), None, "the cursor wasn't there");
    }

    /// A field whose letters are 8 pixels wide on 20-pixel lines.
    struct Field {
        clock: Clock,
        start: Instant,
        grow: Grow,
    }

    impl Field {
        fn new() -> Self {
            Self { clock: Clock::default(), start: Instant::now(), grow: Grow::default() }
        }
        fn caret(column: usize, line: usize) -> Bounds<Pixels> {
            Bounds::new(point(px(8. * column as f32), px(20. * line as f32)), size(px(0.), px(20.)))
        }
        fn key(&mut self, t: u64) {
            self.clock.key = Some(Stamp { frame: self.clock.frame, at: self.start + Duration::from_millis(t) });
        }
        fn draw(&mut self, text: &str, cursor: usize, line: usize, t: u64) -> usize {
            self.draw_in(text, cursor, line, 1, 8., t)
        }
        fn draw_in(&mut self, text: &str, cursor: usize, line: usize, view: u64, width: f32, t: u64) -> usize {
            self.clock.frame += 1;
            self.clock.now = Some(self.start + Duration::from_millis(t));
            let seen = Seen { text: text.to_string().into(), cursor, caret: Self::caret(cursor, line), view };
            self.grow.step(seen, &self.clock, LENGTH, |_| Some(px(width)));
            self.grow.pops.len()
        }
    }

    #[test]
    fn a_typed_letter_grows_into_the_room_it_took() {
        let mut f = Field::new();
        f.draw("ab", 2, 0, 0);
        f.key(10);
        assert_eq!(f.draw("abc", 3, 0, 16), 1);
        let pop = &f.grow.pops[0];
        assert_eq!(pop.cell, Bounds::new(point(px(16.), px(0.)), size(px(8.), px(20.))));
        let now = f.start + Duration::from_millis(16);
        let (_, scale, alpha) = f.grow.shapes(now, LENGTH).next().unwrap();
        assert!((scale - 0.6).abs() < 0.01 && alpha < 0.01, "starts small and clear: {scale} {alpha}");
        let (_, scale, alpha) = f.grow.shapes(now + Duration::from_millis(50), LENGTH).next().unwrap();
        assert!(scale > 0.9 && alpha > 0.85, "most of the way at half time: {scale} {alpha}");
        assert_eq!(f.draw("abc", 3, 0, 120), 0, "then it's the field's own letter");
    }

    #[test]
    fn nothing_grows_unless_a_key_typed_it_in_place() {
        let mut f = Field::new();
        f.draw("ab", 2, 0, 0);
        assert_eq!(f.draw("abc", 3, 0, 16), 0, "no key: the app set it");

        let mut f = Field::new();
        f.draw("ab", 2, 0, 0);
        f.key(10);
        assert_eq!(f.draw_in("abc", 3, 0, 1, 11., 16), 0, "the copy wouldn't match");

        let mut f = Field::new();
        f.draw("ab", 2, 0, 0);
        f.key(10);
        assert_eq!(f.draw_in("abc", 3, 0, 2, 8., 16), 0, "the field changed");
    }

    #[test]
    fn a_wrap_a_deletion_or_typing_before_them_ends_the_growing() {
        let mut f = Field::new();
        f.draw("ab", 2, 0, 0);
        f.key(10);
        f.draw("abc", 3, 0, 16);
        f.key(20);
        assert_eq!(f.draw("abcd", 4, 0, 32), 2, "two at once");
        f.key(40);
        assert_eq!(f.draw("abcd e", 1, 1, 48), 0, "the word wrapped");

        let mut f = Field::new();
        f.draw("ab", 2, 0, 0);
        f.key(10);
        f.draw("abc", 3, 0, 16);
        f.key(20);
        assert_eq!(f.draw("ab", 2, 0, 32), 0, "deleted");

        let mut f = Field::new();
        f.draw("ab", 1, 0, 0);
        f.key(5);
        f.draw("axb", 2, 0, 16);
        f.draw("axb", 1, 0, 24);
        f.key(30);
        assert_eq!(f.draw("ayxb", 2, 0, 40), 1, "the earlier one moved along, only the new one grows");
        assert_eq!(&*f.grow.pops[0].text, "y");
    }
}
