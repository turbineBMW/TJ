//! Characters the terminal draws itself rather than taking from the font, as foot, VTE and
//! Ghostty do: box drawing (U+2500–257F), block elements (U+2580–259F) and the Powerline
//! separators (U+E0B0–E0BF).
//!
//! A font draws these for its own line height and its own idea of a shade, so in a grid whose
//! rows are taller than the font they leave gaps between rows, and `░▒▓` come out as dither
//! patterns. Drawn here, each one fills its cell exactly: lines meet the edges on whole device
//! pixels, shades are the foreground at a quarter, half and three quarters, and curves are
//! anti-aliased paths.

use gpui_kit::{Bounds, Hsla, PathBuilder, Pixels, Point, Size, Window, fill, point, px, size};

/// Whether `ch` is drawn by [`paint`] rather than taken from the font.
pub fn drawn(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{259F}' | '\u{E0B0}'..='\u{E0BF}')
}

/// Paint `ch` over one cell whose top-left corner `origin` is on a device pixel and whose
/// `cell` size is a whole number of device pixels. `font_size` sets the line weight.
pub fn paint(ch: char, origin: Point<Pixels>, cell: Size<Pixels>, font_size: Pixels, color: Hsla, window: &mut Window) {
    let scale = window.scale_factor();
    let w = (f32::from(cell.width) * scale).round();
    let h = (f32::from(cell.height) * scale).round();
    // About the stem of a regular-weight letter: one device pixel at 14px and 1x, two from 1.5x.
    let light = (f32::from(font_size) * scale / 12.0).round().max(1.0);
    let mut c = Canvas { origin, scale, w, h, light, color, window };
    let code = ch as u32;
    match code {
        0x2500..=0x257F => c.box_char(code),
        0x2580..=0x259F => c.block(code),
        0xE0B0..=0xE0BF => c.powerline(code),
        _ => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Weight {
    None,
    Light,
    Heavy,
    Double,
}

use Weight::{Double as D, Heavy as H, Light as L, None as N};

/// The arms of a box-drawing character: up, right, down, left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Arms {
    up: Weight,
    right: Weight,
    down: Weight,
    left: Weight,
}

const fn arms(up: Weight, right: Weight, down: Weight, left: Weight) -> Arms {
    Arms { up, right, down, left }
}

/// U+2500–254B and U+2550–256C, U+2574–257F: the line characters, by their arms.
fn lines_of(code: u32) -> Option<Arms> {
    Some(match code {
        0x2500 => arms(N, L, N, L),
        0x2501 => arms(N, H, N, H),
        0x2502 => arms(L, N, L, N),
        0x2503 => arms(H, N, H, N),
        0x250C => arms(N, L, L, N),
        0x250D => arms(N, H, L, N),
        0x250E => arms(N, L, H, N),
        0x250F => arms(N, H, H, N),
        0x2510 => arms(N, N, L, L),
        0x2511 => arms(N, N, L, H),
        0x2512 => arms(N, N, H, L),
        0x2513 => arms(N, N, H, H),
        0x2514 => arms(L, L, N, N),
        0x2515 => arms(L, H, N, N),
        0x2516 => arms(H, L, N, N),
        0x2517 => arms(H, H, N, N),
        0x2518 => arms(L, N, N, L),
        0x2519 => arms(L, N, N, H),
        0x251A => arms(H, N, N, L),
        0x251B => arms(H, N, N, H),
        0x251C => arms(L, L, L, N),
        0x251D => arms(L, H, L, N),
        0x251E => arms(H, L, L, N),
        0x251F => arms(L, L, H, N),
        0x2520 => arms(H, L, H, N),
        0x2521 => arms(H, H, L, N),
        0x2522 => arms(L, H, H, N),
        0x2523 => arms(H, H, H, N),
        0x2524 => arms(L, N, L, L),
        0x2525 => arms(L, N, L, H),
        0x2526 => arms(H, N, L, L),
        0x2527 => arms(L, N, H, L),
        0x2528 => arms(H, N, H, L),
        0x2529 => arms(H, N, L, H),
        0x252A => arms(L, N, H, H),
        0x252B => arms(H, N, H, H),
        0x252C => arms(N, L, L, L),
        0x252D => arms(N, L, L, H),
        0x252E => arms(N, H, L, L),
        0x252F => arms(N, H, L, H),
        0x2530 => arms(N, L, H, L),
        0x2531 => arms(N, L, H, H),
        0x2532 => arms(N, H, H, L),
        0x2533 => arms(N, H, H, H),
        0x2534 => arms(L, L, N, L),
        0x2535 => arms(L, L, N, H),
        0x2536 => arms(L, H, N, L),
        0x2537 => arms(L, H, N, H),
        0x2538 => arms(H, L, N, L),
        0x2539 => arms(H, L, N, H),
        0x253A => arms(H, H, N, L),
        0x253B => arms(H, H, N, H),
        0x253C => arms(L, L, L, L),
        0x253D => arms(L, L, L, H),
        0x253E => arms(L, H, L, L),
        0x253F => arms(L, H, L, H),
        0x2540 => arms(H, L, L, L),
        0x2541 => arms(L, L, H, L),
        0x2542 => arms(H, L, H, L),
        0x2543 => arms(H, L, L, H),
        0x2544 => arms(H, H, L, L),
        0x2545 => arms(L, L, H, H),
        0x2546 => arms(L, H, H, L),
        0x2547 => arms(H, H, L, H),
        0x2548 => arms(L, H, H, H),
        0x2549 => arms(H, L, H, H),
        0x254A => arms(H, H, H, L),
        0x254B => arms(H, H, H, H),
        0x2550 => arms(N, D, N, D),
        0x2551 => arms(D, N, D, N),
        0x2552 => arms(N, D, L, N),
        0x2553 => arms(N, L, D, N),
        0x2554 => arms(N, D, D, N),
        0x2555 => arms(N, N, L, D),
        0x2556 => arms(N, N, D, L),
        0x2557 => arms(N, N, D, D),
        0x2558 => arms(L, D, N, N),
        0x2559 => arms(D, L, N, N),
        0x255A => arms(D, D, N, N),
        0x255B => arms(L, N, N, D),
        0x255C => arms(D, N, N, L),
        0x255D => arms(D, N, N, D),
        0x255E => arms(L, D, L, N),
        0x255F => arms(D, L, D, N),
        0x2560 => arms(D, D, D, N),
        0x2561 => arms(L, N, L, D),
        0x2562 => arms(D, N, D, L),
        0x2563 => arms(D, N, D, D),
        0x2564 => arms(N, D, L, D),
        0x2565 => arms(N, L, D, L),
        0x2566 => arms(N, D, D, D),
        0x2567 => arms(L, D, N, D),
        0x2568 => arms(D, L, N, L),
        0x2569 => arms(D, D, N, D),
        0x256A => arms(L, D, L, D),
        0x256B => arms(D, L, D, L),
        0x256C => arms(D, D, D, D),
        0x2574 => arms(N, N, N, L),
        0x2575 => arms(L, N, N, N),
        0x2576 => arms(N, L, N, N),
        0x2577 => arms(N, N, L, N),
        0x2578 => arms(N, N, N, H),
        0x2579 => arms(H, N, N, N),
        0x257A => arms(N, H, N, N),
        0x257B => arms(N, N, H, N),
        0x257C => arms(N, H, N, L),
        0x257D => arms(L, N, H, N),
        0x257E => arms(N, L, N, H),
        0x257F => arms(H, N, L, N),
        _ => return None,
    })
}

/// Bézier control distance for a quarter circle, as a fraction of its radius.
const KAPPA: f32 = 0.552_284_8;

/// One cell, measured in device pixels from its top-left corner.
struct Canvas<'a> {
    origin: Point<Pixels>,
    scale: f32,
    w: f32,
    h: f32,
    light: f32,
    color: Hsla,
    window: &'a mut Window,
}

impl Canvas<'_> {
    fn heavy(&self) -> f32 {
        self.light * 2.0
    }

    /// A point `x`, `y` device pixels into the cell.
    fn at(&self, x: f32, y: f32) -> Point<Pixels> {
        point(self.origin.x + px(x / self.scale), self.origin.y + px(y / self.scale))
    }

    /// Fill the rectangle from `x0`, `y0` to `x1`, `y1` (device pixels in the cell).
    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.rect_in(x0, y0, x1, y1, self.color);
    }

    fn rect_in(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, color: Hsla) {
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let bounds = Bounds::new(self.at(x0, y0), size(px((x1 - x0) / self.scale), px((y1 - y0) / self.scale)));
        self.window.paint_quad(fill(bounds, color));
    }

    fn draw(&mut self, path: PathBuilder) {
        if let Ok(path) = path.build() {
            self.window.paint_path(path, self.color);
        }
    }

    /// Where a horizontal stroke `t` pixels thick starts, centred in the cell's height.
    fn h_top(&self, t: f32) -> f32 {
        ((self.h - t) / 2.0).floor()
    }

    /// Where a vertical stroke `t` pixels thick starts, centred in the cell's width.
    fn v_left(&self, t: f32) -> f32 {
        ((self.w - t) / 2.0).floor()
    }

    // ---------- box drawing ----------

    fn box_char(&mut self, code: u32) {
        let (light, heavy) = (self.light, self.heavy());
        match code {
            // Dashed lines: triple (┄┅┆┇), quadruple (┈┉┊┋) and double (╌╍╎╏).
            0x2504 => self.dashes_h(3, light),
            0x2505 => self.dashes_h(3, heavy),
            0x2506 => self.dashes_v(3, light),
            0x2507 => self.dashes_v(3, heavy),
            0x2508 => self.dashes_h(4, light),
            0x2509 => self.dashes_h(4, heavy),
            0x250A => self.dashes_v(4, light),
            0x250B => self.dashes_v(4, heavy),
            0x254C => self.dashes_h(2, light),
            0x254D => self.dashes_h(2, heavy),
            0x254E => self.dashes_v(2, light),
            0x254F => self.dashes_v(2, heavy),
            0x256D..=0x2570 => self.arc(code),
            0x2571..=0x2573 => self.diagonals(code),
            _ => {
                if let Some(a) = lines_of(code) {
                    self.lines(a);
                }
            }
        }
    }

    /// Straight arms from the centre to each edge. Arms of different weights meet the way
    /// fonts draw them: a heavy crossing line covers the joint, and double lines leave the
    /// inside of a corner open.
    fn lines(&mut self, a: Arms) {
        let (w, h, light, heavy) = (self.w, self.h, self.light, self.heavy());
        let h_light_top = self.h_top(light);
        let h_light_bottom = h_light_top + light;
        let h_heavy_top = self.h_top(heavy);
        let h_heavy_bottom = h_heavy_top + heavy;
        // A double line is two light strokes with a light stroke's gap between them.
        let h_double_top = h_light_top - light;
        let h_double_bottom = h_light_bottom + light;
        let v_light_left = self.v_left(light);
        let v_light_right = v_light_left + light;
        let v_heavy_left = self.v_left(heavy);
        let v_heavy_right = v_heavy_left + heavy;
        let v_double_left = v_light_left - light;
        let v_double_right = v_light_right + light;

        let any = |x: Weight, y: Weight, of: Weight| x == of || y == of;
        // Where the up arm stops: through the horizontal arms when they're heavy or double,
        // to its far side when it carries on down, otherwise just past the centre.
        let up_bottom = if any(a.left, a.right, H) {
            h_heavy_bottom
        } else if a.left != a.right || a.down == a.up {
            if any(a.left, a.right, D) { h_double_bottom } else { h_light_bottom }
        } else if a.left == N && a.right == N {
            h_light_bottom
        } else {
            h_light_top
        };
        let down_top = if any(a.left, a.right, H) {
            h_heavy_top
        } else if a.left != a.right || a.up == a.down {
            if any(a.left, a.right, D) { h_double_top } else { h_light_top }
        } else if a.left == N && a.right == N {
            h_light_top
        } else {
            h_light_bottom
        };
        let left_right = if any(a.up, a.down, H) {
            v_heavy_right
        } else if a.up != a.down || a.left == a.right {
            if any(a.up, a.down, D) { v_double_right } else { v_light_right }
        } else if a.up == N && a.down == N {
            v_light_right
        } else {
            v_light_left
        };
        let right_left = if any(a.up, a.down, H) {
            v_heavy_left
        } else if a.up != a.down || a.right == a.left {
            if any(a.up, a.down, D) { v_double_left } else { v_light_left }
        } else if a.up == N && a.down == N {
            v_light_left
        } else {
            v_light_right
        };

        match a.up {
            N => {}
            L => self.rect(v_light_left, 0.0, v_light_right, up_bottom),
            H => self.rect(v_heavy_left, 0.0, v_heavy_right, up_bottom),
            D => {
                let left_bottom = if a.left == D { h_light_top } else { up_bottom };
                let right_bottom = if a.right == D { h_light_top } else { up_bottom };
                self.rect(v_double_left, 0.0, v_light_left, left_bottom);
                self.rect(v_light_right, 0.0, v_double_right, right_bottom);
            }
        }
        match a.right {
            N => {}
            L => self.rect(right_left, h_light_top, w, h_light_bottom),
            H => self.rect(right_left, h_heavy_top, w, h_heavy_bottom),
            D => {
                let top_left = if a.up == D { v_light_right } else { right_left };
                let bottom_left = if a.down == D { v_light_right } else { right_left };
                self.rect(top_left, h_double_top, w, h_light_top);
                self.rect(bottom_left, h_light_bottom, w, h_double_bottom);
            }
        }
        match a.down {
            N => {}
            L => self.rect(v_light_left, down_top, v_light_right, h),
            H => self.rect(v_heavy_left, down_top, v_heavy_right, h),
            D => {
                let left_top = if a.left == D { h_light_bottom } else { down_top };
                let right_top = if a.right == D { h_light_bottom } else { down_top };
                self.rect(v_double_left, left_top, v_light_left, h);
                self.rect(v_light_right, right_top, v_double_right, h);
            }
        }
        match a.left {
            N => {}
            L => self.rect(0.0, h_light_top, left_right, h_light_bottom),
            H => self.rect(0.0, h_heavy_top, left_right, h_heavy_bottom),
            D => {
                let top_right = if a.up == D { v_light_left } else { left_right };
                let bottom_right = if a.down == D { v_light_left } else { left_right };
                self.rect(0.0, h_double_top, top_right, h_light_top);
                self.rect(0.0, h_light_bottom, bottom_right, h_double_bottom);
            }
        }
    }

    /// `n` dashes across the cell, each centred in its share so a row of cells keeps an even
    /// rhythm across cell edges.
    fn dashes_h(&mut self, n: u32, t: f32) {
        let top = self.h_top(t);
        let share = self.w / n as f32;
        let gap = (share / 3.0).round().max(1.0);
        for i in 0..n {
            let x0 = (share * i as f32 + gap / 2.0).round();
            let x1 = (share * (i + 1) as f32 - gap / 2.0).round();
            self.rect(x0, top, x1, top + t);
        }
    }

    fn dashes_v(&mut self, n: u32, t: f32) {
        let left = self.v_left(t);
        let share = self.h / n as f32;
        let gap = (share / 3.0).round().max(1.0);
        for i in 0..n {
            let y0 = (share * i as f32 + gap / 2.0).round();
            let y1 = (share * (i + 1) as f32 - gap / 2.0).round();
            self.rect(left, y0, left + t, y1);
        }
    }

    /// ╭╮╯╰: a light line from one edge's centre, round a quarter circle, to the next edge.
    fn arc(&mut self, code: u32) {
        let (w, h, light) = (self.w, self.h, self.light);
        // The middle of the light strokes, so the arc's ends meet ─ and │ exactly.
        let cx = self.v_left(light) + light / 2.0;
        let cy = self.h_top(light) + light / 2.0;
        // Away from the centre: right or left, and down or up.
        let (sx, sy): (f32, f32) = match code {
            0x256D => (1.0, 1.0),
            0x256E => (-1.0, 1.0),
            0x256F => (-1.0, -1.0),
            _ => (1.0, -1.0),
        };
        let r = (w / 2.0).min(h / 2.0);
        let vertical_end = if sy > 0.0 { h } else { 0.0 };
        let horizontal_end = if sx > 0.0 { w } else { 0.0 };
        let k = r * KAPPA;
        let mut path = PathBuilder::stroke(px(light / self.scale));
        path.move_to(self.at(cx, vertical_end));
        path.line_to(self.at(cx, cy + sy * r));
        path.cubic_bezier_to(self.at(cx + sx * r, cy), self.at(cx, cy + sy * (r - k)), self.at(cx + sx * (r - k), cy));
        path.line_to(self.at(horizontal_end, cy));
        self.draw(path);
    }

    /// ╱╲╳: corner to corner.
    fn diagonals(&mut self, code: u32) {
        let (w, h) = (self.w, self.h);
        let mut path = PathBuilder::stroke(px(self.light / self.scale));
        if code != 0x2572 {
            path.move_to(self.at(w, 0.0));
            path.line_to(self.at(0.0, h));
        }
        if code != 0x2571 {
            path.move_to(self.at(0.0, 0.0));
            path.line_to(self.at(w, h));
        }
        self.draw(path);
    }

    // ---------- block elements ----------

    fn block(&mut self, code: u32) {
        let (w, h) = (self.w, self.h);
        let x = |f: f32| (w * f).round();
        let y = |f: f32| (h * f).round();
        match code {
            0x2580 => self.rect(0.0, 0.0, w, y(0.5)),
            // ▁ to ▇: the lower eighths.
            0x2581..=0x2587 => self.rect(0.0, h - y((code - 0x2580) as f32 / 8.0), w, h),
            0x2588 => self.rect(0.0, 0.0, w, h),
            // ▉ to ▏: the left eighths, seven down to one.
            0x2589..=0x258F => self.rect(0.0, 0.0, x((0x2590 - code) as f32 / 8.0), h),
            0x2590 => self.rect(x(0.5), 0.0, w, h),
            // ░▒▓: the colour at a quarter, half and three quarters, not a dither.
            0x2591..=0x2593 => {
                let color = self.color.opacity((code - 0x2590) as f32 / 4.0);
                self.rect_in(0.0, 0.0, w, h, color);
            }
            0x2594 => self.rect(0.0, 0.0, w, y(1.0 / 8.0)),
            0x2595 => self.rect(w - x(1.0 / 8.0), 0.0, w, h),
            0x2596..=0x259F => {
                // Upper left, upper right, lower left, lower right.
                let [ul, ur, ll, lr] = match code {
                    0x2596 => [false, false, true, false],
                    0x2597 => [false, false, false, true],
                    0x2598 => [true, false, false, false],
                    0x2599 => [true, false, true, true],
                    0x259A => [true, false, false, true],
                    0x259B => [true, true, true, false],
                    0x259C => [true, true, false, true],
                    0x259D => [false, true, false, false],
                    0x259E => [false, true, true, false],
                    _ => [false, true, true, true],
                };
                let (mx, my) = (x(0.5), y(0.5));
                if ul {
                    self.rect(0.0, 0.0, mx, my);
                }
                if ur {
                    self.rect(mx, 0.0, w, my);
                }
                if ll {
                    self.rect(0.0, my, mx, h);
                }
                if lr {
                    self.rect(mx, my, w, h);
                }
            }
            _ => {}
        }
    }

    // ---------- Powerline ----------

    fn powerline(&mut self, code: u32) {
        let (w, h, light) = (self.w, self.h, self.light);
        // Thin outlines sit half a stroke inside the cell, so they aren't clipped at its edge.
        let inset = light / 2.0;
        let stroke = || PathBuilder::stroke(px(light / self.scale));
        match code {
            //  and : a triangle pointing right or left.
            0xE0B0 | 0xE0B2 => {
                let (base, tip) = if code == 0xE0B0 { (0.0, w) } else { (w, 0.0) };
                let mut path = PathBuilder::fill();
                path.move_to(self.at(base, 0.0));
                path.line_to(self.at(tip, h / 2.0));
                path.line_to(self.at(base, h));
                path.close();
                self.draw(path);
            }
            //  and : the triangle's outline, without its base.
            0xE0B1 | 0xE0B3 => {
                let (base, tip) = if code == 0xE0B1 { (0.0, w - inset) } else { (w, inset) };
                let mut path = stroke();
                path.move_to(self.at(base, 0.0));
                path.line_to(self.at(tip, h / 2.0));
                path.line_to(self.at(base, h));
                self.draw(path);
            }
            //  and : a half ellipse one cell wide and one tall, bulging right or left.
            0xE0B4 | 0xE0B6 => {
                let mut path = PathBuilder::fill();
                self.half_ellipse(&mut path, code == 0xE0B4, 0.0);
                path.close();
                self.draw(path);
            }
            //  and : its outline.
            0xE0B5 | 0xE0B7 => {
                let mut path = stroke();
                self.half_ellipse(&mut path, code == 0xE0B5, inset);
                self.draw(path);
            }
            // , , , : a triangle filling half the cell, cut on a diagonal.
            0xE0B8 | 0xE0BA | 0xE0BC | 0xE0BE => {
                let corners = match code {
                    0xE0B8 => [(0.0, 0.0), (0.0, h), (w, h)],
                    0xE0BA => [(w, 0.0), (w, h), (0.0, h)],
                    0xE0BC => [(0.0, 0.0), (w, 0.0), (0.0, h)],
                    _ => [(0.0, 0.0), (w, 0.0), (w, h)],
                };
                let mut path = PathBuilder::fill();
                path.move_to(self.at(corners[0].0, corners[0].1));
                path.line_to(self.at(corners[1].0, corners[1].1));
                path.line_to(self.at(corners[2].0, corners[2].1));
                path.close();
                self.draw(path);
            }
            //  and : corner to corner down to the right;  and : up to the right.
            _ => {
                let mut path = stroke();
                if code == 0xE0B9 || code == 0xE0BF {
                    path.move_to(self.at(0.0, 0.0));
                    path.line_to(self.at(w, h));
                } else {
                    path.move_to(self.at(w, 0.0));
                    path.line_to(self.at(0.0, h));
                }
                self.draw(path);
            }
        }
    }

    /// From the top of the flat side, round to the bottom of it, `inset` inside the cell.
    fn half_ellipse(&self, path: &mut PathBuilder, right: bool, inset: f32) {
        let (w, h) = (self.w, self.h);
        let (flat, sign) = if right { (0.0, 1.0) } else { (w, -1.0) };
        let rx = w - inset;
        let ry = h / 2.0 - inset;
        let cy = h / 2.0;
        let (kx, ky) = (rx * KAPPA, ry * KAPPA);
        let top = cy - ry;
        let bottom = cy + ry;
        let far = flat + sign * rx;
        path.move_to(self.at(flat, top));
        path.cubic_bezier_to(self.at(far, cy), self.at(flat + sign * kx, top), self.at(far, cy - ky));
        path.cubic_bezier_to(self.at(flat, bottom), self.at(far, cy + ky), self.at(flat + sign * kx, bottom));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_character_has_arms() {
        let others = [0x2504..=0x250B, 0x254C..=0x254F, 0x256D..=0x2573];
        for code in 0x2500..=0x257F {
            if others.iter().any(|r| r.contains(&code)) {
                continue;
            }
            assert!(lines_of(code).is_some(), "U+{code:04X} has no arms");
        }
    }

    #[test]
    fn only_graphics_are_drawn() {
        assert!(drawn('░') && drawn('│') && drawn('╭') && drawn('\u{E0B4}') && drawn('▟'));
        assert!(!drawn('a') && !drawn('⠿') && !drawn('\u{E0A0}') && !drawn('\u{E0C0}'));
    }
}
