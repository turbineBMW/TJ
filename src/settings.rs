//! TJ's own settings, in `settings.toml` next to `accounts.toml`.
//!
//! As in the rest of the app family, how see-through the sidebar is belongs to the app,
//! not to the theme: a switch (off by default) and an opacity (0.8 by default).

use std::collections::BTreeMap;
use std::fs;

use serde::{Deserialize, Serialize};

use crate::store::Paths;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Take colours from the active Omarchy theme, live.
    pub follow_omarchy_theme: bool,
    pub transparent_sidebar: bool,
    /// 0.3 to 1.0; used while `transparent_sidebar` is on.
    pub sidebar_opacity: f32,
    /// How long the cursor takes to glide to a new place after a key, and each fade of its
    /// blink, in milliseconds (0: it jumps and blinks on and off). An old `cursor_trail` is
    /// ignored.
    pub cursor_glide_ms: u32,
    /// The UI font's family; `None` for the default (see `fonts`).
    pub ui_font: Option<String>,
    /// The monospace family, for code, tool output and the terminal; `None` for the default.
    pub mono_font: Option<String>,
    /// Shortcuts changed from their defaults: a command's id (see `keybindings::COMMANDS`) and
    /// its keys; an empty list leaves it without one. Last, so TOML writes it as a table.
    pub keybindings: BTreeMap<String, Vec<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            follow_omarchy_theme: true,
            transparent_sidebar: false,
            sidebar_opacity: 0.8,
            cursor_glide_ms: 100,
            ui_font: None,
            mono_font: None,
            keybindings: BTreeMap::new(),
        }
    }
}

impl gpui_kit::Global for Settings {}

impl Settings {
    pub const MIN_OPACITY: f32 = 0.3;
    /// The slowest cursor animation the settings offer.
    pub const MAX_GLIDE_MS: u32 = 300;

    pub fn load(paths: &Paths) -> Self {
        fs::read_to_string(paths.config_dir.join("settings.toml"))
            .ok()
            .and_then(|text| toml::from_str::<Self>(&text).ok())
            .map(|mut s| {
                s.cursor_glide_ms = s.cursor_glide_ms.min(Self::MAX_GLIDE_MS);
                s
            })
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        fs::create_dir_all(&paths.config_dir)?;
        fs::write(paths.config_dir.join("settings.toml"), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// How opaque the sidebar is drawn.
    pub fn sidebar_alpha(&self) -> f32 {
        if self.transparent_sidebar { self.sidebar_opacity.clamp(Self::MIN_OPACITY, 1.0) } else { 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;
    use crate::store::{Paths, new_id};

    #[test]
    fn defaults_save_and_load() {
        let paths = Paths::in_dir(&std::env::temp_dir().join(format!("tj-settings-{}", new_id())));
        assert_eq!(Settings::load(&paths), Settings::default());
        let mut settings = Settings {
            transparent_sidebar: true,
            sidebar_opacity: 0.6,
            cursor_glide_ms: 150,
            ui_font: Some("Noto Sans".into()),
            mono_font: Some("JetBrainsMono Nerd Font".into()),
            ..Default::default()
        };
        settings.keybindings.insert("find".into(), vec!["ctrl-alt-f".into()]);
        settings.keybindings.insert("grow_pane".into(), vec![]);
        settings.save(&paths).unwrap();
        assert_eq!(Settings::load(&paths), settings, "changed keys and fonts round-trip, an unbound key too");
        let old = "cursor_glide_ms = 90\ncursor_trail = true\n";
        std::fs::write(paths.config_dir.join("settings.toml"), old).unwrap();
        assert_eq!(Settings::load(&paths).cursor_glide_ms, 90, "a file from before the trail went still loads");
        assert_eq!(settings.sidebar_alpha(), 0.6);
        assert_eq!(Settings::default().sidebar_alpha(), 1.0, "off means opaque");
        assert_eq!(Settings { transparent_sidebar: true, sidebar_opacity: 0.0, ..settings }.sidebar_alpha(), 0.3);
    }
}
