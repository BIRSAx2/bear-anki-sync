use std::collections::{HashMap, HashSet};

use anyhow::Result;
use bear_cli::cloudkit::auth::AuthConfig;
use bear_cli::cloudkit::auth_server;
use bear_cli::cloudkit::client::CloudKitClient;
use sha2::{Digest, Sha256};

use crate::anki::{AnkiClient, AnkiNote};
use crate::auth_error::is_auth_error_message;
use crate::config::Config;
use crate::parser::{BearNote, Card, CardKind, parse_cards};
use crate::render::render_for_anki;
use crate::state::SyncState;

#[derive(Debug)]
pub struct SyncReport {
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub skipped: usize,
}

pub struct SyncOptions<'a> {
    pub tag_filter: Option<&'a str>,
    pub note_filter: Option<&'a str>,
    pub dry_run: bool,
    pub force: bool,
    pub verbose: bool,
    pub config: &'a Config,
}

pub fn load_client() -> Result<CloudKitClient> {
    match try_load_validated_client() {
        Ok(client) => Ok(client),
        Err(err) if is_auth_error_message(&format!("{err:#}")) => {
            eprintln!("bear-anki: CloudKit auth missing or expired, starting Apple sign-in...");
            authenticate_client()
        }
        Err(err) => Err(err),
    }
}

pub fn check_auth() -> Result<()> {
    try_load_validated_client().map(|_| ())
}

pub fn authenticate() -> Result<()> {
    authenticate_client().map(|_| ())
}

fn try_load_validated_client() -> Result<CloudKitClient> {
    let client = CloudKitClient::new(AuthConfig::load()?)?;
    client.list_tags()?;
    Ok(client)
}

fn authenticate_client() -> Result<CloudKitClient> {
    let token = auth_server::acquire_token()?;
    let auth = AuthConfig {
        ck_web_auth_token: token,
    };
    auth.save()?;

    let client = CloudKitClient::new(auth)?;
    client.list_tags()?;
    Ok(client)
}

pub fn export_notes(client: &CloudKitClient, tag_filter: Option<&str>) -> Result<Vec<BearNote>> {
    let mut notes = Vec::new();
    for record in client.list_notes(false, false, None)? {
        let tags = record.string_list_field("tagsStrings");
        if let Some(tag_filter) = tag_filter {
            if !tags.iter().any(|tag| tag == tag_filter) {
                continue;
            }
        }
        notes.push(BearNote {
            identifier: record.record_name.clone(),
            title: record.str_field("title").unwrap_or("").to_string(),
            text: record.str_field("textADP").unwrap_or("").to_string(),
            pinned: record.bool_field("pinned").unwrap_or(false),
            created_at: record.i64_field("sf_creationDate"),
            modified_at: record.i64_field("sf_modificationDate"),
            tags,
        });
    }
    Ok(notes)
}

pub fn note_title_map(client: &CloudKitClient) -> Result<HashMap<String, String>> {
    let mut titles = HashMap::new();
    for note in client.list_notes(false, true, None)? {
        let title = note.str_field("title").unwrap_or("").trim();
        titles.insert(
            note.record_name.clone(),
            if title.is_empty() {
                note.record_name.clone()
            } else {
                title.to_string()
            },
        );
    }
    Ok(titles)
}

pub fn sync(
    client_ck: &CloudKitClient,
    client: &AnkiClient,
    state: &mut SyncState,
    opts: &SyncOptions<'_>,
) -> Result<SyncReport> {
    let mut notes = export_notes(client_ck, opts.tag_filter)?;

    if let Some(title) = opts.note_filter {
        let title_lower = title.to_lowercase();
        notes.retain(|n| n.title.to_lowercase().contains(&title_lower));
        if notes.is_empty() {
            eprintln!("bear-anki: no notes found matching {:?}", title);
        }
    }

    let mut processed_note_ids: HashSet<String> = HashSet::new();
    let mut current: HashMap<(String, String), (Card, String)> = HashMap::new();
    let mut note_tags: HashMap<String, Vec<String>> = HashMap::new();

    for note in &notes {
        processed_note_ids.insert(note.identifier.clone());
        note_tags.insert(note.identifier.clone(), note.tags.clone());
        for card in parse_cards(note) {
            current.insert(
                (note.identifier.clone(), card.fingerprint.clone()),
                (card, note.identifier.clone()),
            );
        }
    }

    // Images are fetched lazily: only after we confirm a card needs add/update,
    // so skipped cards never trigger extra network calls.
    let mut note_image_cache: HashMap<String, Vec<(String, String)>> = HashMap::new();
    // Upload cache: deduplicate Anki media uploads within a single sync run.
    // Keyed on CloudKit download URL → Anki filename (e.g. "bear_image.png").
    let mut upload_cache: HashMap<String, String> = HashMap::new();

    let mut report = SyncReport {
        added: 0,
        updated: 0,
        deleted: 0,
        skipped: 0,
    };

    for ((note_id, fp), (card, _)) in &current {
        let hash = content_hash(card);
        let bear_tags = note_tags
            .get(note_id.as_str())
            .map_or(&[][..], Vec::as_slice);

        if let Some(anki_id) = state.get(note_id, fp) {
            if !opts.force && state.get_hash(note_id, fp) == Some(hash.as_str()) {
                if opts.verbose {
                    println!("[skip]   {} — {}", card.deck, card_preview(card));
                }
                report.skipped += 1;
                continue;
            }
            let image_files = fetch_images_cached(&mut note_image_cache, client_ck, note_id)?;
            let anki_note = build_anki_note(
                card,
                image_files,
                bear_tags,
                client,
                opts.config,
                &mut upload_cache,
            )?;
            if opts.dry_run {
                println!(
                    "[dry-run] would update Anki note {anki_id} (deck: {})",
                    card.deck
                );
            } else {
                client.create_deck(&card.deck)?;
                let update_result = client.update_note(anki_id, &anki_note.fields, &anki_note.tags);
                match update_result {
                    Ok(()) => {
                        client.move_note_to_deck(anki_id, &card.deck)?;
                        state.set_hash(note_id, fp, hash);
                    }
                    Err(err) if is_note_not_found(&err) => {
                        eprintln!("bear-anki: note {anki_id} not found in Anki, re-adding");
                        let new_id = client.add_note(&anki_note)?;
                        state.insert(note_id, fp, new_id);
                        state.set_hash(note_id, fp, hash);
                    }
                    Err(err) => return Err(err),
                }
                if opts.verbose {
                    println!("[update] {} — {}", card.deck, card_preview(card));
                }
            }
            report.updated += 1;
        } else {
            let image_files = fetch_images_cached(&mut note_image_cache, client_ck, note_id)?;
            let anki_note = build_anki_note(
                card,
                image_files,
                bear_tags,
                client,
                opts.config,
                &mut upload_cache,
            )?;
            if opts.dry_run {
                println!(
                    "[dry-run] would add card to deck {:?} ({})",
                    card.deck,
                    display_kind(card)
                );
            } else {
                client.create_deck(&card.deck)?;
                let anki_id = client.add_note(&anki_note)?;
                state.insert(note_id, fp, anki_id);
                state.set_hash(note_id, fp, hash);
                if opts.verbose {
                    println!("[add]    {} — {}", card.deck, card_preview(card));
                }
            }
            report.added += 1;
        }
    }

    let stale: Vec<(String, String, u64)> = state
        .all_keys()
        .filter(|(note_id, fp, _)| {
            if opts.note_filter.is_some() && !processed_note_ids.contains(*note_id) {
                return false;
            }
            !current.contains_key(&(note_id.to_string(), fp.to_string()))
        })
        .map(|(n, f, id)| (n.to_string(), f.to_string(), id))
        .collect();

    if !stale.is_empty() {
        let ids: Vec<u64> = stale.iter().map(|(_, _, id)| *id).collect();
        if opts.dry_run {
            println!("[dry-run] would delete {} Anki note(s)", ids.len());
        } else {
            client.delete_notes(&ids)?;
            for (note_id, fp, _) in &stale {
                state.remove(note_id, fp);
            }
        }
        report.deleted += stale.len();
    }

    if !opts.dry_run {
        state.save()?;
    }

    Ok(report)
}

fn fetch_images_cached<'a>(
    cache: &'a mut HashMap<String, Vec<(String, String)>>,
    client: &CloudKitClient,
    note_id: &str,
) -> Result<&'a [(String, String)]> {
    if !cache.contains_key(note_id) {
        cache.insert(note_id.to_owned(), note_image_urls(client, note_id)?);
    }
    Ok(cache[note_id].as_slice())
}

fn note_image_urls(client: &CloudKitClient, note_id: &str) -> Result<Vec<(String, String)>> {
    let note = client.fetch_note(note_id)?;
    let file_ids = note.string_list_field("files");
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }

    let refs: Vec<&str> = file_ids.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for file in client.lookup(&refs)? {
        if file.record_type != "SFNoteImage" {
            continue;
        }
        let Some(filename) = file.str_field("filenameADP").map(str::to_string) else {
            continue;
        };
        let Some(download_url) = file
            .fields
            .get("file")
            .and_then(|f| f.value.get("downloadURL"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        out.push((filename, download_url));
    }
    Ok(out)
}

fn build_anki_note(
    card: &Card,
    image_files: &[(String, String)],
    bear_tags: &[String],
    client: &AnkiClient,
    config: &Config,
    upload_cache: &mut HashMap<String, String>,
) -> Result<AnkiNote> {
    let mut tags = vec![config.tag_for(&card.callout_type)];
    for t in bear_tags {
        tags.push(t.replace('/', "::"));
    }

    let note = match &card.kind {
        CardKind::Basic { front, back } => AnkiNote {
            deck: card.deck.clone(),
            model: "Basic".into(),
            fields: [
                (
                    "Front".to_owned(),
                    render_for_anki(front, image_files, client, upload_cache)?,
                ),
                (
                    "Back".to_owned(),
                    render_for_anki(back, image_files, client, upload_cache)?,
                ),
            ]
            .into(),
            tags,
        },
        CardKind::Cloze { text } => AnkiNote {
            deck: card.deck.clone(),
            model: "Cloze".into(),
            fields: [(
                "Text".to_owned(),
                render_for_anki(text, image_files, client, upload_cache)?,
            )]
            .into(),
            tags,
        },
    };
    Ok(note)
}

fn display_kind(card: &Card) -> &str {
    match &card.kind {
        CardKind::Basic { .. } => "Basic",
        CardKind::Cloze { .. } => "Cloze",
    }
}

fn card_preview(card: &Card) -> String {
    let raw = match &card.kind {
        CardKind::Basic { front, .. } => front.as_str(),
        CardKind::Cloze { text } => text.as_str(),
    };
    let needs_ellipsis = raw.chars().count() > 60;
    let trimmed: String = raw.chars().take(60).collect();
    if needs_ellipsis {
        format!("{}…", trimmed)
    } else {
        trimmed
    }
}

fn is_note_not_found(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("not found") || msg.contains("no note with id")
}

fn content_hash(card: &Card) -> String {
    let mut h = Sha256::new();
    h.update(card.deck.as_bytes());
    h.update(b"\0");
    h.update(card.callout_type.as_bytes());
    h.update(b"\0");
    match &card.kind {
        CardKind::Basic { front, back } => {
            h.update(front.as_bytes());
            h.update(b"\0");
            h.update(back.as_bytes());
        }
        CardKind::Cloze { text } => {
            h.update(text.as_bytes());
        }
    }
    hex::encode(&h.finalize()[..8])
}
