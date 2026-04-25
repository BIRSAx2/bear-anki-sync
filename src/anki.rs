use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

pub struct AnkiClient {
    url: String,
}

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
