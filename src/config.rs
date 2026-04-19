use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// bear-anki configuration.
///
/// Loaded from `~/Library/Application Support/bear-anki/config.toml`.
/// Missing file → all defaults. Unknown keys are ignored.
///
/// Precedence for `anki_url`:
///   CLI flag / env var  >  config file  >  built-in default
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Override AnkiConnect URL (default: `http://127.0.0.1:8765`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anki_url: Option<String>,

    /// Background sync cadence for the menu bar app.
    /// `None` or `0` disables periodic sync.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_interval_minutes: Option<u64>,

    /// Maps callout type (lowercase) → Anki tag.
    ///
    /// Example in config.toml:
    /// ```toml
    /// [tags]
    /// important = "exam-critical"
    /// warning   = "pitfall"
    /// ```
    pub tags: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            anki_url: None,
            sync_interval_minutes: None,
            tags: default_tags(),
        }
    }
}

fn default_tags() -> HashMap<String, String> {
    [
        ("important", "bear-important"),
        ("note", "bear-note"),
        ("tip", "bear-tip"),
        ("warning", "bear-warning"),
        ("card", "bear-card"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

impl Config {
    /// Load from disk. Returns `Config::default()` when the file is absent.
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write the current config to disk (creates parent directories as needed).
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Anki tag for the given callout type.
    /// Falls back to `"bear-{type}"` for any type not listed in `[tags]`.
    pub fn tag_for(&self, callout_type: &str) -> String {
        self.tags
            .get(callout_type)
            .cloned()
            .unwrap_or_else(|| format!("bear-{callout_type}"))
    }

    pub fn path() -> Result<PathBuf> {
        config_path()
    }
}

fn config_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("bear-anki")
        .join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn default_tags_cover_all_callout_types() {
        let cfg = Config::default();
        for ct in &["important", "note", "tip", "warning", "card"] {
            assert!(cfg.tags.contains_key(*ct), "missing default tag for {ct}");
        }
    }

    #[test]
    fn tag_for_returns_configured_value() {
        let mut cfg = Config::default();
        cfg.tags.insert("important".into(), "exam-critical".into());
        assert_eq!(cfg.tag_for("important"), "exam-critical");
    }

    #[test]
    fn tag_for_falls_back_for_unknown_type() {
        let cfg = Config::default();
        assert_eq!(cfg.tag_for("custom"), "bear-custom");
    }

    #[test]
    fn round_trips_through_toml() {
        let mut cfg = Config {
            anki_url: Some("http://localhost:9999".into()),
            ..Config::default()
        };
        cfg.tags.insert("important".into(), "must-know".into());

        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(parsed.anki_url.as_deref(), Some("http://localhost:9999"));
        assert_eq!(parsed.tag_for("important"), "must-know");
        assert_eq!(parsed.tag_for("note"), "bear-note"); // unchanged default
    }
}
