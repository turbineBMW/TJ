//! Each agent's mark, as T3 Code draws it, for the end of a thread's row in the sidebar.
//!
//! The marks come from T3 Code (`apps/web/src/components/Icons.tsx` and each
//! `packages/provider-*/src/client.ts`) and live in `data/agents/`. Monochrome marks use
//! `currentColor`, which is swapped for the theme's text colour here, so they follow the
//! Omarchy theme the way T3 Code's swap between black and white. Coloured marks (Claude,
//! Kiro, Muse, Antigravity) keep their own colours.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

use gpui_kit::{Hsla, Image, ImageFormat};

use crate::agent::AgentKind;

/// The mark's file: an SVG template (or, for Antigravity, a PNG).
fn source(kind: AgentKind) -> (ImageFormat, &'static [u8]) {
    let svg = |bytes: &'static [u8]| (ImageFormat::Svg, bytes);
    match kind {
        AgentKind::Claude => svg(include_bytes!("../data/agents/claude.svg")),
        AgentKind::Codex => svg(include_bytes!("../data/agents/codex.svg")),
        AgentKind::OpenCode => svg(include_bytes!("../data/agents/opencode.svg")),
        AgentKind::Cursor => svg(include_bytes!("../data/agents/cursor.svg")),
        AgentKind::Grok => svg(include_bytes!("../data/agents/grok.svg")),
        AgentKind::Pi => svg(include_bytes!("../data/agents/pi.svg")),
        AgentKind::Kiro => svg(include_bytes!("../data/agents/kiro.svg")),
        AgentKind::Muse => svg(include_bytes!("../data/agents/muse.svg")),
        AgentKind::Fx => svg(include_bytes!("../data/agents/fx.svg")),
        AgentKind::Antigravity => (ImageFormat::Png, include_bytes!("../data/agents/antigravity.png")),
    }
}

/// `color` as `#rrggbb`, with alpha ignored.
fn hex(color: Hsla) -> String {
    let rgb = color.to_rgb();
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(rgb.r), byte(rgb.g), byte(rgb.b))
}

/// Marks built so far, by agent and `#rrggbb` colour.
type Cache = Mutex<HashMap<(AgentKind, String), Arc<Image>>>;

/// `kind`'s mark, with monochrome parts drawn in `color`. Built once per agent and colour,
/// so every frame hands gpui the same image (and gpui keeps the rasterized copy).
pub fn icon(kind: AgentKind, color: Hsla) -> Arc<Image> {
    static CACHE: LazyLock<Cache> = LazyLock::new(Default::default);
    let color = hex(color);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry((kind, color.clone()))
        .or_insert_with(|| {
            let (format, bytes) = source(kind);
            let bytes = match format {
                ImageFormat::Svg => String::from_utf8_lossy(bytes).replace("currentColor", &color).into_bytes(),
                _ => bytes.to_vec(),
            };
            Arc::new(Image::from_bytes(format, bytes))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mark parses, fits the 32 px box gpui rasterizes it in, and draws something.
    #[test]
    fn every_mark_draws() {
        let options = resvg::usvg::Options::default();
        for kind in AgentKind::ALL {
            let drawn = match source(kind) {
                (ImageFormat::Svg, bytes) => {
                    let svg = String::from_utf8(bytes.to_vec()).unwrap().replace("currentColor", "#123456");
                    let tree = resvg::usvg::Tree::from_str(&svg, &options).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
                    let size = tree.size();
                    assert!(size.width() <= 32.0 && size.height() <= 32.0, "{kind:?} is {size:?}");
                    let mut pixmap = resvg::tiny_skia::Pixmap::new(32, 32).unwrap();
                    resvg::render(&tree, Default::default(), &mut pixmap.as_mut());
                    pixmap.pixels().iter().filter(|p| p.alpha() > 0).count()
                }
                (_, bytes) => {
                    let image = image::load_from_memory(bytes).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
                    image.to_rgba8().pixels().filter(|p| p[3] > 0).count()
                }
            };
            assert!(drawn > 50, "{kind:?} draws only {drawn} pixels");
        }
    }

    #[test]
    fn monochrome_marks_take_the_colour() {
        let white = gpui_kit::hsla(0.0, 0.0, 1.0, 1.0);
        let bytes = icon(AgentKind::Codex, white);
        let svg = String::from_utf8(bytes.bytes().to_vec()).unwrap();
        assert!(svg.contains("#ffffff") && !svg.contains("currentColor"));
        // Claude's mark keeps its own orange.
        let claude = String::from_utf8(icon(AgentKind::Claude, white).bytes().to_vec()).unwrap();
        assert!(claude.contains("#d97757"));
    }

    #[test]
    fn agent_names_round_trip() {
        for kind in AgentKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<AgentKind>(&json).unwrap(), kind);
        }
        assert_eq!(serde_json::to_string(&AgentKind::OpenCode).unwrap(), "\"opencode\"");
    }
}
