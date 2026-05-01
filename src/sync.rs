use std::collections::{HashMap, HashSet};

use anyhow::Result;
use bear_rs::SqliteStore;
use sha2::{Digest, Sha256};

use crate::anki::{AnkiClient, AnkiNote};
use crate::config::Config;
use crate::parser::{parse_cards, BearNote, Card, CardKind};
use crate::render::{referenced_images, render_for_anki, NoteImage};
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
    let mut current_order: Vec<(String, String)> = Vec::new();
    let mut current: HashMap<(String, String), Card> = HashMap::new();
    let mut note_tags: HashMap<String, Vec<String>> = HashMap::new();

    for note in &notes {
        processed_note_ids.insert(note.identifier.clone());
        note_tags.insert(note.identifier.clone(), note.tags.clone());
        for card in parse_cards(note) {
            let key = (note.identifier.clone(), card.fingerprint.clone());
            if !current.contains_key(&key) {
                current_order.push(key.clone());
            }
            current.insert(key, card);
        }
    }

    // Images are read lazily per note. Hashing needs referenced media metadata,
    // but uploads only happen when rendering a real add/update.
    let mut note_image_cache: HashMap<String, Vec<NoteImage>> = HashMap::new();
    // Upload cache: deduplicate Anki media uploads within a single sync run.
    // Keyed on Bear attachment identity -> unique Anki filename.
    let mut upload_cache: HashMap<String, String> = HashMap::new();

    let mut report = SyncReport {
        added: 0,
        updated: 0,
        deleted: 0,
        skipped: 0,
    };

    for (note_id, fp) in &current_order {
        let card = current
            .get(&(note_id.clone(), fp.clone()))
            .expect("ordered current card key should exist");
        let bear_tags = note_tags
            .get(note_id.as_str())
            .map_or(&[][..], Vec::as_slice);
        let tags = build_tags(card, bear_tags, opts.config);
        let image_files = fetch_images_cached(&mut note_image_cache, store, note_id)?;
        let media_refs = card_referenced_images(card, image_files);
        let hash = content_hash(card, &tags, &media_refs);

        if let Some(anki_id) = state.get(note_id, fp) {
            if !opts.force && state.get_hash(note_id, fp) == Some(hash.as_str()) {
                if opts.verbose {
                    println!("[skip]   {} — {}", card.deck, card_preview(card));
                }
                report.skipped += 1;
                continue;
            }
            if opts.dry_run {
                println!(
                    "[dry-run] would update Anki note {anki_id} (deck: {})",
                    card.deck
                );
                report.updated += 1;
                continue;
            }

            let anki_note = build_anki_note(card, image_files, tags, client, &mut upload_cache)?;
            client.create_deck(&card.deck)?;
            let update_result = client.update_note(anki_id, &anki_note.fields, &anki_note.tags);
            match update_result {
                Ok(()) => {
                    client.move_note_to_deck(anki_id, &card.deck)?;
                    state.set_hash(note_id, fp, hash);
                    state.save()?;
                }
                Err(err) if is_note_not_found(&err) => {
                    eprintln!("bear-anki: note {anki_id} not found in Anki, re-adding");
                    let new_id = client.add_note(&anki_note)?;
                    state.insert(note_id, fp, new_id);
                    state.set_hash(note_id, fp, hash);
                    state.save()?;
                }
                Err(err) => return Err(err),
            }
            if opts.verbose {
                println!("[update] {} — {}", card.deck, card_preview(card));
            }
            report.updated += 1;
        } else {
            if opts.dry_run {
                println!(
                    "[dry-run] would add card to deck {:?} ({})",
                    card.deck,
                    display_kind(card)
                );
                report.added += 1;
                continue;
            }

            let anki_note = build_anki_note(card, image_files, tags, client, &mut upload_cache)?;
            client.create_deck(&card.deck)?;
            let anki_id = client.add_note(&anki_note)?;
            state.insert(note_id, fp, anki_id);
            state.set_hash(note_id, fp, hash);
            state.save()?;
            if opts.verbose {
                println!("[add]    {} — {}", card.deck, card_preview(card));
            }
            report.added += 1;
        }
    }

    let stale: Vec<(String, String, u64)> = state
        .all_keys()
        .filter(|(note_id, fp, _)| {
            if (opts.note_filter.is_some() || opts.tag_filter.is_some())
                && !processed_note_ids.contains(*note_id)
            {
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
            state.save()?;
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
        out.push(NoteImage::new(
            attachment.filename.clone(),
            format!("{note_id}:{}", attachment.uuid),
            store.read_attachment(Some(note_id), None, &attachment.filename)?,
        ));
    }
    Ok(out)
}

fn build_anki_note(
    card: &Card,
    image_files: &[NoteImage],
    tags: Vec<String>,
    client: &AnkiClient,
    upload_cache: &mut HashMap<String, String>,
) -> Result<AnkiNote> {
    let note = match &card.kind {
        CardKind::Basic { front, back } => AnkiNote {
            deck: card.deck.clone(),
            model: "Basic".into(),
            fields: [
                (
                    "Front".to_owned(),
                    with_sort_key_prefix(
                        render_for_anki(front, image_files, client, upload_cache)?,
                        &card.sort_key,
                    ),
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
                with_sort_key_prefix(
                    render_for_anki(text, image_files, client, upload_cache)?,
                    &card.sort_key,
                ),
            )]
            .into(),
            tags,
        },
    };
    Ok(note)
}

fn build_tags(card: &Card, bear_tags: &[String], config: &Config) -> Vec<String> {
    let mut tags = vec![config.tag_for(&card.callout_type)];
    for tag in bear_tags {
        tags.push(tag.replace('/', "::"));
    }
    dedup_preserve_order(tags)
}

fn dedup_preserve_order(tags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        if seen.insert(tag.clone()) {
            out.push(tag);
        }
    }
    out
}

fn card_referenced_images<'a>(card: &Card, image_files: &'a [NoteImage]) -> Vec<&'a NoteImage> {
    let mut refs = Vec::new();
    match &card.kind {
        CardKind::Basic { front, back } => {
            refs.extend(referenced_images(front, image_files));
            refs.extend(referenced_images(back, image_files));
        }
        CardKind::Cloze { text } => refs.extend(referenced_images(text, image_files)),
    }
    dedup_images(refs)
}

fn dedup_images(images: Vec<&NoteImage>) -> Vec<&NoteImage> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(images.len());
    for image in images {
        if seen.insert(image.upload_key.as_str()) {
            out.push(image);
        }
    }
    out
}

fn with_sort_key_prefix(html: String, sort_key: &str) -> String {
    let mut out = String::with_capacity(html.len() + sort_key.len() + 64);
    out.push_str("<span class=\"bear-anki-order\" style=\"display:none\">");
    out.push_str(&escape_html_text(sort_key));
    out.push_str(" </span>");
    out.push_str(&html);
    out
}

fn escape_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
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

fn content_hash(card: &Card, tags: &[String], images: &[&NoteImage]) -> String {
    let mut h = Sha256::new();
    h.update(card.deck.as_bytes());
    h.update(b"\0");
    h.update(card.sort_key.as_bytes());
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
    h.update(b"\0tags\0");
    for tag in tags {
        h.update(tag.as_bytes());
        h.update(b"\0");
    }
    h.update(b"\0images\0");
    for image in images {
        h.update(image.filename.as_bytes());
        h.update(b"\0");
        h.update(image.upload_key.as_bytes());
        h.update(b"\0");
        h.update(image.content_hash.as_bytes());
        h.update(b"\0");
        h.update(image.anki_filename().as_bytes());
        h.update(b"\0");
    }
    hex::encode(&h.finalize()[..8])
}

#[cfg(test)]
mod tests {
    use super::{build_tags, card_referenced_images, content_hash, with_sort_key_prefix};
    use crate::config::Config;
    use crate::parser::{Card, CardKind};
    use crate::render::NoteImage;

    fn make_card(sort_key: &str) -> Card {
        Card {
            kind: CardKind::Basic {
                front: "Front".into(),
                back: "Back".into(),
            },
            deck: "Deck".into(),
            fingerprint: "fingerprint".into(),
            callout_type: "card".into(),
            sort_key: sort_key.into(),
        }
    }

    #[test]
    fn sort_key_prefix_is_hidden_html_before_the_sort_field() {
        let html = with_sort_key_prefix("<p>Front</p>\n".to_owned(), "000001");
        assert_eq!(
            html,
            "<span class=\"bear-anki-order\" style=\"display:none\">000001 </span><p>Front</p>\n"
        );
    }

    #[test]
    fn content_hash_changes_when_source_order_changes() {
        let tags = vec!["bear-card".to_owned()];
        assert_ne!(
            content_hash(&make_card("000001"), &tags, &[]),
            content_hash(&make_card("000002"), &tags, &[])
        );
    }

    #[test]
    fn content_hash_changes_when_tags_change() {
        let card = make_card("000001");
        assert_ne!(
            content_hash(&card, &["bear-card".to_owned()], &[]),
            content_hash(&card, &["bear-card".to_owned(), "topic".to_owned()], &[])
        );
    }

    #[test]
    fn content_hash_changes_when_referenced_image_changes() {
        let card = Card {
            kind: CardKind::Basic {
                front: "See ![](image.png)".into(),
                back: "Back".into(),
            },
            ..make_card("000001")
        };
        let tags = vec!["bear-card".to_owned()];
        let image_a = NoteImage::new("image.png".into(), "note:image".into(), b"a".to_vec());
        let image_b = NoteImage::new("image.png".into(), "note:image".into(), b"b".to_vec());
        let refs_a = card_referenced_images(&card, std::slice::from_ref(&image_a));
        let refs_b = card_referenced_images(&card, std::slice::from_ref(&image_b));

        assert_ne!(
            content_hash(&card, &tags, &refs_a),
            content_hash(&card, &tags, &refs_b)
        );
    }

    #[test]
    fn build_tags_deduplicates_preserving_callout_tag_first() {
        let card = make_card("000001");
        let tags = build_tags(
            &card,
            &[
                "topic".to_owned(),
                "topic".to_owned(),
                "nested/tag".to_owned(),
            ],
            &Config::default(),
        );

        assert_eq!(tags, vec!["bear-card", "topic", "nested::tag"]);
    }
}
