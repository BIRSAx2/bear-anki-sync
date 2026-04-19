use std::thread;
use std::time::{Duration, Instant};

use bear_anki_sync::anki::AnkiClient;
use bear_anki_sync::config::Config;
use bear_anki_sync::state::SyncState;
use bear_anki_sync::sync::{self, SyncOptions, SyncReport};
use bear_cli::config::resolve_database_path;
use bear_cli::db::BearDb;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

#[cfg(target_os = "macos")]
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

// ── Custom event from background sync thread ──────────────────────────────────

#[derive(Debug)]
enum AppEvent {
    SyncDone(Result<SyncReport, String>),
}

// ── Application state ─────────────────────────────────────────────────────────

struct MenuBarApp {
    _tray: Option<TrayIcon>,

    sync_item: MenuItem,
    force_item: MenuItem,
    last_sync_item: MenuItem, // informational, always disabled
    card_count_item: MenuItem, // informational, always disabled
    quit_item: MenuItem,

    proxy: EventLoopProxy<AppEvent>,
    db_path_override: Option<String>,
    anki_url: String,
    cfg: Config,

    is_syncing: bool,
    card_count: usize,
    last_sync_at: Option<Instant>,
}

impl MenuBarApp {
    fn new(
        proxy: EventLoopProxy<AppEvent>,
        db_path_override: Option<String>,
        anki_url: String,
        cfg: Config,
    ) -> Self {
        let card_count = SyncState::load()
            .map(|s| s.all_keys().count())
            .unwrap_or(0);

        let sync_item = MenuItem::new("Sync Now", true, None);
        let force_item = MenuItem::new("Force Sync (re-sync all)", true, None);
        let last_sync_item = MenuItem::new("Last sync: never", false, None);
        let card_count_item = MenuItem::new(card_count_label(card_count), false, None);
        let quit_item = MenuItem::new("Quit Bear-Anki", true, None);

        Self {
            _tray: None,
            sync_item,
            force_item,
            last_sync_item,
            card_count_item,
            quit_item,
            proxy,
            db_path_override,
            anki_url,
            cfg,
            is_syncing: false,
            card_count,
            last_sync_at: None,
        }
    }

    fn build_tray(&self) -> TrayIcon {
        let menu = Menu::new();
        menu.append(&self.sync_item).unwrap();
        menu.append(&self.force_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&self.last_sync_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&self.card_count_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&self.quit_item).unwrap();

        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Bear-Anki Sync")
            .with_icon(card_icon())
            .build()
            .expect("failed to create tray icon")
    }

    fn start_sync(&mut self, force: bool) {
        if self.is_syncing {
            return;
        }
        self.is_syncing = true;
        self.sync_item.set_text("Syncing…");
        self.sync_item.set_enabled(false);
        self.force_item.set_enabled(false);
        self.last_sync_item.set_text("Syncing…");

        let proxy = self.proxy.clone();
        let db_path_override = self.db_path_override.clone();
        let anki_url = self.anki_url.clone();
        let cfg = self.cfg.clone();

        thread::spawn(move || {
            let result = do_sync(db_path_override, &anki_url, &cfg, force);
            let _ = proxy.send_event(AppEvent::SyncDone(result));
        });
    }

    fn on_sync_done(&mut self, result: Result<SyncReport, String>) {
        self.is_syncing = false;
        self.sync_item.set_text("Sync Now");
        self.sync_item.set_enabled(true);
        self.force_item.set_enabled(true);
        self.last_sync_at = Some(Instant::now());

        // Reload card count from the updated state file.
        self.card_count = SyncState::load()
            .map(|s| s.all_keys().count())
            .unwrap_or(self.card_count);
        self.card_count_item
            .set_text(card_count_label(self.card_count));

        match result {
            Ok(ref r) => {
                let detail = format!(
                    "+{} ~{} −{} ={} (added · updated · deleted · unchanged)",
                    r.added, r.updated, r.deleted, r.skipped
                );
                self.last_sync_item
                    .set_text(format!("Last sync: {detail}"));
                notify("Bear-Anki Sync", &detail);
            }
            Err(ref msg) => {
                self.last_sync_item
                    .set_text(format!("Last sync failed: {msg}"));
                notify("Bear-Anki Sync Failed", msg);
            }
        }
    }
}

// ── winit ApplicationHandler ──────────────────────────────────────────────────

impl ApplicationHandler<AppEvent> for MenuBarApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self._tray.is_none() {
            self._tray = Some(self.build_tray());
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::SyncDone(result) => self.on_sync_done(result),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Poll);

        while TrayIconEvent::receiver().try_recv().is_ok() {}

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *self.sync_item.id() {
                self.start_sync(false);
            } else if event.id == *self.force_item.id() {
                self.start_sync(true);
            } else if event.id == *self.quit_item.id() {
                event_loop.exit();
            }
        }

        thread::sleep(Duration::from_millis(20));
    }
}

// ── Sync (background thread) ──────────────────────────────────────────────────

fn do_sync(
    db_path_override: Option<String>,
    anki_url: &str,
    cfg: &Config,
    force: bool,
) -> Result<SyncReport, String> {
    let db_path = match db_path_override {
        Some(p) => std::path::PathBuf::from(p),
        None => resolve_database_path(None).map_err(|e| e.to_string())?,
    };

    let media_dir = db_path
        .parent()
        .ok_or_else(|| "invalid Bear database path".to_owned())?
        .join("Local Files/Note Images");

    let db = BearDb::open(db_path).map_err(|e| e.to_string())?;
    let mut state = SyncState::load().map_err(|e| e.to_string())?;
    let client = AnkiClient::new(anki_url);

    client.check_connection().map_err(|e| e.to_string())?;

    sync::sync(
        &db,
        &client,
        &mut state,
        &media_dir,
        &SyncOptions {
            tag_filter: None,
            note_filter: None,
            dry_run: false,
            force,
            verbose: false,
            config: cfg,
        },
    )
    .map_err(|e| e.to_string())
}

// ── Notifications (via osascript — works without a bundle ID) ─────────────────

fn notify(title: &str, body: &str) {
    let title = title.replace('\\', "\\\\").replace('"', "\\\"");
    let body = body.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(r#"display notification "{body}" with title "{title}""#);
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn();
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn card_count_label(n: usize) -> String {
    format!("{n} card(s) tracked")
}

// ── Tray icon ─────────────────────────────────────────────────────────────────

/// 22×22 "card" icon for the macOS menu bar.
/// Black pixels on transparent background — renders correctly in both
/// light and dark menu bar appearances.
fn card_icon() -> tray_icon::Icon {
    const S: u32 = 22;
    let mut rgba = vec![0u8; (S * S * 4) as usize];

    let mut px = |x: u32, y: u32| {
        if x < S && y < S {
            let i = ((y * S + x) * 4) as usize;
            rgba[i] = 0;
            rgba[i + 1] = 0;
            rgba[i + 2] = 0;
            rgba[i + 3] = 230;
        }
    };

    // Card border (4 px inset)
    for n in 4..=17u32 {
        px(n, 4);
        px(n, 17);
        px(4, n);
        px(17, n);
    }
    // Two horizontal "text lines"
    for n in 7..=15u32 {
        px(n, 9);
        px(n, 13);
    }

    tray_icon::Icon::from_rgba(rgba, S, S).expect("icon creation failed")
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let cfg = Config::load().unwrap_or_default();
    let anki_url = cfg
        .anki_url
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:8765".to_owned());
    let db_path_override = cfg.bear_database.clone();

    let mut builder = EventLoop::<AppEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    builder.with_activation_policy(ActivationPolicy::Accessory);

    let event_loop = builder.build().expect("failed to create event loop");
    let proxy = event_loop.create_proxy();

    let mut app = MenuBarApp::new(proxy, db_path_override, anki_url, cfg);
    event_loop.run_app(&mut app).expect("event loop error");
}
