//! Remembers the last pick so the menu opens where it was left.
//!
//! Deliberately not a preferences file: only what is needed to move the cursor onto a row.
//! It never skips a screen, that is what the command-line flags are for. A missing or
//! unparsable state file is not an error, it just means starting at the top.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub last_harness: Option<String>,
    #[serde(default)]
    pub last_provider: Option<String>,
    #[serde(default)]
    pub last_model: Option<String>,
}

fn path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("state.toml"))
}

pub fn load() -> State {
    let Some(p) = path() else {
        return State::default();
    };
    let Ok(raw) = std::fs::read_to_string(p) else {
        return State::default();
    };
    toml::from_str(&raw).unwrap_or_default()
}

pub fn save(harness: &str, provider: &str, model: &str) {
    let Some(p) = path() else { return };
    let state = State {
        last_harness: Some(harness.to_string()),
        last_provider: Some(provider.to_string()),
        last_model: Some(model.to_string()),
    };
    let Ok(raw) = toml::to_string_pretty(&state) else {
        return;
    };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Written beside the target and renamed over it. A plain write truncates first, so two
    // fastpick runs ending at the same moment can leave a half-written file, and the next
    // start would quietly forget where the cursor was.
    let tmp = p.with_extension("toml.tmp");
    if std::fs::write(&tmp, raw).is_ok() && std::fs::rename(&tmp, &p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
