//! Follow the active Omarchy theme: colours for the whole window, and a palette for the
//! terminal. The palette resolution (`Palette::resolve` and its helpers) is Bubo's, ported
//! from Rustle, so TJ reads the same colours as the rest of the desktop, including legacy
//! themes that only define `color0`..`color15`.
//!
//! Omarchy keeps the active theme at `~/.local/state/omarchy/current/theme` and swaps that
//! directory on every `omarchy theme set`; [`stamp`] changes when it does.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use gpui_kit::component::ThemeConfig;
use serde_json::{Map, Value, json};

pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/omarchy/current")
}

pub fn theme_dir(home: &Path) -> PathBuf {
    state_dir(home).join("theme")
}

/// Whether this machine has an Omarchy theme to follow.
pub fn detected(home: &Path) -> bool {
    theme_dir(home).is_dir()
}

/// The active theme's name, for the settings popover.
pub fn theme_name(home: &Path) -> Option<String> {
    let name = fs::read_to_string(state_dir(home).join("theme.name")).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Something that changes whenever the theme does: its name and its files' change times.
pub fn stamp(home: &Path) -> Option<String> {
    let dir = theme_dir(home);
    let name = fs::read_to_string(state_dir(home).join("theme.name")).unwrap_or_default();
    let mtime = |file: &str| {
        fs::metadata(dir.join(file))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    };
    detected(home).then(|| format!("{}|{}|{}", name.trim(), mtime("colors.toml"), mtime("alacritty.toml")))
}

/// The terminal's colours: background, foreground, cursor and the 16 ANSI colours, as `#rrggbb`.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalColors {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub ansi: [String; 16],
}

impl gpui_kit::Global for TerminalColors {}

pub struct Loaded {
    pub light: bool,
    /// For `Theme::apply_config`.
    pub config: ThemeConfig,
    pub terminal: TerminalColors,
}

/// Read the active theme, or `None` when Omarchy isn't installed or the theme has no palette.
pub fn load(home: &Path, font_family: Option<&str>) -> Option<Loaded> {
    let dir = theme_dir(home);
    let colors = fs::read_to_string(dir.join("colors.toml")).ok()?;
    let palette = Palette::resolve(&colors, dir.join("light.mode").exists());
    let name = theme_name(home);
    // Terminals follow the theme's terminal config, which may differ from the window palette
    // (a light theme can pin its terminals to a dark palette).
    let terminal = fs::read_to_string(dir.join("alacritty.toml"))
        .ok()
        .and_then(|text| terminal_from_alacritty(&text))
        .unwrap_or_else(|| palette.terminal());
    let config = palette.theme_config(name.as_deref().unwrap_or("Omarchy"), font_family);
    Some(Loaded { light: palette.light, config, terminal })
}

/// `[colors.primary]`, `[colors.cursor]`, `[colors.normal]` and `[colors.bright]` from an
/// Alacritty config.
pub fn terminal_from_alacritty(text: &str) -> Option<TerminalColors> {
    let value: toml::Value = toml::from_str(text).ok()?;
    let colors = value.get("colors")?;
    let get = |table: &str, key: &str| {
        colors.get(table)?.get(key)?.as_str().map(|s| s.trim().replace("0x", "#").to_lowercase())
    };
    let names = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    let mut ansi: [String; 16] = Default::default();
    for (ix, name) in names.iter().enumerate() {
        ansi[ix] = get("normal", name)?;
        ansi[ix + 8] = get("bright", name).unwrap_or_else(|| ansi[ix].clone());
    }
    let background = get("primary", "background")?;
    let foreground = get("primary", "foreground")?;
    let cursor = get("cursor", "cursor").filter(|c| rgb(c).is_some()).unwrap_or_else(|| foreground.clone());
    Some(TerminalColors { background, foreground, cursor, ansi })
}

struct Palette {
    colors: HashMap<String, String>,
    light: bool,
}

impl Palette {
    /// Port of `omarchy-theme-color`'s parse-and-resolve. Keep the two in
    /// step: a theme that renders correctly in Alacritty must render the same
    /// here, and the fallbacks are the only thing standing between a
    /// pre-`colors.toml` theme and a half-empty palette.
    fn resolve(toml: &str, light_mode_file: bool) -> Self {
        let mut colors = parse(toml);
        let mut palette = Palette { light: false, colors: HashMap::new() };

        // The short palette names an older theme may use instead.
        for (canonical, legacy) in [
            ("background", "bg"),
            ("dark_background", "dark_bg"),
            ("darker_background", "darker_bg"),
            ("lighter_background", "lighter_bg"),
            ("foreground", "fg"),
            ("dark_foreground", "dark_fg"),
            ("light_foreground", "light_fg"),
            ("bright_foreground", "bright_fg"),
        ] {
            alias(&mut colors, canonical, legacy);
        }

        // Themes predating the semantic palette define only ANSI names.
        alias(&mut colors, "background", "color0");
        alias(&mut colors, "foreground", "color7");
        mirror(&mut colors, "color0", "background");
        mirror(&mut colors, "color7", "foreground");

        for (name, ansi) in [
            ("red", "color1"),
            ("green", "color2"),
            ("yellow", "color3"),
            ("blue", "color4"),
            ("magenta", "color5"),
            ("cyan", "color6"),
            ("bright_red", "color9"),
            ("bright_green", "color10"),
            ("bright_yellow", "color11"),
            ("bright_blue", "color12"),
            ("bright_magenta", "color13"),
            ("bright_cyan", "color14"),
        ] {
            alias(&mut colors, name, ansi);
        }
        alias(&mut colors, "magenta", "purple");
        alias(&mut colors, "bright_magenta", "bright_purple");

        alias_any(&mut colors, "light_foreground", &["color7", "foreground"]);
        alias_any(&mut colors, "bright_foreground", &["color15", "foreground"]);
        // Unconditional, exactly as omarchy resolves it: the cursor is the
        // brightest foreground, not a key a theme sets on its own.
        mirror(&mut colors, "cursor", "bright_foreground");
        alias_any(&mut colors, "lighter_background", &["color0", "background"]);
        alias_any(&mut colors, "dark_foreground", &["color8", "foreground"]);
        alias_any(&mut colors, "muted", &["color8", "dark_foreground"]);
        alias_any(&mut colors, "selection", &["selection_background", "color8", "color0", "background"]);
        alias(&mut colors, "selection_background", "selection");
        alias(&mut colors, "selection_foreground", "bright_foreground");
        alias(&mut colors, "orange", "yellow");
        derive(&mut colors, "brown", "orange", "#000000", 0.5);

        derive(&mut colors, "dark_background", "background", "#000000", 0.25);
        derive(&mut colors, "darker_background", "background", "#000000", 0.5);
        for (bright, base) in [
            ("bright_red", "red"),
            ("bright_yellow", "yellow"),
            ("bright_green", "green"),
            ("bright_cyan", "cyan"),
            ("bright_blue", "blue"),
            ("bright_magenta", "magenta"),
        ] {
            derive(&mut colors, bright, base, "#ffffff", 0.2);
        }

        for (ansi, name) in [
            ("color0", "background"),
            ("color1", "red"),
            ("color2", "green"),
            ("color3", "yellow"),
            ("color4", "blue"),
            ("color5", "magenta"),
            ("color6", "cyan"),
            ("color7", "foreground"),
            ("color8", "muted"),
            ("color9", "bright_red"),
            ("color10", "bright_green"),
            ("color11", "bright_yellow"),
            ("color12", "bright_blue"),
            ("color13", "bright_magenta"),
            ("color14", "bright_cyan"),
            ("color15", "bright_foreground"),
        ] {
            alias(&mut colors, ansi, name);
        }

        palette.light = resolve_mode(&colors, light_mode_file);
        palette.colors = colors;
        palette
    }

    /// A palette key, or the value of `fallback` when the theme is missing it
    /// even after the cascade above.
    fn get(&self, key: &str, fallback: &str) -> String {
        self.colors
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| self.colors.get(fallback).cloned().unwrap_or_else(|| fallback.to_string()))
    }
}

impl Palette {
    fn terminal(&self) -> TerminalColors {
        let c = |key: &str, fallback: &str| self.get(key, fallback);
        let mut ansi: [String; 16] = Default::default();
        for (ix, slot) in ansi.iter_mut().enumerate() {
            *slot = c(&format!("color{ix}"), "foreground");
        }
        TerminalColors {
            background: c("background", "#1d1d20"),
            foreground: c("foreground", "#d0cfcc"),
            cursor: c("cursor", "foreground"),
            ansi,
        }
    }

    /// The palette as a gpui-kit theme. The content is the theme's own background, as in its
    /// terminal; around it the surfaces are Chrome's for the theme, as Omarchy themes Chromium
    /// ([`chrome_colors`]): the frame (sidebar, top bar, tabs not on show), the tab on show,
    /// and the address bar's colour for fields and cards. The rest follows Bubo's libadwaita
    /// mapping: hairlines at the lowest opacity that still reads, and a light theme's
    /// foreground turned into ink.
    fn theme_config(&self, name: &str, font_family: Option<&str>) -> ThemeConfig {
        let c = |key: &str, fallback: &str| self.get(key, fallback);
        let seed = c("background", "#1d1d20");
        let window = chrome_colors(&seed, self.light);
        let bg = seed.clone();
        let shown_tab = window.as_ref().map_or_else(|| seed.clone(), |w| w.base.clone());
        let fg = c("foreground", "#d0cfcc");
        let fg = if self.light { ink(&fg, &bg) } else { fg };
        let accent = c("accent", "blue");
        let accent = if rgb(&accent).is_some() { accent } else { c("blue", "#3584e4") };
        let chrome = window.as_ref().map_or_else(|| c("dark_background", "background"), |w| w.header.clone());
        let raised = c("lighter_background", "background");
        let selection = c("selection", "background");
        let muted = c("muted", "dark_foreground");
        let opacity = border_opacity(&fg, &[&bg, &chrome]);
        let tint = |base: &str, amount: f64| mix(base, &fg, amount).unwrap_or_else(|| base.to_string());
        let border = tint(&bg, opacity as f64 / 100.0);
        let field = window.as_ref().map_or_else(|| tint(&bg, 0.06), |w| w.omnibox.clone());
        let tab_fg = window.as_ref().map_or_else(|| muted.clone(), |w| w.on_surface_secondary.clone());
        let alpha = |color: &str, a: u8| format!("{color}{a:02x}");

        let mut colors = Map::new();
        let mut set = |key: &str, value: String| {
            colors.insert(key.to_string(), Value::String(value));
        };
        set("background", bg.clone());
        set("foreground", fg.clone());
        set("border", border.clone());
        set("input.border", border.clone());
        set("ring", accent.clone());
        set("caret", fg.clone());
        set("selection.background", alpha(&selection, 0xcc));
        set("muted.background", chrome.clone());
        set("muted.foreground", muted.clone());
        set("accent.background", tint(&bg, 0.08));
        set("accent.foreground", fg.clone());
        set("primary.background", accent.clone());
        set("primary.foreground", readable_on(&accent).into());
        set("primary.hover.background", mix(&accent, &bg, 0.12).unwrap_or(accent.clone()));
        set("primary.active.background", mix(&accent, "#000000", 0.12).unwrap_or(accent.clone()));
        set("secondary.background", field.clone());
        set("secondary.hover.background", tint(&field, 0.05));
        set("secondary.active.background", tint(&field, 0.09));
        set("secondary.foreground", fg.clone());
        set("sidebar.background", chrome.clone());
        set("sidebar.foreground", fg.clone());
        set("sidebar.border", border.clone());
        set("sidebar.accent.background", tint(&chrome, 0.10));
        set("sidebar.accent.foreground", fg.clone());
        set("sidebar.primary.background", accent.clone());
        set("sidebar.primary.foreground", readable_on(&accent).into());
        set("title_bar.background", chrome.clone());
        set("tab_bar.background", chrome.clone());
        set("tab.background", chrome.clone());
        set("tab.foreground", tab_fg);
        set("tab.active.background", shown_tab);
        set("tab.active.foreground", fg.clone());
        set("title_bar.border", border.clone());
        set("popover.background", if self.light { raised } else { chrome.clone() });
        set("popover.foreground", fg.clone());
        set("list.background", bg.clone());
        set("list.even.background", bg.clone());
        set("list.head.background", chrome.clone());
        set("list.hover.background", tint(&bg, 0.06));
        set("list.active.background", mix(&bg, &accent, 0.18).unwrap_or(selection.clone()));
        set("list.active.border", accent.clone());
        set("link", accent.clone());
        set("link.hover", mix(&accent, &fg, 0.2).unwrap_or(accent.clone()));
        set("link.active", accent.clone());
        set("scrollbar.background", "#00000000".into());
        set("scrollbar.thumb.background", alpha(&fg, 0x40));
        set("scrollbar.thumb.hover.background", alpha(&fg, 0x66));
        set("overlay", "#00000066".into());
        for (prefix, base) in [("success", "green"), ("warning", "yellow"), ("danger", "red")] {
            let base = c(base, base);
            if rgb(&base).is_none() {
                continue;
            }
            set(&format!("{prefix}.background"), base.clone());
            set(&format!("{prefix}.foreground"), readable_on(&base).into());
            set(&format!("{prefix}.hover.background"), mix(&base, &bg, 0.12).unwrap_or(base.clone()));
            set(&format!("{prefix}.active.background"), mix(&base, "#000000", 0.12).unwrap_or(base.clone()));
        }

        let mut config = json!({
            "name": name,
            "mode": if self.light { "light" } else { "dark" },
            "colors": Value::Object(colors),
        });
        if let Some(font) = font_family {
            config["font.family"] = json!(font);
        }
        serde_json::from_value(config).expect("the theme config is built from known keys")
    }
}

/// Chrome's window colours for a theme colour. Omarchy gives Chromium the theme's
/// `background` as its theme colour; Chrome takes its hue and builds Material "tonal spot"
/// palettes from it (primary chroma 40, secondary 16, neutral 6), then picks tones from them
/// for a themed window (`ui/color/sys_color_mixer.cc`, `AddThemedSysColorOverrides`).
#[derive(Debug, PartialEq)]
struct ChromeColors {
    /// The frame behind the tabs, and the tabs not on show (`kColorSysHeader`).
    header: String,
    /// The tab on show, the toolbar and the page (`kColorSysBase`).
    base: String,
    /// The address bar (`kColorSysOmniboxContainer`).
    omnibox: String,
    /// Text on the tab on show (`kColorSysOnSurface`) and on the others
    /// (`kColorSysOnSurfaceSecondary`).
    on_surface: String,
    on_surface_secondary: String,
}

fn chrome_colors(seed: &str, light: bool) -> Option<ChromeColors> {
    use material_colors::{color::Rgb, hct::Hct};
    let (r, g, b) = rgb(seed)?;
    let hue = Hct::new(Rgb::new(r, g, b)).get_hue();
    let tone = |chroma: f64, tone: f64| {
        let c: Rgb = Hct::from(hue, chroma, tone).into();
        format!("#{:02x}{:02x}{:02x}", c.red, c.green, c.blue)
    };
    let (primary, secondary, neutral) = (40.0, 16.0, 6.0);
    Some(if light {
        // Surface4: the primary colour (tone 40) at 0x1E over the surface (neutral 99).
        let omnibox = mix(&tone(neutral, 99.0), &tone(primary, 40.0), f64::from(0x1E) / 255.0)?;
        ChromeColors {
            header: tone(secondary, 90.0),
            base: tone(neutral, 98.0),
            omnibox,
            on_surface: tone(neutral, 10.0),
            on_surface_secondary: tone(secondary, 30.0),
        }
    } else {
        ChromeColors {
            header: tone(secondary, 12.0),
            base: tone(secondary, 25.0),
            omnibox: tone(secondary, 15.0),
            on_surface: tone(neutral, 90.0),
            on_surface_secondary: tone(secondary, 80.0),
        }
    })
}

/// `key = "value"` pairs, quotes and inline comments stripped. Keys and
/// values outside the charset omarchy accepts are dropped rather than
/// smuggled into CSS.
fn parse(toml: &str) -> HashMap<String, String> {
    let mut colors = HashMap::new();
    for line in toml.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key: String = key.chars().filter(|ch| !matches!(ch, '"' | '\'' | ' ' | '\t')).collect();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        let value = match value.split_once(['"', '\'']) {
            Some((_, rest)) => rest.split(['"', '\'']).next().unwrap_or_default().to_string(),
            None => value.trim().to_string(),
        };
        if !key.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')) {
            continue;
        }
        if !value.chars().all(|ch| ch.is_ascii_alphanumeric() || "#(),._+/% -".contains(ch)) {
            continue;
        }
        colors.insert(key, value);
    }
    colors
}

/// Fill `key` from `from` only when the theme left it unset.
fn alias(colors: &mut HashMap<String, String>, key: &str, from: &str) {
    alias_any(colors, key, &[from]);
}

fn alias_any(colors: &mut HashMap<String, String>, key: &str, from: &[&str]) {
    if colors.get(key).is_some_and(|value| !value.is_empty()) {
        return;
    }
    for candidate in from {
        if let Some(value) = colors.get(*candidate).filter(|value| !value.is_empty()) {
            let value = value.clone();
            colors.insert(key.to_string(), value);
            return;
        }
    }
}

/// Overwrite `key` with `from` whenever `from` is set.
fn mirror(colors: &mut HashMap<String, String>, key: &str, from: &str) {
    if let Some(value) = colors.get(from).filter(|value| !value.is_empty()) {
        let value = value.clone();
        colors.insert(key.to_string(), value);
    }
}

/// Fill `key` by blending `base` toward `toward`, when the theme left it unset.
fn derive(colors: &mut HashMap<String, String>, key: &str, base: &str, toward: &str, amount: f64) {
    if colors.get(key).is_some_and(|value| !value.is_empty()) {
        return;
    }
    let Some(base) = colors.get(base).cloned() else {
        return;
    };
    if let Some(value) = mix(&base, toward, amount) {
        colors.insert(key.to_string(), value);
    }
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

/// Blend two hex colors, `amount` of the way from `start` to `end`. A
/// non-hex value (an `rgba()` a theme wrote by hand) has nothing to blend.
fn mix(start: &str, end: &str, amount: f64) -> Option<String> {
    let (sr, sg, sb) = rgb(start)?;
    let (er, eg, eb) = rgb(end)?;
    let amount = amount.clamp(0.0, 1.0);
    let blend = |s: u8, e: u8| (s as f64 * (1.0 - amount) + e as f64 * amount + 0.5) as u8;
    Some(format!("#{:02x}{:02x}{:02x}", blend(sr, er), blend(sg, eg), blend(sb, eb)))
}

/// WCAG relative luminance.
fn luminance(color: &str) -> Option<f64> {
    let (r, g, b) = rgb(color)?;
    let linear = |channel: u8| {
        let c = channel as f64 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
}

/// The WCAG contrast ratio between two colours, or `None` when either is
/// not a colour this can measure.
fn contrast(a: &str, b: &str) -> Option<f64> {
    let (a, b) = (luminance(a)?, luminance(b)?);
    Some((a.max(b) + 0.05) / (a.min(b) + 0.05))
}

/// `#rrggbb` as hue, saturation and lightness, each `0.0..=1.0`.
fn hsl(color: &str) -> Option<(f64, f64, f64)> {
    let (r, g, b) = rgb(color)?;
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let chroma = max - min;
    if chroma == 0.0 {
        return Some((0.0, 0.0, lightness));
    }
    let hue = if max == r {
        ((g - b) / chroma).rem_euclid(6.0)
    } else if max == g {
        (b - r) / chroma + 2.0
    } else {
        (r - g) / chroma + 4.0
    };
    Some((hue / 6.0, chroma / (1.0 - (2.0 * lightness - 1.0).abs()), lightness))
}

fn from_hsl(hue: f64, saturation: f64, lightness: f64) -> String {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue = hue * 6.0;
    let second = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    let base = lightness - chroma / 2.0;
    let channel = |value: f64| ((value + base) * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", channel(r), channel(g), channel(b))
}

/// The chroma an ink may carry before dimming it tints the whole window:
/// libadwaita composites `.dim-label` at 55%, and 55% of a saturated blue
/// over a pale surface is lavender. Omarchy's own light themes all sit well
/// under this -- `catppuccin-latte` is the most coloured at 16%.
const INK_MAX_SATURATION: f64 = 0.30;
/// The contrast body text is held to. WCAG asks 4.5 for text alone, but the
/// dimmed half of it is drawn from the same ink, and only has room to stay
/// legible when what it is dimmed from has this much. Omarchy's own light
/// themes all clear it or come within a tenth.
const INK_CONTRAST: f64 = 7.0;
const INK_STEP: f64 = 0.005;

/// A light theme's `foreground` is the ink its terminal and editor write
/// with, where a saturated mid-tone is the theme's signature over a mostly
/// empty screen -- Tokyo Night Day's `#3760bf` is the blue it is on purpose.
/// A window is not a terminal: that one colour becomes every label at once,
/// and libadwaita dims a good share of them to 55%, which lands a mid-tone
/// blue on lavender. So the ink keeps the hue the theme chose, but is pulled
/// under [`INK_MAX_SATURATION`] and deepened until it reaches
/// [`INK_CONTRAST`]. A foreground that already reads as ink is returned
/// exactly as the theme wrote it, which is every light theme omarchy ships.
///
/// Dark themes are left alone: their foregrounds are pale by construction,
/// and a colour cast that would shout on white is what a dark palette is
/// for.
fn ink(fg: &str, bg: &str) -> String {
    let Some((hue, saturation, lightness)) = hsl(fg) else {
        return fg.to_string();
    };
    if saturation <= INK_MAX_SATURATION && contrast(fg, bg).is_none_or(|c| c >= INK_CONTRAST) {
        return fg.to_string();
    }
    let saturation = saturation.min(INK_MAX_SATURATION);
    let mut lightness = lightness;
    loop {
        let candidate = from_hsl(hue, saturation, lightness);
        if lightness <= 0.0 || contrast(&candidate, bg).is_none_or(|c| c >= INK_CONTRAST) {
            return candidate;
        }
        lightness = (lightness - INK_STEP).max(0.0);
    }
}

/// libadwaita's stock hairline opacity, and the ceiling past which a line
/// stops being a hairline.
const BORDER_OPACITY: u32 = 15;
const BORDER_OPACITY_MAX: u32 = 40;
/// The contrast a hairline needs against its surface to read as a line;
/// stock Adwaita sits between 1.3 (light) and 1.6 (dark).
const BORDER_CONTRAST: f64 = 1.4;

/// The lowest `--border-opacity`, in percent, at which `fg` painted over
/// each of `surfaces` reaches [`BORDER_CONTRAST`]. Colours that aren't hex
/// can't be measured and keep the stock opacity.
fn border_opacity(fg: &str, surfaces: &[&str]) -> u32 {
    let line_contrast = |surface: &str, opacity: u32| contrast(&mix(surface, fg, opacity as f64 / 100.0)?, surface);
    (BORDER_OPACITY..=BORDER_OPACITY_MAX)
        .find(|&opacity| {
            surfaces.iter().all(|surface| line_contrast(surface, opacity).is_none_or(|c| c >= BORDER_CONTRAST))
        })
        .unwrap_or(BORDER_OPACITY_MAX)
}

/// Black or white, whichever stays legible on `color`. Used for text on the
/// saturated accent and status fills, where no palette key is guaranteed to
/// contrast with them in both light and dark themes.
fn readable_on(color: &str) -> &'static str {
    match rgb(color) {
        // Rec. 601 luma, the same threshold GTK uses to pick icon shades.
        Some((r, g, b)) if 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64 > 140.0 => "#000000",
        _ => "#ffffff",
    }
}

/// `mode`, then the legacy `theme_type`, then a `light.mode` file beside
/// colors.toml, then the background's brightness.
fn resolve_mode(colors: &HashMap<String, String>, light_mode_file: bool) -> bool {
    for key in ["mode", "theme_type"] {
        if let Some(mode) = colors.get(key).filter(|mode| !mode.is_empty()) {
            return mode.eq_ignore_ascii_case("light");
        }
    }
    if light_mode_file {
        return true;
    }
    match colors.get("background").and_then(|bg| rgb(bg)) {
        Some((r, g, b)) => r as u32 + g as u32 + b as u32 > 382,
        None => false,
    }
}

/// Apply the active Omarchy theme to the app, or TJ's dark default when it isn't followed
/// (or there's no theme), with the fonts settings pick (see `fonts::chosen`).
pub fn apply(cx: &mut gpui_kit::App, follow: bool) {
    use gpui_kit::component::{Theme, ThemeMode};
    let (ui_font, mono_font) = crate::fonts::chosen(cx);
    let loaded = follow.then(|| load(&crate::store::home_dir(), Some(&ui_font))).flatten();
    match loaded {
        Some(loaded) => {
            Theme::change(if loaded.light { ThemeMode::Light } else { ThemeMode::Dark }, None, cx);
            Theme::global_mut(cx).apply_config(&std::rc::Rc::new(loaded.config));
            cx.set_global(loaded.terminal);
        }
        None => {
            Theme::change(ThemeMode::Dark, None, cx);
            if cx.has_global::<TerminalColors>() {
                cx.remove_global::<TerminalColors>();
            }
        }
    }
    // Through `update`, so the Base layer's copy of the theme gets the fonts too.
    Theme::update(cx, |theme| {
        theme.font_family = ui_font;
        theme.mono_font_family = mono_font;
    });
    // TJ draws the text fields' caret itself, so it can glide (see `caret`).
    crate::caret::hide_native_caret(cx);
    cx.refresh_windows();
}

/// Re-apply the theme whenever Omarchy switches it (checked every two seconds).
pub fn watch(cx: &mut gpui_kit::App) {
    let home = crate::store::home_dir();
    let mut last = stamp(&home);
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
            let now = stamp(&home);
            if now == last {
                continue;
            }
            last = now;
            cx.update(|cx| {
                let follow = cx.try_global::<crate::settings::Settings>().is_none_or(|s| s.follow_omarchy_theme);
                if follow {
                    apply(cx, true);
                }
            });
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::{Palette, chrome_colors, contrast, hsl, luminance, terminal_from_alacritty};

    const OSAKA_JADE: &str = r##"
mode = "dark"
accent = "#509475"
selection = "#32473B"
background = "#111c18"
foreground = "#C1C497"
red = "#FF5345"
green = "#549e6a"
blue = "#509475"
"##;

    #[test]
    fn a_dark_theme_maps_onto_the_window_theme() {
        let palette = Palette::resolve(OSAKA_JADE, false);
        let config = serde_json::to_value(palette.theme_config("Osaka Jade", Some("Noto Sans"))).unwrap();
        let colors = &config["colors"];
        assert_eq!(config["mode"], "dark");
        assert_eq!(config["font.family"], "Noto Sans");
        assert_eq!(colors["primary.background"], "#509475");
        // The content is the theme's background; the surfaces around it are Chrome's for it.
        let chrome = chrome_colors("#111c18", false).unwrap();
        assert_eq!(colors["background"], "#111c18");
        assert_eq!(colors["tab.active.background"], chrome.base.as_str());
        assert_eq!(colors["sidebar.background"], chrome.header.as_str());
        assert_eq!(colors["tab.background"], chrome.header.as_str());
        assert_eq!(colors["secondary.background"], chrome.omnibox.as_str());
    }

    #[test]
    fn chrome_colors_follow_chromes_tones_for_the_theme_colour() {
        // Dark: the frame darkest, the address bar a step up, the page and the tab on show
        // lightest; all in the theme colour's hue.
        let dark = chrome_colors("#111c18", false).unwrap();
        let l = |c: &str| luminance(c).unwrap();
        assert!(l(&dark.header) < l(&dark.omnibox) && l(&dark.omnibox) < l(&dark.base), "{dark:?}");
        let (h, ..) = hsl(&dark.base).unwrap();
        let (seed_h, ..) = hsl("#111c18").unwrap();
        assert!((h - seed_h).abs() * 360.0 < 20.0, "{dark:?}");
        // Light: the page near white, the frame a tinted step darker.
        let light = chrome_colors("#f7efe6", true).unwrap();
        assert!(l(&light.base) > 0.9 && l(&light.header) < l(&light.base), "{light:?}");
        assert!(contrast(&light.on_surface, &light.base).unwrap() > 12.0, "{light:?}");
        assert!(chrome_colors("not a colour", false).is_none());
    }

    #[test]
    fn a_light_theme_is_light_and_keeps_legible_text() {
        let palette = Palette::resolve("mode = \"light\"\nbackground = \"#f7efe6\"\nforeground = \"#3760bf\"\n", false);
        assert!(palette.light);
        let config = serde_json::to_value(palette.theme_config("Day", None)).unwrap();
        // A saturated mid-tone foreground becomes ink: same hue, deeper and calmer.
        assert_ne!(config["colors"]["foreground"], "#3760bf");
    }

    #[test]
    fn legacy_ansi_only_themes_give_a_terminal_palette() {
        let palette = Palette::resolve("color0 = \"#101010\"\ncolor1 = \"#ff0000\"\ncolor7 = \"#dddddd\"\n", false);
        let terminal = palette.terminal();
        assert_eq!(terminal.background, "#101010");
        assert_eq!(terminal.ansi[1], "#ff0000");
        assert_eq!(terminal.foreground, "#dddddd");
    }

    #[test]
    fn alacritty_colors_become_the_terminal_palette() {
        let text = r##"
[colors.primary]
background = "#1a1b26"
foreground = "#c0caf5"
[colors.cursor]
cursor = "#c0caf5"
[colors.normal]
black = "#15161e"
red = "#f7768e"
green = "#9ece6a"
yellow = "#e0af68"
blue = "#7aa2f7"
magenta = "#bb9af7"
cyan = "#7dcfff"
white = "#a9b1d6"
[colors.bright]
black = "0x414868"
red = "#f7768e"
green = "#9ece6a"
yellow = "#e0af68"
blue = "#7aa2f7"
magenta = "#bb9af7"
cyan = "#7dcfff"
white = "#c0caf5"
"##;
        let terminal = terminal_from_alacritty(text).unwrap();
        assert_eq!(terminal.background, "#1a1b26");
        assert_eq!(terminal.ansi[4], "#7aa2f7");
        assert_eq!(terminal.ansi[8], "#414868", "0x colours are read too");
    }

    /// The user's own active theme, if there is one, loads without panicking.
    #[test]
    fn the_active_theme_loads_if_present() {
        let home = crate::store::home_dir();
        if super::detected(&home) {
            assert!(super::load(&home, None).is_some());
        }
    }
}
