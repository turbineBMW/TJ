//! A SIXEL decoder. The body is what follows `ESC P` up to the `ESC \`: the parameters
//! (digits and `;`), the `q`, then the sixel data.
//!
//! Each data character (`?` to `~`) paints a column of six pixels in the current colour;
//! `!n` repeats the next one, `$` goes back to the start of the band, `-` starts the next band,
//! `#n` picks colour n and `#n;2;r;g;b` (percentages) or `#n;1;h;l;s` defines it, and
//! `"a;b;w;h` gives the size. Pixels nothing paints stay transparent.

use super::MAX_SIDE;

/// The VT340's sixteen colours (as percentages), the palette SIXEL starts with.
const VT340: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (20, 20, 80),
    (80, 13, 13),
    (20, 80, 20),
    (80, 20, 80),
    (20, 80, 80),
    (80, 80, 20),
    (53, 53, 53),
    (26, 26, 26),
    (33, 33, 60),
    (60, 26, 26),
    (33, 60, 33),
    (60, 33, 60),
    (33, 60, 60),
    (60, 60, 33),
    (80, 80, 80),
];

fn percent(p: u32) -> u8 {
    ((p.min(100) * 255 + 50) / 100) as u8
}

/// DEC's HLS: hue 0 is blue, 120 red, 240 green (standard HSL hue + 240).
fn hls(h: u32, l: u32, s: u32) -> [u8; 3] {
    let (h, l, s) = (((h + 240) % 360) as f32, l.min(100) as f32 / 100.0, s.min(100) as f32 / 100.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r, g, b].map(|v| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// Read a decimal number at `i`, moving past it.
fn number(data: &[u8], i: &mut usize) -> u32 {
    let mut n: u32 = 0;
    while let Some(d) = data.get(*i).filter(|b| b.is_ascii_digit()) {
        n = n.saturating_mul(10).saturating_add((d - b'0') as u32);
        *i += 1;
    }
    n
}

/// Read `n;n;...` at `i`.
fn numbers(data: &[u8], i: &mut usize) -> Vec<u32> {
    let mut out = vec![number(data, i)];
    while data.get(*i) == Some(&b';') {
        *i += 1;
        out.push(number(data, i));
    }
    out
}

pub fn decode(body: &[u8]) -> Option<image::RgbaImage> {
    // Skip the DCS parameters (P1;P2;P3) and the `q` to the data.
    let start = body.iter().position(|b| *b == b'q').map(|i| i + 1).unwrap_or(0);
    let data = &body[start..];
    let mut palette: Vec<[u8; 3]> =
        VT340.iter().map(|(r, g, b)| [percent(*r as u32), percent(*g as u32), percent(*b as u32)]).collect();
    palette.resize(256, [0, 0, 0]);
    let mut colour = 0usize;
    let (mut width, mut height) = (0usize, 0usize);
    let mut pixels: Vec<u32> = Vec::new();
    let mut stride = 0usize;
    let (mut x, mut y) = (0usize, 0usize);
    let max = MAX_SIDE as usize;

    let mut grow = |pixels: &mut Vec<u32>, stride: &mut usize, w: usize, h: usize| {
        let (w, h) = (w.min(max), h.min(max));
        let rows = pixels.len().checked_div(*stride).unwrap_or(0);
        if w > *stride || h > rows {
            let new_stride = w.max(*stride).next_power_of_two().min(max);
            let new_rows = h.max(rows);
            let mut grown = vec![0u32; new_stride * new_rows];
            for r in 0..rows {
                grown[r * new_stride..r * new_stride + *stride]
                    .copy_from_slice(&pixels[r * *stride..(r + 1) * *stride]);
            }
            *pixels = grown;
            *stride = new_stride;
        }
    };

    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        i += 1;
        match b {
            b'"' => {
                let n = numbers(data, &mut i);
                if let (Some(&w), Some(&h)) = (n.get(2), n.get(3)) {
                    grow(&mut pixels, &mut stride, w as usize, h as usize);
                }
            }
            b'#' => {
                let n = numbers(data, &mut i);
                colour = n[0] as usize % 256;
                if n.len() >= 5 {
                    palette[colour] = match n[1] {
                        1 => hls(n[2], n[3], n[4]),
                        _ => [percent(n[2]), percent(n[3]), percent(n[4])],
                    };
                }
            }
            b'!' => {
                let count = number(data, &mut i).max(1) as usize;
                if let Some(&c) = data.get(i) {
                    i += 1;
                    if (b'?'..=b'~').contains(&c) {
                        paint(
                            c,
                            count,
                            &mut x,
                            y,
                            palette[colour],
                            &mut pixels,
                            &mut stride,
                            &mut width,
                            &mut height,
                            &mut grow,
                        );
                    }
                }
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                y += 6;
            }
            b'?'..=b'~' => {
                paint(b, 1, &mut x, y, palette[colour], &mut pixels, &mut stride, &mut width, &mut height, &mut grow)
            }
            _ => {}
        }
    }
    if width == 0 || height == 0 {
        return None;
    }
    let mut out = image::RgbaImage::new(width as u32, height as u32);
    for (py, row) in out.rows_mut().enumerate() {
        for (px, p) in row.enumerate() {
            let v = pixels[py * stride + px];
            p.0 = v.to_le_bytes();
        }
    }
    Some(out)
}

#[allow(clippy::too_many_arguments)]
fn paint(
    c: u8,
    count: usize,
    x: &mut usize,
    y: usize,
    rgb: [u8; 3],
    pixels: &mut Vec<u32>,
    stride: &mut usize,
    width: &mut usize,
    height: &mut usize,
    grow: &mut impl FnMut(&mut Vec<u32>, &mut usize, usize, usize),
) {
    let bits = c - b'?';
    let max = MAX_SIDE as usize;
    if *x >= max || y >= max {
        *x += count;
        return;
    }
    let end = (*x + count).min(max);
    if bits != 0 {
        let bottom = (y + 6).min(max);
        grow(pixels, stride, end, bottom);
        let value = u32::from_le_bytes([rgb[0], rgb[1], rgb[2], 255]);
        for bit in 0..6 {
            if bits & (1 << bit) != 0 && y + bit < max {
                let row = (y + bit) * *stride;
                pixels[row + *x..row + end].fill(value);
                *height = (*height).max(y + bit + 1);
            }
        }
        *width = (*width).max(end);
    }
    *x += count;
}
