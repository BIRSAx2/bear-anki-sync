use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use bear_cli::db::BearDb;
use sha2::{Digest, Sha256};

use crate::anki::{AnkiClient, AnkiNote};
use crate::parser::{Card, CardKind, parse_cards};
use crate::render::render_for_anki;
use crate::state::SyncState;

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
}

pub fn sync(
    db: &BearDb,
    client: &AnkiClient,
    state: &mut SyncState,
    media_dir: &Path,
    opts: &SyncOptions<'_>,
) -> Result<SyncReport> {
    let mut notes = db.export_notes(opts.tag_filter)?;

    // Filter to a specific note by title if requested
    if let Some(title) = opts.note_filter {
        let title_lower = title.to_lowercase();
        notes.retain(|n| n.title.to_lowercase().contains(&title_lower));
        if notes.is_empty() {
            eprintln!("bear-anki: no notes found matching {:?}", title);
        }
    }

    // Track which note IDs we actually processed (for scoped stale detection)
    let mut processed_note_ids: HashSet<String> = HashSet::new();

    // Build current card map: (note_id, fingerprint) → Card
    let mut current: HashMap<(String, String), (Card, String)> = HashMap::new();
    // note_id → Bear tags (for forwarding to Anki)
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

    // Pre-build image file map: note_id → [(filename, path)]
    let mut note_image_files: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
    for note in &notes {
        let files = db.note_files(&note.identifier)?;
        let image_files: Vec<(String, PathBuf)> = files
            .into_iter()
            .map(|f| {
                let path = media_dir.join(&f.file_uuid).join(&f.filename);
                (f.filename, path)
            })
            .filter(|(_, path)| path.exists())
            .collect();
        if !image_files.is_empty() {
            note_image_files.insert(note.identifier.clone(), image_files);
        }
    }

    let mut report = SyncReport {
        added: 0,
        updated: 0,
        deleted: 0,
        skipped: 0,
    };

    // Add or update
    for ((note_id, fp), (card, _)) in &current {
        let hash = content_hash(card);
        let image_files = note_image_files
            .get(note_id.as_str())
            .map_or(&[][..], Vec::as_slice);
        let bear_tags = note_tags
            .get(note_id.as_str())
            .map_or(&[][..], Vec::as_slice);

        if let Some(anki_id) = state.get(note_id, fp) {
            // Exists — skip if content unchanged (unless --force)
            if !opts.force && state.get_hash(note_id, fp) == Some(hash.as_str()) {
                if opts.verbose {
                    println!("[skip]   {} — {}", card.deck, card_preview(card));
                }
                report.skipped += 1;
                continue;
            }
            let anki_note = build_anki_note(card, image_files, bear_tags, client)?;
            if opts.dry_run {
                println!(
                    "[dry-run] would update Anki note {anki_id} (deck: {})",
                    card.deck
                );
            } else {
                client.create_deck(&card.deck)?;
                let update_result =
                    client.update_note(anki_id, anki_note.fields.clone(), &anki_note.tags);
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
            // New card
            let anki_note = build_anki_note(card, image_files, bear_tags, client)?;
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

    // Delete stale entries — only within processed notes when a note filter is active
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

fn build_anki_note(
    card: &Card,
    image_files: &[(String, PathBuf)],
    bear_tags: &[String],
    client: &AnkiClient,
) -> Result<AnkiNote> {
    let mut tags = vec![format!("bear-{}", card.callout_type)];
    for t in bear_tags {
        // Normalise Bear's hierarchy separator / → :: for Anki
        tags.push(t.replace('/', "::"));
    }

    let note = match &card.kind {
        CardKind::Basic { front, back } => AnkiNote {
            deck: card.deck.clone(),
            model: "Basic".into(),
            fields: [
                (
                    "Front".to_owned(),
                    render_for_anki(front, image_files, client)?,
                ),
                (
                    "Back".to_owned(),
                    render_for_anki(back, image_files, client)?,
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
                render_for_anki(text, image_files, client)?,
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
    let trimmed: String = raw.chars().take(60).collect();
    if raw.len() > 60 {
        format!("{}…", trimmed)
    } else {
        trimmed
    }
}

fn is_note_not_found(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("not found") || msg.contains("no note with id")
}

/// Stable hash of all card content (deck + fields + callout type).
/// Changing any of these triggers an Anki update on next sync.
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
