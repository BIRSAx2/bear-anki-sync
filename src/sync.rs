use std::collections::{HashMap, HashSet};

use anyhow::Result;
use bear_rs::SqliteStore;
use sha2::{Digest, Sha256};

use crate::anki::{AnkiClient, AnkiNote};
use crate::config::Config;
use crate::parser::{parse_cards, BearNote, Card, CardKind};
use crate::render::{referenced_images, render_for_anki, NoteImage};
use crate::state::SyncState;

const ANKI_BATCH_SIZE: usize = 50;

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
    pub progress: bool,
    pub config: &'a Config,
}

struct PendingAdd {
    note_id: String,
    fp: String,
    hash: String,
    note: AnkiNote,
}

struct PendingUpdate {
    note_id: String,
    fp: String,
    hash: String,
    anki_id: u64,
    note: AnkiNote,
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
    if opts.progress {
        println!("Scanning Bear notes...");
    }

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

    if opts.progress {
        println!(
            "Found {} note(s), {} card(s) in sync scope.",
            notes.len(),
            current_order.len()
        );
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
    let mut pending_adds = Vec::new();
    let mut pending_updates = Vec::new();
    let mut created_decks = HashSet::new();

    let total_cards = current_order.len();
    for (index, (note_id, fp)) in current_order.iter().enumerate() {
        let card = current
            .get(&(note_id.clone(), fp.clone()))
            .expect("ordered current card key should exist");
        let bear_tags = note_tags
            .get(note_id.as_str())
            .map_or(&[][..], Vec::as_slice);
        let tags = build_tags(card, bear_tags, opts.config);
        let image_files = fetch_images_cached(&mut note_image_cache, store, note_id)?;
        let media_refs = card_referenced_images(card, image_files);
        let context = card_context_label(card, opts.config);
        let hash = content_hash(card, &tags, &media_refs, context.as_deref());

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

            if opts.progress {
                println!(
                    "[prepare update] {}/{} {} — {}",
                    index + 1,
                    total_cards,
                    card.deck,
                    card_preview(card)
                );
            }
            let anki_note = build_anki_note(
                card,
                image_files,
                tags,
                client,
                opts.config,
                &mut upload_cache,
            )?;
            if opts.verbose && !opts.progress {
                println!("[update] {} — {}", card.deck, card_preview(card));
            }
            pending_updates.push(PendingUpdate {
                note_id: note_id.clone(),
                fp: fp.clone(),
                hash,
                anki_id,
                note: anki_note,
            });
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

            if opts.progress {
                println!(
                    "[prepare add]    {}/{} {} — {}",
                    index + 1,
                    total_cards,
                    card.deck,
                    card_preview(card)
                );
            }
            let anki_note = build_anki_note(
                card,
                image_files,
                tags,
                client,
                opts.config,
                &mut upload_cache,
            )?;
            if opts.verbose && !opts.progress {
                println!("[add]    {} — {}", card.deck, card_preview(card));
            }
            pending_adds.push(PendingAdd {
                note_id: note_id.clone(),
                fp: fp.clone(),
                hash,
                note: anki_note,
            });
        }
    }

    apply_update_batches(
        client,
        state,
        &mut report,
        &mut created_decks,
        pending_updates,
        opts.progress,
    )?;
    apply_add_batches(
        client,
        state,
        &mut report,
        &mut created_decks,
        pending_adds,
        opts.progress,
        true,
    )?;

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
            if opts.progress {
                println!("[delete] {} stale Anki note(s)", ids.len());
            }
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

fn apply_add_batches(
    client: &AnkiClient,
    state: &mut SyncState,
    report: &mut SyncReport,
    created_decks: &mut HashSet<String>,
    pending: Vec<PendingAdd>,
    progress: bool,
    count_as_added: bool,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }

    ensure_decks(
        client,
        created_decks,
        pending.iter().map(|item| item.note.deck.as_str()),
    )?;
    if progress {
        println!(
            "Adding {} Anki note(s) in batches of up to {}...",
            pending.len(),
            ANKI_BATCH_SIZE
        );
    }

    let total = pending.len();
    let mut done = 0usize;
    for chunk in pending.chunks(ANKI_BATCH_SIZE) {
        let notes: Vec<&AnkiNote> = chunk.iter().map(|item| &item.note).collect();
        let ids = client.add_notes(&notes)?;
        for (item, anki_id) in chunk.iter().zip(ids) {
            state.insert(&item.note_id, &item.fp, anki_id);
            state.set_hash(&item.note_id, &item.fp, item.hash.clone());
        }
        state.save()?;
        done += chunk.len();
        if count_as_added {
            report.added += chunk.len();
        }
        if progress {
            println!("[add]    {done}/{total} Anki note(s) added");
        }
    }
    Ok(())
}

fn apply_update_batches(
    client: &AnkiClient,
    state: &mut SyncState,
    report: &mut SyncReport,
    created_decks: &mut HashSet<String>,
    pending: Vec<PendingUpdate>,
    progress: bool,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }

    ensure_decks(
        client,
        created_decks,
        pending.iter().map(|item| item.note.deck.as_str()),
    )?;
    if progress {
        println!(
            "Updating {} Anki note(s) in batches of up to {}...",
            pending.len(),
            ANKI_BATCH_SIZE
        );
    }

    let total = pending.len();
    let mut done = 0usize;
    for chunk in pending.chunks(ANKI_BATCH_SIZE) {
        let updates: Vec<(u64, &AnkiNote)> = chunk
            .iter()
            .map(|item| (item.anki_id, &item.note))
            .collect();
        let outcomes = client.update_notes(&updates)?;

        let mut successful = Vec::new();
        let mut readds = Vec::new();
        for (item, outcome) in chunk.iter().zip(outcomes) {
            match outcome {
                None => successful.push(item),
                Some(err) if is_note_not_found_message(&err) => {
                    eprintln!(
                        "bear-anki: note {} not found in Anki, re-adding",
                        item.anki_id
                    );
                    readds.push(PendingAdd {
                        note_id: item.note_id.clone(),
                        fp: item.fp.clone(),
                        hash: item.hash.clone(),
                        note: item.note.clone(),
                    });
                }
                Some(err) => return Err(anyhow::anyhow!("AnkiConnect update failed: {err}")),
            }
        }

        let moves: Vec<(u64, &str)> = successful
            .iter()
            .map(|item| (item.anki_id, item.note.deck.as_str()))
            .collect();
        client.move_notes_to_decks(&moves)?;
        for item in successful {
            state.set_hash(&item.note_id, &item.fp, item.hash.clone());
        }
        if !readds.is_empty() {
            apply_add_batches(client, state, report, created_decks, readds, false, false)?;
        }
        state.save()?;
        done += chunk.len();
        report.updated += chunk.len();
        if progress {
            println!("[update] {done}/{total} Anki note(s) updated");
        }
    }
    Ok(())
}

fn ensure_decks<'a>(
    client: &AnkiClient,
    created_decks: &mut HashSet<String>,
    decks: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    for deck in decks {
        if created_decks.insert(deck.to_owned()) {
            client.create_deck(deck)?;
        }
    }
    Ok(())
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
    config: &Config,
    upload_cache: &mut HashMap<String, String>,
) -> Result<AnkiNote> {
    let note = match &card.kind {
        CardKind::Basic { front, back } => AnkiNote {
            deck: card.deck.clone(),
            model: "Basic".into(),
            fields: [
                (
                    "Front".to_owned(),
                    decorate_prompt_html(
                        render_for_anki(front, image_files, client, upload_cache)?,
                        card,
                        config,
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
                decorate_prompt_html(
                    render_for_anki(text, image_files, client, upload_cache)?,
                    card,
                    config,
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

fn decorate_prompt_html(html: String, card: &Card, config: &Config) -> String {
    let html = with_context_pill(html, card_context_label(card, config).as_deref());
    with_sort_key_prefix(html, &card.sort_key)
}

fn card_context_label(card: &Card, config: &Config) -> Option<String> {
    if !config.include_card_context || card.context.is_empty() {
        return None;
    }
    Some(card.context.join(&config.card_context_separator))
}

fn with_context_pill(html: String, context: Option<&str>) -> String {
    let Some(context) = context else {
        return html;
    };

    let mut out = String::with_capacity(html.len() + context.len() + 220);
    out.push_str("<div class=\"bear-anki-context\" style=\"display:inline-block;margin:0 0 0.65em 0;padding:0.18em 0.55em;border-radius:999px;background:#eef0f3;color:#5f6670;font-size:0.78em;font-weight:600;line-height:1.45;\">");
    out.push_str(&escape_html_text(context));
    out.push_str("</div>\n");
    out.push_str(&html);
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

fn is_note_not_found_message(message: &str) -> bool {
    let msg = message.to_lowercase();
    msg.contains("not found") || msg.contains("no note with id")
}

fn content_hash(
    card: &Card,
    tags: &[String],
    images: &[&NoteImage],
    context: Option<&str>,
) -> String {
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
    h.update(b"\0context\0");
    if let Some(context) = context {
        h.update(context.as_bytes());
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
    use super::{
        build_tags, card_context_label, card_referenced_images, content_hash, decorate_prompt_html,
        with_sort_key_prefix,
    };
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
            context: vec![],
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
            content_hash(&make_card("000001"), &tags, &[], None),
            content_hash(&make_card("000002"), &tags, &[], None)
        );
    }

    #[test]
    fn content_hash_changes_when_tags_change() {
        let card = make_card("000001");
        assert_ne!(
            content_hash(&card, &["bear-card".to_owned()], &[], None),
            content_hash(
                &card,
                &["bear-card".to_owned(), "topic".to_owned()],
                &[],
                None
            )
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
            content_hash(&card, &tags, &refs_a, None),
            content_hash(&card, &tags, &refs_b, None)
        );
    }

    #[test]
    fn context_label_uses_configured_separator() {
        let mut card = make_card("000001");
        card.context = vec!["Subtitle".into(), "Topic".into()];
        let config = Config::default();

        assert_eq!(
            card_context_label(&card, &config).as_deref(),
            Some("Subtitle / Topic")
        );
    }

    #[test]
    fn context_can_be_disabled() {
        let mut card = make_card("000001");
        card.context = vec!["Subtitle".into()];
        let config = Config {
            include_card_context: false,
            ..Config::default()
        };

        assert_eq!(card_context_label(&card, &config), None);
    }

    #[test]
    fn decorate_prompt_html_renders_hidden_order_then_visible_context() {
        let mut card = make_card("000001");
        card.context = vec!["Subtitle".into()];
        let html = decorate_prompt_html("<p>Front</p>\n".to_owned(), &card, &Config::default());

        assert!(html.starts_with("<span class=\"bear-anki-order\" style=\"display:none\">"));
        assert!(html.contains("class=\"bear-anki-context\""));
        assert!(html.contains(">Subtitle</div>\n<p>Front</p>"));
    }

    #[test]
    fn content_hash_changes_when_context_changes() {
        let tags = vec!["bear-card".to_owned()];
        let card = make_card("000001");

        assert_ne!(
            content_hash(&card, &tags, &[], Some("Subtitle")),
            content_hash(&card, &tags, &[], Some("Other Subtitle"))
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
