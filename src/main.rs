use anyhow::Result;
use bear_anki_sync::anki::AnkiClient;
use bear_anki_sync::config::Config;
use bear_anki_sync::parser::parse_cards;
use bear_anki_sync::state::SyncState;
use bear_anki_sync::sync;
use bear_cli::config::resolve_database_path;
use bear_cli::db::BearDb;
use clap::{Args, Parser, Subcommand};

const DEFAULT_ANKI_URL: &str = "http://127.0.0.1:8765";

#[derive(Parser, Debug)]
#[command(
    name = "bear-anki",
    version,
    about = "Sync Bear.app notes to Anki via AnkiConnect"
)]
struct Cli {
    #[arg(long, global = true, env = "BEAR_DATABASE")]
    database: Option<String>,

    #[arg(long, global = true, env = "ANKI_URL")]
    anki_url: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Sync Bear cards to Anki
    Sync(SyncCommand),
    /// List all cards that would be synced (no Anki connection needed)
    List(ListCommand),
    /// Show sync state summary
    Status,
    /// Clear sync state (and optionally delete notes from Anki)
    Reset(ResetCommand),
}

#[derive(Args, Debug)]
struct SyncCommand {
    #[arg(long)]
    tag: Option<String>,

    /// Only sync cards from notes whose title contains this string
    #[arg(long)]
    note: Option<String>,

    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Re-sync all cards even if content hasn't changed
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Print each card action (add/update/skip)
    #[arg(long, short, default_value_t = false)]
    verbose: bool,
}

#[derive(Args, Debug)]
struct ListCommand {
    #[arg(long)]
    tag: Option<String>,

    /// Only list cards from notes whose title contains this string
    #[arg(long)]
    note: Option<String>,
}

#[derive(Args, Debug)]
struct ResetCommand {
    /// Also delete all tracked notes from Anki (default: only clear local state)
    #[arg(long, default_value_t = false)]
    delete_anki_notes: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    // Priority: CLI flag / env var  >  config file  >  built-in default
    let anki_url = cli
        .anki_url
        .or_else(|| cfg.anki_url.clone())
        .unwrap_or_else(|| DEFAULT_ANKI_URL.to_owned());
    let database = cli.database.or_else(|| cfg.bear_database.clone());

    let client = AnkiClient::new(&anki_url);

    match cli.command {
        Commands::Sync(cmd) => {
            let db_path = resolve_database_path(database.as_deref())?;
            let media_dir = db_path.parent().unwrap().join("Local Files/Note Images");
            let db = BearDb::open(db_path)?;
            let mut state = SyncState::load()?;

            client.check_connection()?;

            let report = sync::sync(
                &db,
                &client,
                &mut state,
                &media_dir,
                &sync::SyncOptions {
                    tag_filter: cmd.tag.as_deref(),
                    note_filter: cmd.note.as_deref(),
                    dry_run: cmd.dry_run,
                    force: cmd.force,
                    verbose: cmd.verbose,
                    config: &cfg,
                },
            )?;

            if cmd.dry_run {
                println!(
                    "[dry-run] {} to add, {} to update, {} to delete",
                    report.added, report.updated, report.deleted
                );
            } else {
                println!(
                    "Sync complete: {} added, {} updated, {} deleted, {} unchanged",
                    report.added, report.updated, report.deleted, report.skipped
                );
            }
        }

        Commands::List(cmd) => {
            let db_path = resolve_database_path(database.as_deref())?;
            let db = BearDb::open(db_path)?;
            let mut notes = db.export_notes(cmd.tag.as_deref())?;

            if let Some(ref title) = cmd.note {
                let title_lower = title.to_lowercase();
                notes.retain(|n| n.title.to_lowercase().contains(&title_lower));
            }

            if notes.is_empty() {
                println!("No notes found.");
                return Ok(());
            }

            let mut total_cards = 0usize;
            for note in &notes {
                let cards = parse_cards(note);
                if cards.is_empty() {
                    continue;
                }
                println!("{} ({} card(s))", note.title, cards.len());
                for card in &cards {
                    use bear_anki_sync::parser::CardKind;
                    let (kind, preview) = match &card.kind {
                        CardKind::Basic { front, .. } => ("basic", front.as_str()),
                        CardKind::Cloze { text } => ("cloze", text.as_str()),
                    };
                    let preview: String = preview.chars().take(70).collect();
                    let ellipsis = if preview.len() < 70 { "" } else { "…" };
                    println!("  [{kind}] {preview}{ellipsis}");
                }
                total_cards += cards.len();
            }
            println!("\n{total_cards} card(s) across {} note(s)", notes.len());
        }

        Commands::Status => {
            let state = SyncState::load()?;
            let mut counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut total = 0usize;
            for (note_id, _, _) in state.all_keys() {
                *counts.entry(note_id.to_owned()).or_default() += 1;
                total += 1;
            }

            if total == 0 {
                println!("No cards tracked. Run `bear-anki sync` to get started.");
                return Ok(());
            }

            // Open Bear DB once for title lookups (best-effort)
            let db = database
                .as_deref()
                .map(|p| Ok(std::path::PathBuf::from(p)))
                .unwrap_or_else(|| resolve_database_path(None))
                .and_then(BearDb::open)
                .ok();

            println!("{total} card(s) tracked across {} note(s):", counts.len());
            let mut entries: Vec<(String, usize)> = counts.into_iter().collect();
            entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (note_id, count) in &entries {
                let label = db
                    .as_ref()
                    .and_then(|d| d.note_title(note_id).ok().flatten())
                    .unwrap_or_else(|| note_id.clone());
                println!("  {count:>3} card(s)  {label}");
            }
            println!("\nState file: {}", state.path().display());
        }

        Commands::Reset(cmd) => {
            let mut state = SyncState::load()?;
            let total: Vec<_> = state
                .all_keys()
                .map(|(n, f, id)| (n.to_owned(), f.to_owned(), id))
                .collect();

            if total.is_empty() {
                println!("Nothing to reset — state is already empty.");
                return Ok(());
            }

            if cmd.delete_anki_notes {
                client.check_connection()?;
                let ids: Vec<u64> = total.iter().map(|(_, _, id)| *id).collect();
                client.delete_notes(&ids)?;
                println!("Deleted {} note(s) from Anki.", ids.len());
            }

            for (note_id, fp, _) in &total {
                state.remove(note_id, fp);
            }
            state.save()?;
            println!(
                "State cleared ({} card(s) removed).{}",
                total.len(),
                if cmd.delete_anki_notes {
                    ""
                } else {
                    " Anki notes were NOT deleted (pass --delete-anki-notes to remove them too)."
                }
            );
        }
    }

    Ok(())
}
