//! iTerm2's inline images: `ESC ] 1337 ; File = key=value;... : <base64> BEL`.
//!
//! Only `inline=1` files are shown. `width` and `height` are cells (`N`), pixels (`Npx`), a
//! share of the terminal (`N%`) or `auto`; `preserveAspectRatio=0` stretches; and
//! `doNotMoveCursor=1` leaves the cursor where it was.

use base64::Engine as _;

use super::{Geometry, MAX_SIDE};

pub struct File {
    pub image: image::RgbaImage,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub keep_shape: bool,
    pub stay: bool,
}

/// A size in cells: `N` cells, `Npx` pixels, `N%` of the terminal, or `auto` (`None`).
fn size(value: &str, cell: f32, total: u16) -> Option<u16> {
    let value = value.trim();
    let cells = if let Some(px) = value.strip_suffix("px") {
        px.parse::<f32>().ok()? / cell
    } else if let Some(pct) = value.strip_suffix('%') {
        pct.parse::<f32>().ok()? / 100.0 * total as f32
    } else if value == "auto" {
        return None;
    } else {
        value.parse::<f32>().ok()?
    };
    (cells > 0.0).then(|| cells.ceil().min(u16::MAX as f32) as u16)
}

pub fn parse(body: &[u8], g: &Geometry) -> Option<File> {
    let colon = body.iter().position(|b| *b == b':')?;
    let (args, payload) = (std::str::from_utf8(&body[..colon]).ok()?, &body[colon + 1..]);
    let mut file = File { image: image::RgbaImage::new(1, 1), cols: None, rows: None, keep_shape: true, stay: false };
    let mut inline = false;
    for pair in args.split(';') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        match key {
            "inline" => inline = value == "1",
            "width" => file.cols = size(value, g.cell_width, g.cols),
            "height" => file.rows = size(value, g.cell_height, g.rows),
            "preserveAspectRatio" => file.keep_shape = value != "0",
            "doNotMoveCursor" => file.stay = value == "1",
            _ => {}
        }
    }
    if !inline {
        return None;
    }
    let clean: Vec<u8> = payload.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(clean).ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    if image.width() > MAX_SIDE || image.height() > MAX_SIDE {
        return None;
    }
    file.image = image.to_rgba8();
    Some(file)
}
