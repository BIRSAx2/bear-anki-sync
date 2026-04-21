use std::thread;
use std::time::{Duration, Instant, SystemTime};

use bear_anki_sync::anki::AnkiClient;
use bear_anki_sync::auth_error::is_auth_error_message;
use bear_anki_sync::config::Config;
use bear_anki_sync::state::SyncState;
use bear_anki_sync::sync::{self, SyncOptions, SyncReport};
use muda::{IconMenuItem, Menu, MenuEvent, NativeIcon, PredefinedMenuItem};
use tiny_skia::{Pixmap, Transform};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use usvg::{Options, Tree};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

#[cfg(target_os = "macos")]
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

#[derive(Debug)]
enum AppEvent {
    SyncDone(Result<SyncReport, String>),
    AuthChecked(Result<(), String>),
    AuthDone(Result<(), String>),
}

struct MenuBarApp {
    tray: Option<TrayIcon>,

    sync_item: IconMenuItem,
    force_item: IconMenuItem,
    auth_item: IconMenuItem,
    sync_status_item: IconMenuItem,
    auth_status_item: IconMenuItem,
    last_sync_item: IconMenuItem,
    changes_item: IconMenuItem,
    card_count_item: IconMenuItem,
    quit_item: IconMenuItem,

    proxy: EventLoopProxy<AppEvent>,
    anki_url: String,
    cfg: Config,

    is_syncing: bool,
    is_authenticating: bool,
    is_authenticated: bool,
    card_count: usize,
    last_sync_at: Option<SystemTime>,
    last_sync_failed_message: Option<String>,
    next_auto_sync_at: Option<Instant>,
}

impl MenuBarApp {
    fn new(proxy: EventLoopProxy<AppEvent>, anki_url: String, cfg: Config) -> Self {
        let card_count = SyncState::load().map(|s| s.all_keys().count()).unwrap_or(0);
        let next_auto_sync_at = next_auto_sync_deadline(&cfg);

        Self {
            tray: None,
            sync_item: IconMenuItem::with_native_icon(
                "Sync Now",
                true,
                Some(NativeIcon::Refresh),
                None,
            ),
            force_item: IconMenuItem::with_native_icon(
                "Force Re-sync",
                true,
                Some(NativeIcon::RefreshFreestanding),
                None,
            ),
            auth_item: IconMenuItem::with_native_icon(
                "Check Bear Authentication",
                true,
                Some(NativeIcon::User),
                None,
            ),
            sync_status_item: IconMenuItem::with_native_icon(
                "Sync: idle",
                false,
                Some(NativeIcon::StatusNone),
                None,
            ),
            auth_status_item: IconMenuItem::with_native_icon(
                "Bear auth: checking...",
                false,
                Some(NativeIcon::StatusPartiallyAvailable),
                None,
            ),
            last_sync_item: IconMenuItem::with_native_icon(
                "Last sync: never",
                false,
                Some(NativeIcon::StatusNone),
                None,
            ),
            changes_item: IconMenuItem::with_native_icon(
                "Changes: not synced yet",
                false,
                Some(NativeIcon::ListView),
                None,
            ),
            card_count_item: IconMenuItem::with_native_icon(
                card_count_label(card_count),
                false,
                Some(NativeIcon::MultipleDocuments),
                None,
            ),
            quit_item: IconMenuItem::with_native_icon(
                "Quit Bear-Anki",
                true,
                Some(NativeIcon::Remove),
                None,
            ),
            proxy,
            anki_url,
            cfg,
            is_syncing: false,
            is_authenticating: false,
            is_authenticated: false,
            card_count,
            last_sync_at: None,
            last_sync_failed_message: None,
            next_auto_sync_at,
        }
    }

    fn build_tray(&self) -> TrayIcon {
        let menu = Menu::new();
        menu.append(&self.sync_item).unwrap();
        menu.append(&self.force_item).unwrap();
        menu.append(&self.auth_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&self.sync_status_item).unwrap();
        menu.append(&self.auth_status_item).unwrap();
        menu.append(&self.last_sync_item).unwrap();
        menu.append(&self.changes_item).unwrap();
        menu.append(&self.card_count_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&self.quit_item).unwrap();

        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Bear-Anki Sync")
            .with_icon(card_icon())
            .with_icon_as_template(true)
            .with_menu_on_left_click(true)
            .build()
            .expect("failed to create tray icon")
    }

    fn refresh_auth_status(&mut self) {
        if self.is_authenticating {
            return;
        }
        self.auth_status_item.set_text("Bear auth: checking...");
        self.auth_status_item
            .set_native_icon(Some(NativeIcon::StatusPartiallyAvailable));
        self.auth_item.set_text("Check Bear Authentication");
        self.auth_item.set_enabled(false);

        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let result = sync::check_auth().map_err(|e| e.to_string());
            let _ = proxy.send_event(AppEvent::AuthChecked(result));
        });
    }

    fn start_authenticate(&mut self) {
        if self.is_authenticating || self.is_syncing {
            return;
        }
        self.is_authenticating = true;
        self.auth_item.set_text("Authenticating...");
        self.auth_item.set_enabled(false);
        self.auth_status_item
            .set_text("Bear auth: sign-in in progress");
        self.auth_status_item
            .set_native_icon(Some(NativeIcon::RefreshFreestanding));
        self.sync_item.set_enabled(false);
        self.force_item.set_enabled(false);

        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let result = sync::authenticate().map_err(|e| e.to_string());
            let _ = proxy.send_event(AppEvent::AuthDone(result));
        });
    }

    fn on_auth_checked(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.is_authenticated = true;
                self.auth_status_item.set_text("Bear auth: connected");
                self.auth_status_item
                    .set_native_icon(Some(NativeIcon::StatusAvailable));
                self.auth_item.set_text("Re-authenticate Bear");
                self.auth_item.set_enabled(true);
            }
            Err(msg) => {
                self.is_authenticated = false;
                self.auth_status_item
                    .set_text(format!("Bear auth: {}", concise_auth_error(&msg)));
                self.auth_status_item
                    .set_native_icon(Some(NativeIcon::StatusUnavailable));
                self.auth_item.set_text("Authenticate Bear");
                self.auth_item.set_enabled(true);
            }
        }
        self.refresh_action_state();
    }

    fn on_auth_done(&mut self, result: Result<(), String>) {
        self.is_authenticating = false;
        match result {
            Ok(()) => {
                self.is_authenticated = true;
                self.auth_status_item.set_text("Bear auth: connected");
                self.auth_status_item
                    .set_native_icon(Some(NativeIcon::StatusAvailable));
                self.auth_item.set_text("Re-authenticate Bear");
            }
            Err(msg) => {
                self.is_authenticated = false;
                self.auth_status_item
                    .set_text(format!("Bear auth: {}", concise_auth_error(&msg)));
                self.auth_status_item
                    .set_native_icon(Some(NativeIcon::Caution));
                self.auth_item.set_text("Authenticate Bear");
            }
        }
        self.auth_item.set_enabled(true);
        self.refresh_action_state();
    }

    fn start_sync(&mut self, force: bool) {
        if self.is_syncing || self.is_authenticating || !self.is_authenticated {
            return;
        }
        self.is_syncing = true;
        self.sync_item.set_text("Syncing...");
        self.sync_status_item.set_text(if force {
            "Sync: force re-sync in progress"
        } else {
            "Sync: in progress"
        });
        self.sync_status_item
            .set_native_icon(Some(NativeIcon::RefreshFreestanding));
        self.last_sync_item.set_text("Last sync: in progress");
        self.last_sync_item
            .set_native_icon(Some(NativeIcon::StatusPartiallyAvailable));
        self.changes_item.set_text("Changes: waiting for result");
        self.changes_item
            .set_native_icon(Some(NativeIcon::ListView));
        self.refresh_action_state();
        if !force {
            self.next_auto_sync_at = next_auto_sync_deadline(&self.cfg);
        }

        let proxy = self.proxy.clone();
        let anki_url = self.anki_url.clone();
        let cfg = self.cfg.clone();

        thread::spawn(move || {
            let result = do_sync(&anki_url, &cfg, force);
            let _ = proxy.send_event(AppEvent::SyncDone(result));
        });
    }

    fn on_sync_done(&mut self, result: Result<SyncReport, String>) {
        self.is_syncing = false;
        self.last_sync_at = Some(SystemTime::now());

        self.card_count = SyncState::load()
            .map(|s| s.all_keys().count())
            .unwrap_or(self.card_count);
        self.card_count_item
            .set_text(card_count_label(self.card_count));

        match result {
            Ok(ref report) => {
                self.last_sync_failed_message = None;
                self.sync_status_item.set_text("Sync: healthy");
                self.sync_status_item
                    .set_native_icon(Some(NativeIcon::StatusAvailable));
                self.last_sync_item.set_text(format!(
                    "Last sync: {}",
                    relative_time_label(self.last_sync_at)
                ));
                self.last_sync_item
                    .set_native_icon(Some(NativeIcon::StatusAvailable));
                self.changes_item
                    .set_text(format!("Changes: {}", sync_changes_label(report)));
                self.changes_item
                    .set_native_icon(Some(sync_changes_icon(report)));
            }
            Err(ref msg) => {
                self.last_sync_failed_message = Some(truncate(msg, 72));
                self.sync_status_item.set_text("Sync: failed");
                self.sync_status_item
                    .set_native_icon(Some(NativeIcon::StatusUnavailable));
                self.last_sync_item
                    .set_text(format!("Last sync failed: {}", truncate(msg, 72)));
                self.last_sync_item
                    .set_native_icon(Some(NativeIcon::Caution));
                self.changes_item.set_text("Changes: unavailable");
                self.changes_item.set_native_icon(Some(NativeIcon::Caution));
                if is_auth_error_message(msg) {
                    self.is_authenticated = false;
                    self.auth_status_item.set_text("Bear auth: expired");
                    self.auth_status_item
                        .set_native_icon(Some(NativeIcon::StatusUnavailable));
                    self.auth_item.set_text("Authenticate Bear");
                    self.auth_item.set_enabled(true);
                }
            }
        }

        self.sync_item.set_text("Sync Now");
        self.refresh_action_state();
        self.refresh_time_dependent_labels();
        self.next_auto_sync_at = next_auto_sync_deadline(&self.cfg);
    }

    fn refresh_action_state(&self) {
        let can_sync = self.is_authenticated && !self.is_syncing && !self.is_authenticating;
        self.sync_item.set_enabled(can_sync);
        self.force_item.set_enabled(can_sync);
        self.auth_item
            .set_enabled(!self.is_syncing && !self.is_authenticating);
    }

    fn refresh_time_dependent_labels(&self) {
        if self.is_syncing {
            return;
        }

        if self.last_sync_failed_message.is_none() && self.last_sync_at.is_some() {
            self.last_sync_item.set_text(format!(
                "Last sync: {}",
                relative_time_label(self.last_sync_at)
            ));
        }
    }

    fn maybe_start_auto_sync(&mut self) {
        let Some(next_auto_sync_at) = self.next_auto_sync_at else {
            return;
        };

        if Instant::now() < next_auto_sync_at {
            return;
        }

        if self.is_authenticated && !self.is_syncing && !self.is_authenticating {
            self.start_sync(false);
        } else {
            self.next_auto_sync_at = next_auto_sync_deadline(&self.cfg);
        }
    }
}

impl ApplicationHandler<AppEvent> for MenuBarApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray.is_none() {
            self.tray = Some(self.build_tray());
            self.refresh_auth_status();
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::SyncDone(result) => self.on_sync_done(result),
            AppEvent::AuthChecked(result) => self.on_auth_checked(result),
            AppEvent::AuthDone(result) => self.on_auth_done(result),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.refresh_time_dependent_labels();
        self.maybe_start_auto_sync();
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_secs(1),
        ));

        while TrayIconEvent::receiver().try_recv().is_ok() {}

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *self.sync_item.id() {
                self.start_sync(false);
            } else if event.id == *self.force_item.id() {
                self.start_sync(true);
            } else if event.id == *self.auth_item.id() {
                self.start_authenticate();
            } else if event.id == *self.quit_item.id() {
                event_loop.exit();
            }
        }
    }
}

fn do_sync(anki_url: &str, cfg: &Config, force: bool) -> Result<SyncReport, String> {
    let ck = sync::load_client().map_err(|e| e.to_string())?;
    let mut state = SyncState::load().map_err(|e| e.to_string())?;
    let client = AnkiClient::new(anki_url);

    client.check_connection().map_err(|e| e.to_string())?;

    sync::sync(
        &ck,
        &client,
        &mut state,
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

fn card_count_label(n: usize) -> String {
    format!("Tracked cards: {n}")
}

fn relative_time_label(at: Option<SystemTime>) -> String {
    let Some(at) = at else {
        return "never".to_owned();
    };
    let Ok(elapsed) = SystemTime::now().duration_since(at) else {
        return "just now".to_owned();
    };

    if elapsed.as_secs() < 5 {
        "just now".to_owned()
    } else if elapsed.as_secs() < 60 {
        format!("{}s ago", elapsed.as_secs())
    } else if elapsed.as_secs() < 3600 {
        format!("{}m ago", elapsed.as_secs() / 60)
    } else {
        format!("{}h ago", elapsed.as_secs() / 3600)
    }
}

fn sync_changes_label(report: &SyncReport) -> String {
    let mut parts = Vec::new();
    if report.added > 0 {
        parts.push(format!("{} added", report.added));
    }
    if report.updated > 0 {
        parts.push(format!("{} updated", report.updated));
    }
    if report.deleted > 0 {
        parts.push(format!("{} deleted", report.deleted));
    }
    if parts.is_empty() {
        if report.skipped > 0 {
            return format!("no changes, {} unchanged", report.skipped);
        }
        return "no changes".to_owned();
    }
    if report.skipped > 0 {
        parts.push(format!("{} unchanged", report.skipped));
    }
    parts.join(" | ")
}

fn sync_changes_icon(report: &SyncReport) -> NativeIcon {
    if report.added > 0 || report.updated > 0 || report.deleted > 0 {
        NativeIcon::StatusAvailable
    } else {
        NativeIcon::ListView
    }
}

fn concise_auth_error(message: &str) -> String {
    if is_auth_error_message(message) {
        "not authenticated".to_owned()
    } else {
        truncate(message, 48)
    }
}

fn truncate(input: &str, max_chars: usize) -> String {
    if let Some((idx, _)) = input.char_indices().nth(max_chars) {
        let mut out = input[..idx].to_owned();
        out.push_str("...");
        out
    } else {
        input.to_owned()
    }
}

fn next_auto_sync_deadline(cfg: &Config) -> Option<Instant> {
    let minutes = cfg.sync_interval_minutes?;
    if minutes == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(minutes.saturating_mul(60)))
    }
}

fn card_icon() -> tray_icon::Icon {
    const SIZE: u32 = 22;
    let svg = include_str!("../assets/anki_93962.svg");
    let opt = Options::default();
    let tree = Tree::from_str(svg, &opt).expect("failed to parse tray icon svg");
    let mut pixmap = Pixmap::new(SIZE, SIZE).expect("failed to allocate tray icon pixmap");
    let svg_size = tree.size();
    let sx = SIZE as f32 / svg_size.width();
    let sy = SIZE as f32 / svg_size.height();
    resvg::render(&tree, Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    tray_icon::Icon::from_rgba(pixmap.take(), SIZE, SIZE).expect("icon creation failed")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `bear-anki-app --install [--apps-dir <path>]`
    // Wraps the running binary in a proper .app bundle so it appears in
    // Launchpad and Spotlight. Safe to re-run — removes the old bundle first.
    if args.iter().any(|a| a == "--install") {
        let apps_dir = args
            .windows(2)
            .find(|w| w[0] == "--apps-dir")
            .map(|w| std::path::PathBuf::from(&w[1]))
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
                std::path::PathBuf::from(home).join("Applications")
            });

        if let Err(e) = install_app_bundle(&apps_dir) {
            eprintln!("bear-anki-app: install failed: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    let cfg = Config::load().unwrap_or_default();
    let anki_url = cfg
        .anki_url
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:8765".to_owned());

    let mut builder = EventLoop::<AppEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    builder.with_activation_policy(ActivationPolicy::Accessory);

    let event_loop = builder.build().expect("failed to create event loop");
    let proxy = event_loop.create_proxy();

    let mut app = MenuBarApp::new(proxy, anki_url, cfg);
    event_loop.run_app(&mut app).expect("event loop error");
}

fn install_app_bundle(apps_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::process::Command;

    let exe = std::env::current_exe()?;
    let app_dir = apps_dir.join("BearAnki.app");

    if app_dir.exists() {
        std::fs::remove_dir_all(&app_dir)?;
    }

    let macos_dir = app_dir.join("Contents/MacOS");
    let resources_dir = app_dir.join("Contents/Resources");
    std::fs::create_dir_all(&macos_dir)?;
    std::fs::create_dir_all(&resources_dir)?;

    std::fs::copy(&exe, macos_dir.join("bear-anki-app"))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.birsax2.bear-anki-app</string>
    <key>CFBundleName</key>
    <string>BearAnki</string>
    <key>CFBundleDisplayName</key>
    <string>Bear Anki Sync</string>
    <key>CFBundleExecutable</key>
    <string>bear-anki-app</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>{}</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(app_dir.join("Contents/Info.plist"), plist)?;

    if let Err(e) = generate_icns(&resources_dir) {
        eprintln!("bear-anki-app: icon generation skipped: {e}");
    }

    let lsregister = concat!(
        "/System/Library/Frameworks/CoreServices.framework",
        "/Versions/A/Frameworks/LaunchServices.framework",
        "/Versions/A/Support/lsregister"
    );
    let _ = Command::new(lsregister)
        .args(["-f", app_dir.to_str().unwrap_or("")])
        .status();

    println!("Installed: {}", app_dir.display());
    println!(
        "Launch from Launchpad or Spotlight, or: open '{}'",
        app_dir.display()
    );
    Ok(())
}

fn generate_icns(resources_dir: &std::path::Path) -> anyhow::Result<()> {
    use std::process::Command;

    let svg = include_str!("../assets/icon.svg");
    let opt = Options::default();
    let tree = Tree::from_str(svg, &opt).map_err(|e| anyhow::anyhow!("{e}"))?;

    let iconset = std::env::temp_dir().join("BearAnki.iconset");
    if iconset.exists() {
        std::fs::remove_dir_all(&iconset)?;
    }
    std::fs::create_dir_all(&iconset)?;

    for &size in &[16u32, 32, 64, 128, 256, 512] {
        let mut pixmap =
            Pixmap::new(size, size).ok_or_else(|| anyhow::anyhow!("pixmap alloc failed"))?;
        let svg_size = tree.size();
        let sx = size as f32 / svg_size.width();
        let sy = size as f32 / svg_size.height();
        resvg::render(&tree, Transform::from_scale(sx, sy), &mut pixmap.as_mut());
        pixmap.save_png(iconset.join(format!("icon_{size}x{size}.png")))?;
    }

    // Create @2x entries by copying the higher-resolution variants
    for &(base, double) in &[(16u32, 32u32), (32, 64), (64, 128), (128, 256), (256, 512)] {
        std::fs::copy(
            iconset.join(format!("icon_{double}x{double}.png")),
            iconset.join(format!("icon_{base}x{base}@2x.png")),
        )?;
    }

    let icns_path = resources_dir.join("AppIcon.icns");
    let status = Command::new("iconutil")
        .args(["-c", "icns", "-o"])
        .arg(&icns_path)
        .arg(&iconset)
        .status()?;

    let _ = std::fs::remove_dir_all(&iconset);

    anyhow::ensure!(status.success(), "iconutil exited with {status}");
    Ok(())
}
