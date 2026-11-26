//! The two fonts TJ draws with: the UI font, and the monospace one for code, tool output
//! and the terminal. Settings can pick either (Appearance tab); otherwise the UI font is
//! the desktop's (see [`desktop_ui_font`]) and the monospace one is gpui-kit's default.
//!
//! gpui panics on the first line it lays out in a family it can't find, so a pick is only
//! used while that family is installed: a font removed since falls back to the default.

use std::{collections::HashMap, rc::Rc};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Global, SharedString, font, px};

use crate::settings::Settings;

/// The fonts used when settings don't pick one, found once at start-up (before any pick is
/// applied, so they stay the defaults).
#[derive(Clone, Debug)]
pub struct DefaultFonts {
    pub ui: SharedString,
    pub mono: SharedString,
}

impl Global for DefaultFonts {}

/// Remember the defaults: the desktop's UI font (else the theme's), and the theme's
/// monospace font.
pub fn init(cx: &mut App) {
    let theme = cx.theme();
    let defaults = DefaultFonts {
        ui: desktop_ui_font().map(SharedString::from).unwrap_or_else(|| theme.font_family.clone()),
        mono: theme.mono_font_family.clone(),
    };
    cx.set_global(defaults);
}

/// The fonts used when settings don't pick one.
pub fn defaults(cx: &mut App) -> DefaultFonts {
    if !cx.has_global::<DefaultFonts>() {
        // Tests, which don't run `init`: the theme's fonts, without asking the desktop.
        let theme = cx.theme();
        let defaults = DefaultFonts { ui: theme.font_family.clone(), mono: theme.mono_font_family.clone() };
        cx.set_global(defaults);
    }
    cx.global::<DefaultFonts>().clone()
}

/// The UI and monospace fonts to draw with now: the settings' picks while they're installed,
/// else the defaults.
pub fn chosen(cx: &mut App) -> (SharedString, SharedString) {
    let defaults = defaults(cx);
    let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
    let installed = families(cx).installed.clone();
    let usable = |pick: Option<String>| pick.filter(|f| installed.iter().any(|name| name == f)).map(SharedString::from);
    (usable(settings.ui_font).unwrap_or(defaults.ui), usable(settings.mono_font).unwrap_or(defaults.mono))
}

/// The families on this machine, listed when first asked for (tests set their own).
#[derive(Clone, Default)]
pub struct Families {
    /// Every family gpui can draw, as it names them.
    pub installed: Rc<[String]>,
    /// The families the UI font can be: those that cover English, so icon and emoji fonts
    /// stay out, sorted by name. Without fontconfig, every family gpui lists.
    pub ui: Rc<[String]>,
    /// The UI ones whose letters are all one width, measured when first asked for: fontconfig
    /// doesn't mark every monospace font (Noto Sans Mono, for one).
    pub mono: Option<Rc<[String]>>,
}

impl Global for Families {}

fn families(cx: &mut App) -> &mut Families {
    if !cx.has_global::<Families>() {
        let installed = cx.text_system().all_font_names();
        let latin = run("fc-list", &["-f", "%{family[0]}\n", ":lang=en"]);
        let ui = choices(&installed, latin.as_deref());
        cx.set_global(Families { installed: installed.into(), ui: ui.into(), mono: None });
    }
    cx.global_mut::<Families>()
}

pub fn ui_choices(cx: &mut App) -> Rc<[String]> {
    families(cx).ui.clone()
}

pub fn mono_choices(cx: &mut App) -> Rc<[String]> {
    if let Some(mono) = families(cx).mono.clone() {
        return mono;
    }
    let ui = ui_choices(cx);
    let text = cx.text_system().clone();
    let mono: Rc<[String]> = ui
        .iter()
        .filter(|family| {
            let id = text.resolve_font(&font(family.as_str()));
            let widths: Vec<_> =
                "iMW.0".chars().filter_map(|ch| text.advance(id, px(16.), ch).ok().map(|s| s.width)).collect();
            widths.len() == 5 && widths.iter().all(|w| (*w - widths[0]).abs() < px(0.01))
        })
        .cloned()
        .collect();
    families(cx).mono = Some(mono.clone());
    mono
}

/// The families gpui can draw, kept to those fontconfig lists (one per line) when it could,
/// without gpui's virtual `.SystemUIFont`-style names, sorted and without repeats.
fn choices(installed: &[String], fontconfig: Option<&str>) -> Vec<String> {
    let listed: Option<Vec<&str>> = fontconfig.map(|text| text.lines().map(str::trim).collect());
    let mut families: Vec<String> = installed
        .iter()
        .filter(|name| !name.starts_with('.') && !name.is_empty())
        .filter(|name| listed.as_ref().is_none_or(|listed| listed.contains(&name.as_str())))
        .cloned()
        .collect();
    families.sort_by_key(|name| name.to_lowercase());
    families.dedup();
    families
}

/// Whether every word of the (lower-case) query is in the family's name.
pub fn matches(family: &str, words: &[&str]) -> bool {
    let name = family.to_lowercase();
    words.iter().all(|w| name.contains(w))
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8(out.stdout).ok()).flatten()
}

/// The characters one font has, as fontconfig lists them: sorted, inclusive ranges.
#[derive(Debug, Default, PartialEq)]
struct Coverage(Vec<(u32, u32)>);

impl Coverage {
    /// Read fontconfig's `%{charset}`: hex code points and ranges, such as `20-7e a0 e000-e00a`.
    fn parse(charset: &str) -> Self {
        let hex = |s: &str| u32::from_str_radix(s, 16).ok();
        let mut ranges: Vec<(u32, u32)> = charset
            .split_whitespace()
            .filter_map(|part| match part.split_once('-') {
                Some((a, b)) => Some((hex(a)?, hex(b)?)),
                None => hex(part).map(|c| (c, c)),
            })
            .collect();
        ranges.sort_unstable();
        Self(ranges)
    }

    fn has(&self, ch: char) -> bool {
        let c = ch as u32;
        let ix = self.0.partition_point(|&(start, _)| start <= c);
        ix > 0 && self.0[ix - 1].1 >= c
    }
}

/// Where a monospace family's missing characters come from, in fontconfig's order, so the
/// terminal picks the same font for them as foot and VTE do, and fontconfig rules the user
/// wrote apply. gpui's own fallback ignores fontconfig: it would draw the Omarchy mark from
/// the stock font even when a rule puts a re-centred copy first.
#[derive(Debug, PartialEq)]
pub struct Fallback {
    /// What the family itself has.
    own: Coverage,
    /// The families fontconfig tries after it, best first, with what each has.
    rest: Vec<(SharedString, Coverage)>,
}

impl Fallback {
    /// The family to draw `ch` in when the font lacks it, if fontconfig knows one gpui can
    /// draw. `None` keeps the font (and gpui's own fallback after it).
    pub fn family_for(&self, ch: char) -> Option<&SharedString> {
        if ch.is_ascii() || self.own.has(ch) {
            return None;
        }
        self.rest.iter().find(|(_, coverage)| coverage.has(ch)).map(|(family, _)| family)
    }

    /// Read `fc-match -s -f '%{family[0]}|%{charset}\n' <family>`: one font per line, best
    /// first. Only families gpui has (`installed`) are kept, each once. fontconfig may put
    /// other fonts ahead of `family` (a rule preferring one for every monospace request), but
    /// gpui draws in `family` itself, so the rest is the list without it. `None` when
    /// fontconfig doesn't have `family`.
    fn parse(family: &str, sorted: &str, installed: &[String]) -> Option<Self> {
        let fonts: Vec<(&str, &str)> = sorted.lines().filter_map(|line| line.split_once('|')).collect();
        let is_family = |name: &str| name.eq_ignore_ascii_case(family);
        let own = fonts.iter().find(|(name, _)| is_family(name))?.1;
        let mut rest: Vec<(SharedString, Coverage)> = Vec::new();
        for &(name, charset) in &fonts {
            let known = is_family(name) || rest.iter().any(|(f, _)| f.as_ref() == name);
            if !known && installed.iter().any(|i| i == name) {
                rest.push((SharedString::from(name.to_string()), Coverage::parse(charset)));
            }
        }
        Some(Self { own: Coverage::parse(own), rest })
    }
}

/// Each monospace family's [`Fallback`], read from fontconfig the first time it's asked for.
#[derive(Default)]
struct Fallbacks(HashMap<SharedString, Option<Rc<Fallback>>>);

impl Global for Fallbacks {}

/// Where `family`'s missing characters come from (see [`Fallback`]); `None` without
/// fontconfig, or when it doesn't know the family.
pub fn fallback(family: &SharedString, cx: &mut App) -> Option<Rc<Fallback>> {
    if let Some(known) = cx.try_global::<Fallbacks>().and_then(|f| f.0.get(family)) {
        return known.clone();
    }
    let installed = families(cx).installed.clone();
    let sorted = run("fc-match", &["-s", "-f", "%{family[0]}|%{charset}\n", family]);
    let loaded = sorted.and_then(|sorted| Fallback::parse(family, &sorted, &installed)).map(Rc::new);
    cx.default_global::<Fallbacks>().0.insert(family.clone(), loaded.clone());
    loaded
}

/// Whether `family` has a bold face of its own. gpui on Linux ignores the weight axis of
/// variable fonts, so a variable-only family (Adwaita Sans, say) draws **bold** as regular.
/// A static bold face lives in its own file; a variable font's "Bold" instance shares the
/// regular file. fontconfig reports both as style Bold, so compare files instead.
pub fn has_static_bold(family: &str) -> bool {
    let matched = |pattern: String| run("fc-match", &["-f", "%{family}|%{file}", &pattern]);
    match (matched(family.to_string()), matched(format!("{family}:bold"))) {
        (Some(regular), Some(bold)) => bold.starts_with(family) && regular != bold,
        _ => false,
    }
}

/// The UI font when settings don't pick one: `TJ_UI_FONT`, else the desktop font from GNOME
/// settings if it has a real bold face, else Noto Sans. gpui-kit's default `.SystemUIFont`
/// has the bold problem too (see [`has_static_bold`]).
fn desktop_ui_font() -> Option<String> {
    if let Ok(family) = std::env::var("TJ_UI_FONT") {
        return Some(family);
    }
    let desktop = run("gsettings", &["get", "org.gnome.desktop.interface", "font-name"]).and_then(|value| {
        let value = value.trim().trim_matches('\'').to_string();
        // "Adwaita Sans 11" -> "Adwaita Sans"
        let family = match value.rsplit_once(' ') {
            Some((family, size)) if size.parse::<f32>().is_ok() => family.to_string(),
            _ => value,
        };
        (!family.is_empty()).then_some(family)
    });
    desktop
        .filter(|family| has_static_bold(family))
        .or_else(|| has_static_bold("Noto Sans").then(|| "Noto Sans".to_string()))
}

#[cfg(test)]
mod tests {
    use super::{Coverage, Fallback, choices, matches};

    #[test]
    fn choices_keep_what_fontconfig_lists_and_gpui_can_draw() {
        let installed: Vec<String> =
            ["Noto Sans", ".SystemUIFont", "JoyPixels", "adwaita Mono", "Noto Sans", "Gone"].map(String::from).into();
        let fontconfig = "Noto Sans\nadwaita Mono\nNoto Sans\nOnly In Fontconfig\n";
        assert_eq!(choices(&installed, Some(fontconfig)), ["adwaita Mono", "Noto Sans"], "sorted, once each");
        assert_eq!(
            choices(&installed, None),
            ["adwaita Mono", "Gone", "JoyPixels", "Noto Sans"],
            "without fontconfig, all but the virtual names"
        );
    }

    #[test]
    fn coverage_reads_fontconfig_charsets() {
        let coverage = Coverage::parse("20-7e a0-ac e000-e00a 2665");
        assert!(coverage.has('A') && coverage.has('\u{a0}') && coverage.has('\u{e00a}') && coverage.has('♥'));
        assert!(!coverage.has('\u{1f}') && !coverage.has('\u{ad}') && !coverage.has('\u{e900}'));
        assert_eq!(Coverage::parse(""), Coverage::default());
    }

    #[test]
    fn fallback_follows_fontconfig_order() {
        let installed: Vec<String> = ["Mono", "omarchy-centered", "Icons", "Emoji"].map(String::from).into();
        let sorted = "Mono|20-7e e000-e00a\nomarchy-centered|e900\nNot In Gpui|e900-e9ff\n\
                      Mono|20-7e e900\nIcons|e900-e9ff f000\nEmoji|1f600-1f64f\n";
        let fallback = Fallback::parse("Mono", sorted, &installed).unwrap();
        let family = |ch: char| fallback.family_for(ch).map(|f| f.to_string());
        assert_eq!(family('\u{e900}').as_deref(), Some("omarchy-centered"), "first in fontconfig's order");
        assert_eq!(family('\u{e901}').as_deref(), Some("Icons"), "skipping families gpui doesn't have");
        assert_eq!(family('😀').as_deref(), Some("Emoji"));
        assert_eq!(family('\u{e005}'), None, "the font has it");
        assert_eq!(family('A'), None);
        assert_eq!(family('\u{10ffff}'), None, "nothing has it: gpui's own fallback");
        assert_eq!(Fallback::parse("Missing", sorted, &installed), None, "fontconfig doesn't have it");

        // A rule can put another font ahead of the one asked for; gpui still draws in it.
        let preferred = "Icons|20-7e e900-e9ff\nMono|20-7e\n";
        let fallback = Fallback::parse("Mono", preferred, &installed).unwrap();
        assert_eq!(fallback.family_for('\u{e900}').map(|f| f.to_string()).as_deref(), Some("Icons"));
        assert_eq!(fallback.family_for('z'), None);
    }

    #[test]
    fn a_family_matches_every_word() {
        assert!(matches("JetBrainsMono Nerd Font", &["jet", "nerd"]));
        assert!(!matches("JetBrainsMono Nerd Font", &["jet", "noto"]));
        assert!(matches("Anything", &[]));
    }
}
