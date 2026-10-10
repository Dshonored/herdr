//! Desktop-app preferences, stored beside the endpoint catalog. Machines stay in the
//! shared catalog so the TUI and the desktop app list the same ones.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::warn;

pub(crate) const MIN_FONT_SIZE: f32 = 10.0;
pub(crate) const MAX_FONT_SIZE: f32 = 24.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct GuiPrefs {
    pub(crate) font_size: f32,
    /// Show agent usage for the selected machine.
    pub(crate) usage_enabled: bool,
    /// Also read usage from SSH machines (runs a read-only `find`/`grep` over SSH).
    pub(crate) remote_usage: bool,
    /// Download new desktop releases in the background; installing still asks to restart.
    pub(crate) auto_download_updates: bool,
}

impl Default for GuiPrefs {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            usage_enabled: true,
            remote_usage: true,
            auto_download_updates: true,
        }
    }
}

fn prefs_path() -> PathBuf {
    crate::config::state_dir().join("client").join("gui.json")
}

impl GuiPrefs {
    pub(crate) fn load() -> Self {
        let Ok(text) = std::fs::read_to_string(prefs_path()) else {
            return Self::default();
        };
        let mut prefs: Self = serde_json::from_str(&text).unwrap_or_else(|error| {
            warn!(%error, "desktop app preferences are invalid; using defaults");
            Self::default()
        });
        prefs.font_size = prefs.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        prefs
    }

    pub(crate) fn store(&self) -> Result<(), String> {
        let path = prefs_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(&path, text).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        let prefs: GuiPrefs = serde_json::from_str(r#"{"font_size":16}"#).unwrap();
        assert_eq!(prefs.font_size, 16.0);
        assert!(prefs.usage_enabled);
        assert!(prefs.remote_usage);
    }
}
