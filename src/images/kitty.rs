//! The kitty graphics protocol: `ESC _ G <key=value,...> ; <base64 payload> ESC \`.
//!
//! Supported: transmit (`a=t`), transmit and display (`a=T`), display (`a=p`), query
//! (`a=q`) and delete (`a=d`); PNG (`f=100`), RGB (`f=24`) and RGBA (`f=32`), zlib
//! compression (`o=z`); direct data, files, temporary files and shared memory (`t=d/f/t/s`);
//! chunked uploads (`m=1`); image ids and numbers; `c`/`r` sizes; `C=1`; quiet modes.
//! Not supported: animation, source rectangles, offsets, z-index, relative placements.

use base64::Engine as _;

use super::MAX_SIDE;

/// A command's control data. Keys TJ doesn't use are ignored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Control {
    pub action: u8,
    pub format: u32,
    pub medium: u8,
    pub compressed: bool,
    pub width: u32,
    pub height: u32,
    pub id: Option<u32>,
    pub number: Option<u32>,
    pub placement: Option<u32>,
    pub more: bool,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub cursor_stays: bool,
    pub quiet: u8,
    pub delete: u8,
    /// For files: where to start reading, and how much (0: to the end).
    pub offset: u64,
    pub size: u64,
}

/// Split a command into its control data and payload.
pub fn parse(body: &[u8]) -> (Control, &[u8]) {
    let (keys, payload) = match body.iter().position(|b| *b == b';') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, &[][..]),
    };
    let mut c = Control { action: b't', format: 32, medium: b'd', delete: b'a', ..Default::default() };
    for pair in keys.split(|b| *b == b',') {
        let Some(eq) = pair.iter().position(|b| *b == b'=') else { continue };
        let (key, value) = (&pair[..eq], &pair[eq + 1..]);
        let number = || std::str::from_utf8(value).ok().and_then(|v| v.parse::<i64>().ok());
        let first = value.first().copied().unwrap_or(0);
        match key {
            b"a" => c.action = first,
            b"f" => c.format = number().unwrap_or(32) as u32,
            b"t" => c.medium = first,
            b"o" => c.compressed = first == b'z',
            b"s" => c.width = number().unwrap_or(0).max(0) as u32,
            b"v" => c.height = number().unwrap_or(0).max(0) as u32,
            b"i" => c.id = number().filter(|n| *n > 0).map(|n| n as u32),
            b"I" => c.number = number().filter(|n| *n > 0).map(|n| n as u32),
            b"p" => c.placement = number().filter(|n| *n > 0).map(|n| n as u32),
            b"m" => c.more = first == b'1',
            b"c" => c.cols = number().filter(|n| *n > 0).map(|n| n.min(u16::MAX as i64) as u16),
            b"r" => c.rows = number().filter(|n| *n > 0).map(|n| n.min(u16::MAX as i64) as u16),
            b"C" => c.cursor_stays = first == b'1',
            b"q" => c.quiet = number().unwrap_or(0).clamp(0, 2) as u8,
            b"d" => c.delete = first,
            b"O" => c.offset = number().unwrap_or(0).max(0) as u64,
            b"S" => c.size = number().unwrap_or(0).max(0) as u64,
            _ => {}
        }
    }
    (c, payload)
}

/// A complete command: its control data and its payload (still base64).
pub struct Command {
    pub control: Control,
    pub data: Vec<u8>,
}

/// Chunked uploads: the first chunk's control data, and the payload so far.
#[derive(Default)]
pub struct Uploads {
    pending: Option<Command>,
}

impl Uploads {
    /// Take one APC body; returns a command once the last chunk of it has arrived.
    pub fn take(&mut self, body: &[u8]) -> Option<Command> {
        let (control, payload) = parse(body);
        if let Some(mut pending) = self.pending.take() {
            // Later chunks only carry `m` (and maybe `q`); the first one's keys stand.
            pending.data.extend_from_slice(payload);
            if control.more {
                self.pending = Some(pending);
                return None;
            }
            return Some(pending);
        }
        let command = Command { control, data: payload.to_vec() };
        if command.control.more {
            self.pending = Some(command);
            return None;
        }
        Some(command)
    }
}

/// The reply to a command, unless it shouldn't get one: only commands with an id or a
/// number are answered, `q=1` drops OKs and `q=2` drops everything.
pub fn reply(c: &Control, result: Result<(), &str>) -> Option<String> {
    if c.id.is_none() && c.number.is_none() {
        return None;
    }
    let message = match result {
        Ok(()) if c.quiet >= 1 => return None,
        Ok(()) => "OK",
        Err(_) if c.quiet >= 2 => return None,
        Err(err) => err,
    };
    let mut keys = Vec::new();
    if let Some(id) = c.id {
        keys.push(format!("i={id}"));
    }
    if let Some(n) = c.number {
        keys.push(format!("I={n}"));
    }
    if let Some(p) = c.placement {
        keys.push(format!("p={p}"));
    }
    Some(format!("\x1b_G{};{message}\x1b\\", keys.join(",")))
}

fn base64(data: &[u8]) -> Result<Vec<u8>, String> {
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    let clean: Vec<u8> = data.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    engine.decode(clean).map_err(|_| "EINVAL:bad base64".to_string())
}

/// Read a file the protocol names, as it demands: regular files only, nothing from
/// `/proc`, `/sys` or `/dev` except shared memory.
fn read_file(path: &[u8], c: &Control, delete: bool) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Seek as _};
    let bad = || "EBADF:could not read the file".to_string();
    let path = std::path::PathBuf::from(std::str::from_utf8(path).map_err(|_| bad())?);
    let real = path.canonicalize().map_err(|_| bad())?;
    let forbidden = ["/proc", "/sys", "/dev"].iter().any(|p| real.starts_with(p)) && !real.starts_with("/dev/shm");
    if forbidden || !real.is_file() {
        return Err(bad());
    }
    let mut file = std::fs::File::open(&real).map_err(|_| bad())?;
    if c.offset > 0 {
        file.seek(std::io::SeekFrom::Start(c.offset)).map_err(|_| bad())?;
    }
    let mut data = Vec::new();
    match c.size {
        0 => file.read_to_end(&mut data),
        n => file.take(n).read_to_end(&mut data),
    }
    .map_err(|_| bad())?;
    if delete {
        let _ = std::fs::remove_file(&real);
    }
    Ok(data)
}

/// Decode a command's image to RGBA.
pub fn decode(c: &Control, payload: &[u8]) -> Result<image::RgbaImage, String> {
    let data = base64(payload)?;
    let data = match c.medium {
        b'd' => data,
        b'f' => read_file(&data, c, false)?,
        // Temporary files are deleted once read, if they look like one.
        b't' => {
            let name = String::from_utf8_lossy(&data).to_string();
            let temp = name.contains("tty-graphics-protocol")
                && (name.starts_with("/tmp/")
                    || name.starts_with("/dev/shm/")
                    || name.starts_with(&std::env::temp_dir().display().to_string()));
            read_file(&data, c, temp)?
        }
        b's' => {
            let name = String::from_utf8_lossy(&data).trim_start_matches('/').to_string();
            read_file(format!("/dev/shm/{name}").as_bytes(), c, true)?
        }
        _ => return Err("EINVAL:unknown transmission medium".into()),
    };
    let data = if c.compressed {
        miniz_oxide::inflate::decompress_to_vec_zlib(&data).map_err(|_| "EINVAL:bad zlib data".to_string())?
    } else {
        data
    };
    let (w, h) = (c.width, c.height);
    let raw = |channels: usize| -> Result<image::RgbaImage, String> {
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return Err("EINVAL:bad image size".into());
        }
        let needed = w as usize * h as usize * channels;
        if data.len() < needed {
            return Err("ENODATA:not enough image data".into());
        }
        let rgba: Vec<u8> = if channels == 4 {
            data[..needed].to_vec()
        } else {
            data[..needed].as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect()
        };
        image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| "EINVAL:bad image data".into())
    };
    match c.format {
        32 => raw(4),
        24 => raw(3),
        100 => {
            let image = image::load_from_memory(&data).map_err(|_| "EBADPNG:could not decode the image".to_string())?;
            if image.width() > MAX_SIDE || image.height() > MAX_SIDE {
                return Err("EINVAL:image too large".into());
            }
            Ok(image.to_rgba8())
        }
        _ => Err("EINVAL:unknown format".into()),
    }
}
