use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

pub struct AnkiClient {
    url: String,
}

#[derive(Clone)]
pub struct AnkiNote {
    pub deck: String,
    pub model: String,
    pub fields: HashMap<String, String>,
    pub tags: Vec<String>,
}

impl AnkiClient {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
        }
    }

    pub fn check_connection(&self) -> Result<()> {
        self.request("requestPermission", json!({}))?;
        Ok(())
    }

    pub fn create_deck(&self, name: &str) -> Result<()> {
        self.request("createDeck", json!({ "deck": name }))?;
        Ok(())
    }

    pub fn add_notes(&self, notes: &[&AnkiNote]) -> Result<Vec<u64>> {
        if notes.is_empty() {
            return Ok(Vec::new());
        }

        let notes_json: Vec<Value> = notes
            .iter()
            .map(|note| {
                json!({
                    "deckName": note.deck,
                    "modelName": note.model,
                    "fields": note.fields,
                    "tags": note.tags,
                    "options": { "allowDuplicate": false }
                })
            })
            .collect();
        let expected = notes_json.len();
        let result = self.request("addNotes", json!({ "notes": notes_json }))?;
        let ids = result
            .as_array()
            .context("addNotes did not return an array")?
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_u64()
                    .with_context(|| format!("addNotes did not return a note ID at index {index}"))
            })
            .collect::<Result<Vec<_>>>()?;
        if ids.len() != expected {
            bail!(
                "addNotes returned {} note IDs for {} requested notes",
                ids.len(),
                expected
            );
        }
        Ok(ids)
    }

    pub fn add_note(&self, note: &AnkiNote) -> Result<u64> {
        let result = self.request(
            "addNote",
            json!({
                "note": {
                    "deckName": note.deck,
                    "modelName": note.model,
                    "fields": note.fields,
                    "tags": note.tags,
                    "options": { "allowDuplicate": false }
                }
            }),
        )?;
        result
            .as_u64()
            .context("addNote did not return a numeric note ID")
    }

    pub fn update_note(
        &self,
        id: u64,
        fields: &HashMap<String, String>,
        tags: &[String],
    ) -> Result<()> {
        self.request(
            "updateNote",
            json!({ "note": { "id": id, "fields": fields, "tags": tags } }),
        )?;
        Ok(())
    }

    pub fn update_notes(&self, updates: &[(u64, &AnkiNote)]) -> Result<Vec<Option<String>>> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }

        let actions: Vec<Value> = updates
            .iter()
            .map(|(id, note)| {
                json!({
                    "action": "updateNote",
                    "version": 6,
                    "params": {
                        "note": {
                            "id": id,
                            "fields": note.fields,
                            "tags": note.tags
                        }
                    }
                })
            })
            .collect();
        self.multi_action_errors(actions)
    }

    /// Move all cards of a note to the given deck.
    pub fn move_note_to_deck(&self, note_id: u64, deck: &str) -> Result<()> {
        let info = self.request("notesInfo", json!({ "notes": [note_id] }))?;
        let card_ids: Vec<serde_json::Value> = info[0]["cards"]
            .as_array()
            .map(|v| v.to_vec())
            .unwrap_or_default();
        if !card_ids.is_empty() {
            self.request("changeDeck", json!({ "cards": card_ids, "deck": deck }))?;
        }
        Ok(())
    }

    /// Move all cards for each note to its target deck, grouping cards by deck.
    pub fn move_notes_to_decks(&self, moves: &[(u64, &str)]) -> Result<()> {
        if moves.is_empty() {
            return Ok(());
        }

        let note_ids: Vec<u64> = moves.iter().map(|(note_id, _)| *note_id).collect();
        let info = self.request("notesInfo", json!({ "notes": note_ids }))?;
        let infos = info
            .as_array()
            .context("notesInfo did not return an array")?;
        if infos.len() != moves.len() {
            bail!(
                "notesInfo returned {} entries for {} requested notes",
                infos.len(),
                moves.len()
            );
        }

        let mut by_deck: HashMap<String, Vec<Value>> = HashMap::new();
        for ((_, deck), note_info) in moves.iter().zip(infos) {
            let cards = note_info
                .get("cards")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            by_deck.entry((*deck).to_owned()).or_default().extend(cards);
        }

        let actions: Vec<Value> = by_deck
            .into_iter()
            .filter(|(_, cards)| !cards.is_empty())
            .map(|(deck, cards)| {
                json!({
                    "action": "changeDeck",
                    "version": 6,
                    "params": {
                        "cards": cards,
                        "deck": deck
                    }
                })
            })
            .collect();
        let errors = self.multi_action_errors(actions)?;
        if let Some(error) = errors.into_iter().flatten().next() {
            bail!("AnkiConnect error changing deck: {error}");
        }
        Ok(())
    }

    pub fn delete_notes(&self, ids: &[u64]) -> Result<()> {
        self.request("deleteNotes", json!({ "notes": ids }))?;
        Ok(())
    }

    pub fn store_media_file(&self, filename: &str, data_base64: &str) -> Result<()> {
        self.request(
            "storeMediaFile",
            json!({ "filename": filename, "data": data_base64 }),
        )?;
        Ok(())
    }

    fn multi_action_errors(&self, actions: Vec<Value>) -> Result<Vec<Option<String>>> {
        if actions.is_empty() {
            return Ok(Vec::new());
        }

        let expected = actions.len();
        let result = self.request("multi", json!({ "actions": actions }))?;
        let items = result.as_array().context("multi did not return an array")?;
        if items.len() != expected {
            bail!(
                "multi returned {} results for {} requested actions",
                items.len(),
                expected
            );
        }
        Ok(items
            .iter()
            .map(|item| {
                item.get("error")
                    .and_then(Value::as_str)
                    .filter(|error| !error.is_empty())
                    .map(str::to_owned)
            })
            .collect())
    }

    fn request(&self, action: &str, params: Value) -> Result<Value> {
        let body = json!({
            "action": action,
            "version": 6,
            "params": params,
        });

        let body_str = serde_json::to_string(&body)?;
        let raw = ureq::post(&self.url)
            .set("Content-Type", "application/json")
            .send_string(&body_str)
            .map_err(|err| {
                let err_text = err.to_string();
                let err_text_lower = err_text.to_lowercase();
                let is_connection_err = err_text_lower.contains("connection refused")
                    || err_text_lower.contains("connect error");
                if is_connection_err {
                    anyhow!(
                        "could not connect to Anki at {}\n\
                         Make sure Anki is running and the AnkiConnect plugin is installed.\n\
                        Install it from: https://ankiweb.net/shared/info/2055492159",
                        self.url
                    )
                } else {
                    anyhow!("AnkiConnect request failed: {err_text}")
                }
            })?
            .into_string()
            .context("failed to read AnkiConnect response")?;
        let response: Value =
            serde_json::from_str(&raw).context("failed to parse AnkiConnect response")?;

        if let Some(error) = response.get("error").and_then(|v: &Value| v.as_str()) {
            if !error.is_empty() {
                bail!("AnkiConnect error for action {action}: {error}");
            }
        }

        Ok(response["result"].clone())
    }
}
