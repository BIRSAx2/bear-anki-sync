use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    cards: HashMap<String, u64>,
    /// SHA-256(content)[..8] per card — absent in v1 state files (treated as "")
    #[serde(default)]
    hashes: HashMap<String, String>,
}

pub struct SyncState {
    cards: HashMap<String, u64>,
    hashes: HashMap<String, String>,
    path: PathBuf,
}

impl SyncState {
    pub fn load() -> Result<Self> {
        let path = state_path()?;
        let (cards, hashes) = if path.exists() {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let file: StateFile = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            (file.cards, file.hashes)
        } else {
            (HashMap::new(), HashMap::new())
        };
        Ok(Self {
            cards,
            hashes,
            path,
        })
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = StateFile {
            version: 1,
            cards: self.cards.clone(),
            hashes: self.hashes.clone(),
        };
        let contents = serde_json::to_string_pretty(&file)?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &contents).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                tmp.display(),
                self.path.display()
            )
        })?;
        Ok(())
    }

    pub fn get(&self, note_id: &str, fp: &str) -> Option<u64> {
        self.cards.get(&make_key(note_id, fp)).copied()
    }

    pub fn insert(&mut self, note_id: &str, fp: &str, anki_id: u64) {
        self.cards.insert(make_key(note_id, fp), anki_id);
    }

    pub fn remove(&mut self, note_id: &str, fp: &str) -> Option<u64> {
        let key = make_key(note_id, fp);
        self.hashes.remove(&key);
        self.cards.remove(&key)
    }

    pub fn get_hash(&self, note_id: &str, fp: &str) -> Option<&str> {
        self.hashes.get(&make_key(note_id, fp)).map(String::as_str)
    }

    pub fn set_hash(&mut self, note_id: &str, fp: &str, hash: String) {
        self.hashes.insert(make_key(note_id, fp), hash);
    }

    #[allow(dead_code)]
    pub fn keys_for_note(&self, note_id: &str) -> Vec<String> {
        let prefix = format!("{note_id}:");
        self.cards
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect()
    }

    pub fn all_keys(&self) -> impl Iterator<Item = (&str, &str, u64)> {
        self.cards.iter().filter_map(|(key, &anki_id)| {
            let (note_id, fp) = key.split_once(':')?;
            Some((note_id, fp, anki_id))
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

fn make_key(note_id: &str, fp: &str) -> String {
    format!("{note_id}:{fp}")
}

fn state_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("bear-anki")
        .join("state.json"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{SyncState, make_key};

    // Each test gets a unique file path to avoid concurrent test interference.
    fn temp_state(tag: &str) -> SyncState {
        let path = std::env::temp_dir().join(format!(
            "bear-anki-state-test-{}-{tag}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        SyncState {
            cards: std::collections::HashMap::new(),
            hashes: std::collections::HashMap::new(),
            path,
        }
    }

    #[test]
    fn round_trips_insert_and_get() {
        let mut state = temp_state("get");
        state.insert("NOTE-1", "aabbccdd11223344", 42);
        assert_eq!(state.get("NOTE-1", "aabbccdd11223344"), Some(42));
        assert_eq!(state.get("NOTE-1", "other"), None);
    }

    #[test]
    fn remove_returns_id_and_clears_entry() {
        let mut state = temp_state("remove");
        state.insert("NOTE-1", "fp1", 99);
        assert_eq!(state.remove("NOTE-1", "fp1"), Some(99));
        assert_eq!(state.get("NOTE-1", "fp1"), None);
    }

    #[test]
    fn keys_for_note_returns_correct_entries() {
        let mut state = temp_state("keys");
        state.insert("NOTE-1", "fp1", 1);
        state.insert("NOTE-1", "fp2", 2);
        state.insert("NOTE-2", "fp3", 3);
        let mut keys = state.keys_for_note("NOTE-1");
        keys.sort();
        assert_eq!(
            keys,
            vec![make_key("NOTE-1", "fp1"), make_key("NOTE-1", "fp2")]
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let mut state = temp_state("roundtrip");
        let path = state.path.clone();
        state.insert("NOTE-1", "fp1", 111);
        state.insert("NOTE-2", "fp2", 222);
        state.save().expect("save should succeed");

        let loaded = SyncState {
            cards: {
                let contents = fs::read_to_string(&path).unwrap();
                let file: super::StateFile = serde_json::from_str(&contents).unwrap();
                file.cards
            },
            hashes: std::collections::HashMap::new(),
            path: path.clone(),
        };
        assert_eq!(loaded.get("NOTE-1", "fp1"), Some(111));
        assert_eq!(loaded.get("NOTE-2", "fp2"), Some(222));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let path = std::env::temp_dir().join(format!(
            "bear-anki-nonexistent-state-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let state = SyncState {
            cards: std::collections::HashMap::new(),
            hashes: std::collections::HashMap::new(),
            path,
        };
        assert_eq!(state.get("NOTE-1", "fp1"), None);
    }

    #[test]
    fn atomic_write_uses_tmp_then_renames() {
        let mut state = temp_state("atomic");
        state.insert("NOTE-1", "fp1", 1);
        state.save().expect("save should succeed");

        let tmp = state.path.with_extension("json.tmp");
        assert!(!tmp.exists(), ".tmp file should be cleaned up after save");
        assert!(state.path.exists(), "state file should exist");

        let _ = fs::remove_file(&state.path);
    }
}
