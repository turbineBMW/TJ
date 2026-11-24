//! TJ's own settings, in `settings.toml` next to `accounts.toml`.
//!
//! As in the rest of the app family, how see-through the sidebar is belongs to the app,
//! not to the theme: a switch (off by default) and an opacity (0.8 by default).

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
    /// How long the cursor takes to glide to a new place, in milliseconds (0: it jumps).
    pub cursor_glide_ms: u32,
    /// The cursor stretches along its path as it moves, and snaps back.
    pub cursor_trail: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            follow_omarchy_theme: true,
            transparent_sidebar: false,
            sidebar_opacity: 0.8,
            cursor_glide_ms: 90,
            cursor_trail: true,
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
        let settings = Settings {
            transparent_sidebar: true,
            sidebar_opacity: 0.6,
            cursor_glide_ms: 150,
            cursor_trail: false,
            ..Default::default()
        };
        settings.save(&paths).unwrap();
        assert_eq!(Settings::load(&paths), settings);
        assert_eq!(settings.sidebar_alpha(), 0.6);
        assert_eq!(Settings::default().sidebar_alpha(), 1.0, "off means opaque");
        assert_eq!(Settings { transparent_sidebar: true, sidebar_opacity: 0.0, ..settings }.sidebar_alpha(), 0.3);
    }
}
