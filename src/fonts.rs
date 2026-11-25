//! The two fonts TJ draws with: the UI font, and the monospace one for code, tool output
//! and the terminal. Settings can pick either (Appearance tab); otherwise the UI font is
//! the desktop's (see [`desktop_ui_font`]) and the monospace one is gpui-kit's default.
//!
//! gpui panics on the first line it lays out in a family it can't find, so a pick is only
//! used while that family is installed: a font removed since falls back to the default.

use std::rc::Rc;

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
    use super::{choices, matches};

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
    fn a_family_matches_every_word() {
        assert!(matches("JetBrainsMono Nerd Font", &["jet", "nerd"]));
        assert!(!matches("JetBrainsMono Nerd Font", &["jet", "noto"]));
        assert!(matches("Anything", &[]));
    }
}
