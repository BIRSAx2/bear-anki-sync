use std::collections::{HashMap, HashSet};

use anyhow::Result;
use bear_rs::SqliteStore;
use sha2::{Digest, Sha256};

use crate::anki::{AnkiClient, AnkiNote};
use crate::config::Config;
use crate::parser::{parse_cards, BearNote, Card, CardKind};
use crate::render::{render_for_anki, NoteImage};
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

pub fn load_client() -> Result<SqliteStore> {
    SqliteStore::open_ro()
}

pub fn export_notes(store: &SqliteStore, tag_filter: Option<&str>) -> Result<Vec<BearNote>> {
    let list_input = bear_rs::store::ListInput {
        tag: tag_filter,
        include_tags: true,
        ..Default::default()
    };

    let mut notes = Vec::new();
    for note in store.list_notes(&list_input)? {
        notes.push(BearNote {
            identifier: note.id,
            title: note.title,
            text: note.text,
            pinned: note.pinned,
            created_at: Some(note.created),
            modified_at: Some(note.modified),
            tags: note.tags,
        });
    }
    Ok(notes)
}

pub fn note_title_map(store: &SqliteStore) -> Result<HashMap<String, String>> {
    let mut titles = HashMap::new();
    for note in store.list_notes(&Default::default())? {
        let title = note.title.trim();
        titles.insert(
            note.id.clone(),
            if title.is_empty() {
                note.id
            } else {
                title.to_string()
            },
        );
    }
    Ok(titles)
}

pub fn sync(
    store: &SqliteStore,
    client: &AnkiClient,
    state: &mut SyncState,
    opts: &SyncOptions<'_>,
) -> Result<SyncReport> {
    let mut notes = export_notes(store, opts.tag_filter)?;

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
    let mut note_image_cache: HashMap<String, Vec<NoteImage>> = HashMap::new();
    // Upload cache: deduplicate Anki media uploads within a single sync run.
    // Keyed on Bear attachment identity → Anki filename (e.g. "bear_image.png").
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
            let image_files = fetch_images_cached(&mut note_image_cache, store, note_id)?;
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
            let image_files = fetch_images_cached(&mut note_image_cache, store, note_id)?;
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
    cache: &'a mut HashMap<String, Vec<NoteImage>>,
    store: &SqliteStore,
    note_id: &str,
) -> Result<&'a [NoteImage]> {
    if !cache.contains_key(note_id) {
        cache.insert(note_id.to_owned(), note_images(store, note_id)?);
    }
    Ok(cache[note_id].as_slice())
}

fn note_images(store: &SqliteStore, note_id: &str) -> Result<Vec<NoteImage>> {
    let attachments = store.list_attachments(Some(note_id), None)?;
    let mut out = Vec::new();
    for attachment in attachments {
        let Some(ext) = attachment.filename.rsplit('.').next() else {
            continue;
        };
        if !matches!(
            ext.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff"
        ) {
            continue;
        }
        out.push(NoteImage {
            filename: attachment.filename.clone(),
            upload_key: format!("{note_id}:{}", attachment.uuid),
            data: store.read_attachment(Some(note_id), None, &attachment.filename)?,
        });
    }
    Ok(out)
}

fn build_anki_note(
    card: &Card,
    image_files: &[NoteImage],
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
