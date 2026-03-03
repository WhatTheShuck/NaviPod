use crate::{
    clickwheel::{self, ClickwheelEvent},
    config::Config,
    db::Db,
    player::{Player, PlayerCommand, RepeatMode},
    subsonic::{self, Client},
    system::{self, SystemStatus},
    AppTheme, AppView, AppWindow, LibraryItem,
};
use anyhow::Result;
use slint::{ComponentHandle, Rgb8Pixel, SharedPixelBuffer, VecModel};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

const MENU_ITEM_COUNT: i32 = 5;

// ── Library / settings navigation levels ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum LibraryLevel {
    // Music library
    Artists,
    Albums,
    Tracks,
    Starred,
    // Settings
    SettingsRoot,
    WifiList,
    WifiNetworkDetail,
    BtDeviceList,
    BtDeviceDetail,
}

struct NavState {
    artists:             Vec<subsonic::Artist>,
    albums:              Vec<subsonic::Album>,
    tracks:              Vec<subsonic::Track>,
    current_artist_name: String,
    current_album_name:  String,
    level:               LibraryLevel,
    // Settings sub-state
    wifi_networks:       Vec<system::WifiNetwork>,
    wifi_enabled:        bool,
    selected_wifi_idx:   usize,
    bt_devices:          Vec<system::BtDevice>,
    bt_enabled:          bool,
    selected_bt_idx:     usize,
    // Keyboard context — SSID waiting for a password, if any
    keyboard_wifi_ssid:  Option<String>,
}

impl Default for NavState {
    fn default() -> Self {
        Self {
            artists:             vec![],
            albums:              vec![],
            tracks:              vec![],
            current_artist_name: String::new(),
            current_album_name:  String::new(),
            level:               LibraryLevel::Artists,
            wifi_networks:       vec![],
            wifi_enabled:        false,
            selected_wifi_idx:   0,
            bt_devices:          vec![],
            bt_enabled:          false,
            selected_bt_idx:     0,
            keyboard_wifi_ssid:  None,
        }
    }
}

enum NavCommand {
    MenuSelect(i32),
    LibrarySelect(i32),
    Back,
    KeyboardSubmitted(String),
    KeyboardDismissed,
}

// ── Keyboard constants & navigation helpers ───────────────────────────────────

/// Total number of keyboard keys (rows 0–3).
const KB_TOTAL: i32 = 29;

/// Flat key labels in row order.
/// Row 0 (0–9):   Q W E R T Y U I O P
/// Row 1 (10–18): A S D F G H J K L
/// Row 2 (19–25): Z X C V B N M
/// Row 3 (26–28): ⌫  (space)  ✓
const KB_KEYS: &[&str] = &[
    "Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P",
    "A", "S", "D", "F", "G", "H", "J", "K", "L",
    "Z", "X", "C", "V", "B", "N", "M",
    "⌫", " ", "✓",
];

/// Row sizes: [10, 9, 7, 3]
const KB_ROW_SIZES: [i32; 4] = [10, 9, 7, 3];
/// First flat index of each row: [0, 10, 19, 26]
const KB_ROW_STARTS: [i32; 4] = [0, 10, 19, 26];

fn kb_row_of(idx: i32) -> i32 {
    if idx < 10 { 0 } else if idx < 19 { 1 } else if idx < 26 { 2 } else { 3 }
}
fn kb_col_of(idx: i32) -> i32 {
    let r = kb_row_of(idx) as usize;
    idx - KB_ROW_STARTS[r]
}
fn kb_nav_right(idx: i32) -> i32 { (idx + 1).min(KB_TOTAL - 1) }
fn kb_nav_left(idx: i32)  -> i32 { (idx - 1).max(0) }
fn kb_nav_down(idx: i32)  -> i32 {
    let row = kb_row_of(idx);
    if row >= 3 { return idx; }
    let col     = kb_col_of(idx);
    let new_row = (row + 1) as usize;
    KB_ROW_STARTS[new_row] + col.min(KB_ROW_SIZES[new_row] - 1)
}
fn kb_nav_up(idx: i32) -> i32 {
    let row = kb_row_of(idx);
    if row == 0 { return idx; }
    let col     = kb_col_of(idx);
    let new_row = (row - 1) as usize;
    KB_ROW_STARTS[new_row] + col.min(KB_ROW_SIZES[new_row] - 1)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn format_time(total_secs: f32) -> String {
    let s = total_secs.max(0.0) as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

fn artists_to_items(artists: &[subsonic::Artist]) -> Vec<LibraryItem> {
    artists
        .iter()
        .map(|a| LibraryItem {
            title:    a.name.clone().into(),
            subtitle: format!("{} albums", a.album_count).into(),
        })
        .collect()
}

fn albums_to_items(albums: &[subsonic::Album]) -> Vec<LibraryItem> {
    albums
        .iter()
        .map(|a| LibraryItem {
            title:    a.name.clone().into(),
            subtitle: {
                let year = a.year.map(|y| format!("{} • ", y)).unwrap_or_default();
                format!("{}{} songs", year, a.song_count)
            }
            .into(),
        })
        .collect()
}

fn tracks_to_items(tracks: &[subsonic::Track]) -> Vec<LibraryItem> {
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let num      = t.track_number.unwrap_or((i + 1) as u32);
            let duration = t.duration.map(|d| format_time(d as f32)).unwrap_or_else(|| "—".into());
            LibraryItem {
                title:    format!("{}. {}", num, t.title).into(),
                subtitle: format!("{} • {}", t.artist.as_deref().unwrap_or("?"), duration).into(),
            }
        })
        .collect()
}

// ── Settings helpers ──────────────────────────────────────────────────────────

fn wifi_status_subtitle(w: &system::WifiStatus) -> String {
    if !w.enabled {
        return "Off".into();
    }
    w.ssid.clone().unwrap_or_else(|| "Not connected".into())
}

fn bt_status_subtitle(b: &system::BtStatus) -> String {
    if !b.enabled {
        return "Off".into();
    }
    b.connected_device.clone().unwrap_or_else(|| "On".into())
}

fn settings_items(status: &SystemStatus) -> Vec<LibraryItem> {
    vec![
        LibraryItem {
            title:    "WiFi".into(),
            subtitle: wifi_status_subtitle(&status.wifi).into(),
        },
        LibraryItem {
            title:    "Bluetooth".into(),
            subtitle: bt_status_subtitle(&status.bluetooth).into(),
        },
    ]
}

fn build_wifi_items(enabled: bool, networks: &[system::WifiNetwork]) -> Vec<LibraryItem> {
    let mut items = vec![LibraryItem {
        title:    (if enabled { "WiFi: On" } else { "WiFi: Off" }).into(),
        subtitle: "tap to toggle".into(),
    }];
    for net in networks {
        let bars = match net.signal_bars {
            0 => "▁  ",
            1 => "▁▃ ",
            2 => "▁▃▅",
            _ => "▁▃▅",
        };
        items.push(LibraryItem {
            title: format!("{}{}", if net.connected { "✓ " } else { "" }, net.ssid).into(),
            subtitle: format!("{} • {}", bars, if net.secured { "Secured" } else { "Open" }).into(),
        });
    }
    items
}

fn build_bt_items(enabled: bool, devices: &[system::BtDevice]) -> Vec<LibraryItem> {
    let mut items = vec![LibraryItem {
        title:    (if enabled { "Bluetooth: On" } else { "Bluetooth: Off" }).into(),
        subtitle: "tap to toggle".into(),
    }];
    if enabled {
        items.push(LibraryItem {
            title:    "Scan for devices".into(),
            subtitle: "~5 seconds".into(),
        });
        for dev in devices {
            items.push(LibraryItem {
                title:    format!("{}{}", if dev.connected { "✓ " } else { "" }, dev.name).into(),
                subtitle: dev.address.clone().into(),
            });
        }
    }
    items
}

fn build_wifi_network_detail_items(network: &system::WifiNetwork) -> Vec<LibraryItem> {
    let signal_text = match network.signal_bars {
        0 => "▁   Very Weak",
        1 => "▁▃  Weak",
        2 => "▁▃▅ Good",
        _ => "▁▃▅ Excellent",
    };
    let mut items = vec![
        LibraryItem {
            title:    "Security".into(),
            subtitle: (if network.secured { "WPA2-Personal" } else { "Open" }).into(),
        },
        LibraryItem {
            title:    "Signal".into(),
            subtitle: signal_text.into(),
        },
    ];
    if network.connected {
        items.push(LibraryItem {
            title:    "Disconnect".into(),
            subtitle: "Tap to disconnect".into(),
        });
    } else {
        items.push(LibraryItem {
            title:    "Connect".into(),
            subtitle: (if network.secured { "Uses saved credentials" } else { "Open network" }).into(),
        });
        if network.secured {
            items.push(LibraryItem {
                title:    "Enter Password".into(),
                subtitle: "Coming soon".into(),
            });
        }
    }
    items
}

fn build_bt_device_detail_items(device: &system::BtDevice) -> Vec<LibraryItem> {
    let mut items = vec![LibraryItem {
        title:    "Address".into(),
        subtitle: device.address.clone().into(),
    }];
    if device.connected {
        items.push(LibraryItem {
            title:    "Disconnect".into(),
            subtitle: "Tap to disconnect".into(),
        });
    } else {
        items.push(LibraryItem {
            title:    "Connect".into(),
            subtitle: "Tap to connect".into(),
        });
    }
    items
}

fn set_library_empty_text(ww: &slint::Weak<AppWindow>, text: &'static str) {
    let ww = ww.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(w) = ww.upgrade() { w.set_library_empty_text(text.into()); }
    })
    .ok();
}

/// Format system status into the six AppWindow status-bar properties.
fn format_system_status(
    s: &SystemStatus,
) -> (String, bool, String, bool, String, bool) {
    // Battery
    let (bat_text, bat_low) = match &s.battery {
        None => (String::new(), false),
        Some(b) => {
            let text = if b.charging {
                format!("⚡{}%", b.percent)
            } else {
                format!("{}%", b.percent)
            };
            (text, b.percent < 20 && !b.charging)
        }
    };

    // WiFi — "W" when on (accent if connected, dim if not)
    let wifi_text = if s.wifi.enabled { "W".into() } else { String::new() };
    let wifi_on   = s.wifi.ssid.is_some();

    // Bluetooth — "B" when on
    let bt_text = if s.bluetooth.enabled { "B".into() } else { String::new() };
    let bt_conn = s.bluetooth.connected_device.is_some();

    (bat_text, bat_low, wifi_text, wifi_on, bt_text, bt_conn)
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub async fn run(
    subsonic: Client,
    cfg:      Config,
    db:       Db,
    system_tx: watch::Sender<SystemStatus>,
    system_rx: watch::Receiver<SystemStatus>,
) -> Result<()> {
    // ── Player ────────────────────────────────────────────────────────────────
    let player   = Player::new(subsonic.clone(), db.clone());
    let state_rx = player.state_receiver();
    let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>(16);

    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("player runtime")
            .block_on(player.run(cmd_rx))
    });

    // ── Clickwheel ────────────────────────────────────────────────────────────
    let cw_rx = match clickwheel::spawn_reader().await {
        Ok(rx) => {
            info!("Clickwheel hardware reader started");
            Some(rx)
        }
        Err(e) => {
            warn!("Clickwheel hardware reader not available: {e}");
            match clickwheel::listen_dev_socket().await {
                Ok(rx) => {
                    info!("Dev socket ready — run `cargo run --bin clickwheel_emu` to connect");
                    Some(rx)
                }
                Err(e) => {
                    error!("Failed to start dev socket listener: {e}");
                    None
                }
            }
        }
    };

    // ── Slint window ──────────────────────────────────────────────────────────
    let window = AppWindow::new()?;

    let initial_theme = match cfg.ui.theme.as_str() {
        "material"     => AppTheme::Material,
        "classic_ipod" => AppTheme::ClassicIPod,
        _              => AppTheme::LiquidGlass,
    };
    window.set_current_theme(initial_theme);

    // ── Shared state ──────────────────────────────────────────────────────────
    let (nav_tx, nav_rx)       = mpsc::channel::<NavCommand>(8);
    let library_item_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let vol_hide_gen: Arc<AtomicU64>          = Arc::new(AtomicU64::new(0));
    let rt_handle = tokio::runtime::Handle::current();

    // ── Callbacks ─────────────────────────────────────────────────────────────

    window.on_scroll_up({
        let ww          = window.as_weak();
        let cmd_tx      = cmd_tx.clone();
        let state_rx    = state_rx.clone();
        let lib_count   = library_item_count.clone();
        let vol_hide_gen = vol_hide_gen.clone();
        let rt_handle   = rt_handle.clone();
        move || {
            let Some(w) = ww.upgrade() else { return };
            match w.get_current_view() {
                AppView::Menu => {
                    let idx = w.get_menu_selected_index();
                    if idx > 0 { w.set_menu_selected_index(idx - 1); }
                }
                AppView::Library => {
                    let idx = w.get_library_selected_index();
                    if idx > 0 { w.set_library_selected_index(idx - 1); }
                }
                AppView::NowPlaying => {
                    if w.get_playback_options_visible() {
                        let idx = w.get_playback_options_index();
                        if idx > 0 { w.set_playback_options_index(idx - 1); }
                    } else {
                        let vol = (state_rx.borrow().volume + 0.05).min(1.0);
                        let _ = cmd_tx.try_send(PlayerCommand::SetVolume(vol));
                        w.set_volume_visible(true);
                        let generation = vol_hide_gen.fetch_add(1, Ordering::SeqCst) + 1;
                        let ww2        = ww.clone();
                        let gen_check  = vol_hide_gen.clone();
                        rt_handle.spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            if gen_check.load(Ordering::SeqCst) == generation {
                                slint::invoke_from_event_loop(move || {
                                    if let Some(w) = ww2.upgrade() {
                                        w.set_volume_visible(false);
                                    }
                                })
                                .ok();
                            }
                        });
                    }
                }
            }
            let _ = lib_count;
        }
    });

    window.on_scroll_down({
        let ww           = window.as_weak();
        let cmd_tx       = cmd_tx.clone();
        let state_rx     = state_rx.clone();
        let lib_count    = library_item_count.clone();
        let vol_hide_gen = vol_hide_gen.clone();
        let rt_handle    = rt_handle.clone();
        move || {
            let Some(w) = ww.upgrade() else { return };
            match w.get_current_view() {
                AppView::Menu => {
                    let idx = w.get_menu_selected_index();
                    if idx < MENU_ITEM_COUNT - 1 { w.set_menu_selected_index(idx + 1); }
                }
                AppView::Library => {
                    let count = *lib_count.lock().unwrap() as i32;
                    let idx   = w.get_library_selected_index();
                    if idx < count - 1 { w.set_library_selected_index(idx + 1); }
                }
                AppView::NowPlaying => {
                    if w.get_playback_options_visible() {
                        let idx = w.get_playback_options_index();
                        if idx < 3 { w.set_playback_options_index(idx + 1); }
                    } else {
                        let vol = (state_rx.borrow().volume - 0.05).max(0.0);
                        let _ = cmd_tx.try_send(PlayerCommand::SetVolume(vol));
                        w.set_volume_visible(true);
                        let generation = vol_hide_gen.fetch_add(1, Ordering::SeqCst) + 1;
                        let ww2        = ww.clone();
                        let gen_check  = vol_hide_gen.clone();
                        rt_handle.spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            if gen_check.load(Ordering::SeqCst) == generation {
                                slint::invoke_from_event_loop(move || {
                                    if let Some(w) = ww2.upgrade() {
                                        w.set_volume_visible(false);
                                    }
                                })
                                .ok();
                            }
                        });
                    }
                }
            }
        }
    });

    window.on_select({
        let ww       = window.as_weak();
        let nav_tx   = nav_tx.clone();
        let cmd_tx   = cmd_tx.clone();
        let db       = db.clone();
        let state_rx = state_rx.clone();
        move || {
            let Some(w) = ww.upgrade() else { return };
            match w.get_current_view() {
                AppView::Menu => {
                    let _ = nav_tx.try_send(NavCommand::MenuSelect(w.get_menu_selected_index()));
                }
                AppView::Library => {
                    let _ = nav_tx
                        .try_send(NavCommand::LibrarySelect(w.get_library_selected_index()));
                }
                AppView::NowPlaying => {
                    if w.get_playback_options_visible() {
                        match w.get_playback_options_index() {
                            0 => { let _ = cmd_tx.try_send(PlayerCommand::ToggleShuffle); }
                            1 => { let _ = cmd_tx.try_send(PlayerCommand::CycleRepeat); }
                            2 => {
                                let track = state_rx.borrow().track.clone();
                                if let Some(track) = track {
                                    let currently_starred = w.get_track_starred();
                                    if currently_starred {
                                        if let Err(e) = db.unstar_track(&track.id) {
                                            error!("Failed to unstar: {e}");
                                        }
                                        w.set_track_starred(false);
                                    } else {
                                        if let Err(e) = db.star_track(&track) {
                                            error!("Failed to star: {e}");
                                        }
                                        w.set_track_starred(true);
                                    }
                                }
                            }
                            _ => { w.set_playback_options_visible(false); }
                        }
                    } else {
                        w.set_playback_options_visible(true);
                        w.set_playback_options_index(0);
                    }
                }
            }
        }
    });

    window.on_menu_pressed({
        let ww     = window.as_weak();
        let nav_tx = nav_tx.clone();
        move || {
            let Some(w) = ww.upgrade() else { return };
            if w.get_current_view() == AppView::NowPlaying && w.get_playback_options_visible() {
                w.set_playback_options_visible(false);
                return;
            }
            if w.get_current_view() == AppView::Library {
                let _ = nav_tx.try_send(NavCommand::Back);
            } else {
                w.set_current_view(AppView::Menu);
            }
        }
    });

    window.on_play_pause({
        let cmd_tx   = cmd_tx.clone();
        let state_rx = state_rx.clone();
        move || {
            if state_rx.borrow().is_playing {
                let _ = cmd_tx.try_send(PlayerCommand::Pause);
            } else {
                let _ = cmd_tx.try_send(PlayerCommand::Resume);
            }
        }
    });

    window.on_next_track({
        let cmd_tx = cmd_tx.clone();
        move || { let _ = cmd_tx.try_send(PlayerCommand::Next); }
    });

    window.on_prev_track({
        let cmd_tx = cmd_tx.clone();
        move || { let _ = cmd_tx.try_send(PlayerCommand::Previous); }
    });

    window.on_toggle_shuffle({
        let cmd_tx = cmd_tx.clone();
        move || { let _ = cmd_tx.try_send(PlayerCommand::ToggleShuffle); }
    });

    window.on_cycle_repeat({
        let cmd_tx = cmd_tx.clone();
        move || { let _ = cmd_tx.try_send(PlayerCommand::CycleRepeat); }
    });

    window.on_long_select(move || {});

    // ── Keyboard callbacks ────────────────────────────────────────────────────

    window.on_keyboard_key_pressed({
        let ww = window.as_weak();
        let nav_tx = nav_tx.clone();
        move |idx| {
            let Some(w) = ww.upgrade() else { return };
            match idx {
                // ⌫ backspace
                26 => {
                    let mut text = w.get_keyboard_text().to_string();
                    text.pop();
                    w.set_keyboard_text(text.into());
                }
                // ✓ submit
                28 => {
                    let text = w.get_keyboard_text().to_string();
                    w.set_keyboard_visible(false);
                    w.invoke_keyboard_submitted(text.into());
                }
                // letter or space
                _ => {
                    if let Some(key) = KB_KEYS.get(idx as usize) {
                        let mut text = w.get_keyboard_text().to_string();
                        text.push_str(key);
                        w.set_keyboard_text(text.into());
                    }
                }
            }
            let _ = nav_tx; // keep clone alive
        }
    });

    window.on_keyboard_submitted({
        let nav_tx = nav_tx.clone();
        move |text| {
            let _ = nav_tx.try_send(NavCommand::KeyboardSubmitted(text.to_string()));
        }
    });

    window.on_keyboard_dismissed({
        let ww     = window.as_weak();
        let nav_tx = nav_tx.clone();
        move || {
            if let Some(w) = ww.upgrade() { w.set_keyboard_visible(false); }
            let _ = nav_tx.try_send(NavCommand::KeyboardDismissed);
        }
    });

    window.on_theme_changed({
        let ww = window.as_weak();
        move |theme| {
            if let Some(w) = ww.upgrade() { w.set_current_theme(theme); }
        }
    });

    // ── Playback state → UI bridge ────────────────────────────────────────────
    {
        let ww           = window.as_weak();
        let subsonic_art = subsonic.clone();
        let mut rx       = state_rx;
        let db           = db.clone();
        tokio::spawn(async move {
            let mut last_cover_id: Option<String> = None;
            let mut last_track_id: Option<String> = None;
            loop {
                rx.changed().await.ok();
                let state = rx.borrow().clone();

                let duration_secs = state.track.as_ref().and_then(|t| t.duration).unwrap_or(0);
                let elapsed_secs  = state.progress * duration_secs as f32;
                let elapsed_str   = format_time(elapsed_secs);
                let total_str     = format_time(duration_secs as f32);

                let queue_pos = if !state.queue.is_empty() {
                    format!("{} / {}", state.queue_index + 1, state.queue.len())
                } else {
                    String::new()
                };

                let repeat_str: &'static str = match state.repeat {
                    RepeatMode::None => "off",
                    RepeatMode::One  => "one",
                    RepeatMode::All  => "all",
                };

                let volume  = state.volume;
                let shuffle = state.shuffle;

                let new_track_id = state.track.as_ref().map(|t| t.id.clone());
                let track_changed = new_track_id != last_track_id;
                let starred_opt = if track_changed {
                    last_track_id = new_track_id.clone();
                    let starred = new_track_id
                        .as_ref()
                        .map(|id| db.is_starred(id).unwrap_or(false))
                        .unwrap_or(false);
                    Some(starred)
                } else {
                    None
                };

                let new_cover    = state.track.as_ref().and_then(|t| t.cover_art.clone());
                let cover_changed = new_cover != last_cover_id;
                if cover_changed {
                    last_cover_id = new_cover.clone();
                    if let Some(cover_id) = new_cover {
                        let art_url = subsonic_art.cover_art_url(&cover_id, Some(200));
                        let ww2     = ww.clone();
                        tokio::spawn(async move {
                            fetch_and_set_album_art(art_url, ww2).await;
                        });
                    } else {
                        let ww2 = ww.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(w) = ww2.upgrade() { w.set_album_art(Default::default()); }
                        })
                        .ok();
                    }
                }

                let ww2 = ww.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(w) = ww2.upgrade() else { return };
                    let title  = state.track.as_ref().map(|t| t.title.clone()).unwrap_or_else(|| "Not playing".into());
                    let artist = state.track.as_ref().and_then(|t| t.artist.clone()).unwrap_or_default();
                    let album  = state.track.as_ref().and_then(|t| t.album.clone()).unwrap_or_default();
                    w.set_track_title(title.into());
                    w.set_track_artist(artist.into());
                    w.set_track_album(album.into());
                    w.set_is_playing(state.is_playing);
                    w.set_playback_progress(state.progress);
                    w.set_elapsed_time(elapsed_str.into());
                    w.set_total_time(total_str.into());
                    w.set_volume_level(volume);
                    w.set_shuffle_enabled(shuffle);
                    w.set_repeat_mode(repeat_str.into());
                    w.set_queue_position(queue_pos.into());
                    if let Some(starred) = starred_opt {
                        w.set_track_starred(starred);
                    }
                })
                .ok();
            }
        });
    }

    // ── System status → UI bridge ─────────────────────────────────────────────
    {
        let ww         = window.as_weak();
        let mut sys_rx = system_rx.clone();
        tokio::spawn(async move {
            loop {
                sys_rx.changed().await.ok();
                let status = sys_rx.borrow().clone();
                let (bat, bat_low, wifi, wifi_on, bt, bt_conn) = format_system_status(&status);
                let ww2 = ww.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(w) = ww2.upgrade() else { return };
                    w.set_status_battery(bat.into());
                    w.set_status_battery_low(bat_low);
                    w.set_status_wifi(wifi.into());
                    w.set_status_wifi_on(wifi_on);
                    w.set_status_bt(bt.into());
                    w.set_status_bt_connected(bt_conn);
                })
                .ok();
            }
        });
    }

    // ── Nav task ──────────────────────────────────────────────────────────────
    tokio::spawn(nav_task(
        nav_rx,
        window.as_weak(),
        cmd_tx.clone(),
        subsonic,
        library_item_count,
        db,
        system_tx,
        system_rx,
    ));

    // ── Clickwheel ────────────────────────────────────────────────────────────
    if let Some(mut rx) = cw_rx {
        let ww = window.as_weak();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let ww2 = ww.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(w) = ww2.upgrade() else { return };

                    // ── Keyboard intercept ────────────────────────────────────
                    // When the keyboard overlay is visible all events are
                    // consumed here; nothing reaches normal view handlers.
                    if w.get_keyboard_visible() {
                        match event {
                            ClickwheelEvent::ScrollUp => {
                                let idx = (w.get_keyboard_selected_index() - 1).max(0);
                                w.set_keyboard_selected_index(idx);
                            }
                            ClickwheelEvent::ScrollDown => {
                                let idx = (w.get_keyboard_selected_index() + 1)
                                    .min(KB_TOTAL - 1);
                                w.set_keyboard_selected_index(idx);
                            }
                            // D-pad: FF=right, REW=left, MENU=up, PP=down
                            ClickwheelEvent::FastForward => {
                                w.set_keyboard_selected_index(
                                    kb_nav_right(w.get_keyboard_selected_index()),
                                );
                            }
                            ClickwheelEvent::Rewind => {
                                w.set_keyboard_selected_index(
                                    kb_nav_left(w.get_keyboard_selected_index()),
                                );
                            }
                            ClickwheelEvent::Menu => {
                                w.set_keyboard_selected_index(
                                    kb_nav_up(w.get_keyboard_selected_index()),
                                );
                            }
                            ClickwheelEvent::PlayPause => {
                                w.set_keyboard_selected_index(
                                    kb_nav_down(w.get_keyboard_selected_index()),
                                );
                            }
                            ClickwheelEvent::Select => {
                                w.invoke_keyboard_key_pressed(
                                    w.get_keyboard_selected_index(),
                                );
                            }
                            ClickwheelEvent::LongMenu => {
                                // Long-press MENU dismisses keyboard
                                w.set_keyboard_visible(false);
                                w.invoke_keyboard_dismissed();
                            }
                            _ => {}
                        }
                        return; // consume event — don't fall through
                    }

                    // ── Normal routing ────────────────────────────────────────
                    match event {
                        ClickwheelEvent::ScrollUp    => w.invoke_scroll_up(),
                        ClickwheelEvent::ScrollDown  => w.invoke_scroll_down(),
                        ClickwheelEvent::Select      => w.invoke_select(),
                        ClickwheelEvent::LongSelect  => w.invoke_long_select(),
                        ClickwheelEvent::Menu        => w.invoke_menu_pressed(),
                        ClickwheelEvent::PlayPause   => w.invoke_play_pause(),
                        ClickwheelEvent::FastForward => w.invoke_next_track(),
                        ClickwheelEvent::Rewind      => w.invoke_prev_track(),
                        ClickwheelEvent::LongMenu    => {} // no-op outside keyboard
                    }
                })
                .ok();
            }
        });
    }

    window.run()?;
    Ok(())
}

// ── Album art ─────────────────────────────────────────────────────────────────

async fn fetch_and_set_album_art(url: String, ww: slint::Weak<AppWindow>) {
    let result = async {
        let bytes = reqwest::get(&url)
            .await
            .map_err(|e| anyhow::anyhow!("Fetch art: {e}"))?
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("Read art bytes: {e}"))?;

        let img = image::load_from_memory(&bytes)
            .map_err(|e| anyhow::anyhow!("Decode art: {e}"))?
            .to_rgb8();

        let (w, h) = (img.width(), img.height());
        let raw    = img.into_raw();
        anyhow::Ok((raw, w, h))
    }
    .await;

    match result {
        Ok((raw, w, h)) => {
            slint::invoke_from_event_loop(move || {
                let buf = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(&raw, w, h);
                if let Some(win) = ww.upgrade() {
                    win.set_album_art(slint::Image::from_rgb8(buf));
                }
            })
            .ok();
        }
        Err(e) => warn!("Album art failed: {e}"),
    }
}

// ── Navigation task ───────────────────────────────────────────────────────────

async fn nav_task(
    mut nav_rx: mpsc::Receiver<NavCommand>,
    ww:         slint::Weak<AppWindow>,
    cmd_tx:     mpsc::Sender<PlayerCommand>,
    subsonic:   Client,
    lib_count:  Arc<Mutex<usize>>,
    db:         Db,
    system_tx:  watch::Sender<SystemStatus>,
    system_rx:  watch::Receiver<SystemStatus>,
) {
    let mut state = NavState::default();
    while let Some(cmd) = nav_rx.recv().await {
        match cmd {
            NavCommand::MenuSelect(idx) => {
                handle_menu_select(
                    idx, &ww, &cmd_tx, &subsonic, &mut state,
                    &lib_count, &db, &system_tx, &system_rx,
                )
                .await;
            }
            NavCommand::LibrarySelect(idx) => {
                handle_library_select(
                    idx, &ww, &cmd_tx, &subsonic, &mut state,
                    &lib_count, &system_tx, &system_rx,
                )
                .await;
            }
            NavCommand::Back => {
                handle_back(&ww, &mut state, &lib_count, &system_rx);
            }
            NavCommand::KeyboardSubmitted(text) => {
                if let Some(ssid) = state.keyboard_wifi_ssid.take() {
                    info!(
                        "WiFi password entered for '{}': {} chars \
                         (connect-with-password not yet implemented)",
                        ssid,
                        text.len()
                    );
                    // TODO: call system::wifi_connect_with_password(&ssid, &text)
                } else {
                    info!("Keyboard submitted: {text}");
                }
            }
            NavCommand::KeyboardDismissed => {
                state.keyboard_wifi_ssid = None;
            }
        }
    }
}

fn update_library_in_place(
    ww:        &slint::Weak<AppWindow>,
    items:     Vec<LibraryItem>,
    header:    String,
    lib_count: &Arc<Mutex<usize>>,
) {
    *lib_count.lock().unwrap() = items.len();
    let ww = ww.clone();
    slint::invoke_from_event_loop(move || {
        let Some(w) = ww.upgrade() else { return };
        let model = Rc::new(VecModel::from(items));
        w.set_library_items(model.into());
        w.set_library_header(header.into());
        w.set_library_selected_index(0);
        w.set_library_empty_text("No items".into()); // reset any loading message
    })
    .ok();
}

fn set_loading_header(ww: &slint::Weak<AppWindow>, msg: &'static str) {
    let ww = ww.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(w) = ww.upgrade() { w.set_library_header(msg.into()); }
    })
    .ok();
}

/// Push a fresh system status poll result to the watch channel so the status
/// bar updates immediately after the user performs a settings action.
async fn refresh_system_status(system_tx: &watch::Sender<SystemStatus>) {
    system_tx.send(system::poll_status().await).ok();
}

/// Show the WiFi list with just the toggle item and an "Scanning…" header while
/// the network scan is in progress in the background.
fn show_wifi_scanning_state(
    ww:        &slint::Weak<AppWindow>,
    enabled:   bool,
    lib_count: &Arc<Mutex<usize>>,
) {
    let toggle = vec![LibraryItem {
        title:    (if enabled { "WiFi: On" } else { "WiFi: Off" }).into(),
        subtitle: "tap to toggle".into(),
    }];
    *lib_count.lock().unwrap() = 1;
    let ww = ww.clone();
    slint::invoke_from_event_loop(move || {
        let Some(w) = ww.upgrade() else { return };
        let model = Rc::new(VecModel::from(toggle));
        w.set_library_items(model.into());
        w.set_library_header("Scanning for networks…".into());
        w.set_library_selected_index(0);
        w.set_library_empty_text("Please wait…".into());
        w.set_current_view(AppView::Library);
    })
    .ok();
}

/// Show the BT list with just the toggle item and a "Loading…" header while
/// device enumeration is in progress.
fn show_bt_loading_state(
    ww:        &slint::Weak<AppWindow>,
    enabled:   bool,
    lib_count: &Arc<Mutex<usize>>,
) {
    let toggle = vec![LibraryItem {
        title:    (if enabled { "Bluetooth: On" } else { "Bluetooth: Off" }).into(),
        subtitle: "tap to toggle".into(),
    }];
    *lib_count.lock().unwrap() = 1;
    let ww = ww.clone();
    slint::invoke_from_event_loop(move || {
        let Some(w) = ww.upgrade() else { return };
        let model = Rc::new(VecModel::from(toggle));
        w.set_library_items(model.into());
        w.set_library_header("Loading…".into());
        w.set_library_selected_index(0);
        w.set_library_empty_text("Please wait…".into());
        w.set_current_view(AppView::Library);
    })
    .ok();
}

// ── Menu handler ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn handle_menu_select(
    idx:       i32,
    ww:        &slint::Weak<AppWindow>,
    _cmd_tx:   &mpsc::Sender<PlayerCommand>,
    subsonic:  &Client,
    state:     &mut NavState,
    lib_count: &Arc<Mutex<usize>>,
    db:        &Db,
    system_tx: &watch::Sender<SystemStatus>,
    system_rx: &watch::Receiver<SystemStatus>,
) {
    match idx {
        // 0 — Music → Artists
        0 => {
            {
                let ww2 = ww.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(w) = ww2.upgrade() {
                        w.set_library_items(Default::default());
                        w.set_library_header("Loading…".into());
                        w.set_library_selected_index(0);
                        w.set_current_view(AppView::Library);
                    }
                })
                .ok();
            }
            info!("Fetching artists…");
            match subsonic.get_artists().await {
                Ok(artists) => {
                    let items = artists_to_items(&artists);
                    state.artists = artists;
                    state.level   = LibraryLevel::Artists;
                    update_library_in_place(ww, items, "Artists".into(), lib_count);
                }
                Err(e) => {
                    error!("Failed to load artists: {e}");
                    set_loading_header(ww, "Error loading artists");
                }
            }
        }

        // 1 — Starred ★
        1 => {
            {
                let ww2 = ww.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(w) = ww2.upgrade() {
                        w.set_library_items(Default::default());
                        w.set_library_header("Starred ★".into());
                        w.set_library_selected_index(0);
                        w.set_current_view(AppView::Library);
                    }
                })
                .ok();
            }
            match db.get_starred() {
                Ok(tracks) if !tracks.is_empty() => {
                    let items = tracks_to_items(&tracks);
                    state.tracks = tracks;
                    state.level  = LibraryLevel::Starred;
                    update_library_in_place(ww, items, "Starred ★".into(), lib_count);
                }
                Ok(_) => set_loading_header(ww, "No starred tracks yet"),
                Err(e) => {
                    error!("Failed to load starred: {e}");
                    set_loading_header(ww, "Error loading starred");
                }
            }
        }

        // 2 — Now Playing
        2 => {
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() { w.set_current_view(AppView::NowPlaying); }
            })
            .ok();
        }

        // 3 — Settings
        3 => {
            let status = system_rx.borrow().clone();
            let items  = settings_items(&status);
            state.level = LibraryLevel::SettingsRoot;
            *lib_count.lock().unwrap() = items.len();
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() {
                    let model = Rc::new(VecModel::from(items));
                    w.set_library_items(model.into());
                    w.set_library_header("Settings".into());
                    w.set_library_selected_index(0);
                    w.set_current_view(AppView::Library);
                }
            })
            .ok();
            let _ = (system_tx, system_rx); // silence unused warning if Settings is last
        }

        // 4 — Theme cycle
        4 => {
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                let Some(w) = ww2.upgrade() else { return };
                let next = match w.get_current_theme() {
                    AppTheme::LiquidGlass => AppTheme::Material,
                    AppTheme::Material    => AppTheme::ClassicIPod,
                    _                     => AppTheme::LiquidGlass,
                };
                w.set_current_theme(next);
            })
            .ok();
        }

        _ => {}
    }
}

// ── Library / settings handler ────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn handle_library_select(
    idx:       i32,
    ww:        &slint::Weak<AppWindow>,
    cmd_tx:    &mpsc::Sender<PlayerCommand>,
    subsonic:  &Client,
    state:     &mut NavState,
    lib_count: &Arc<Mutex<usize>>,
    system_tx: &watch::Sender<SystemStatus>,
    system_rx: &watch::Receiver<SystemStatus>,
) {
    let usize_idx = idx as usize;

    match state.level {
        // ── Music library ─────────────────────────────────────────────────────

        LibraryLevel::Artists => {
            let Some(artist) = state.artists.get(usize_idx) else { return };
            let artist_id   = artist.id.clone();
            let artist_name = artist.name.clone();
            set_loading_header(ww, "Loading…");
            info!("Fetching albums for {artist_name}…");
            match subsonic.get_artist_albums(&artist_id).await {
                Ok(albums) => {
                    let items = albums_to_items(&albums);
                    state.albums             = albums;
                    state.current_artist_name = artist_name.clone();
                    state.level              = LibraryLevel::Albums;
                    update_library_in_place(ww, items, artist_name, lib_count);
                }
                Err(e) => {
                    error!("Failed to load albums for {artist_id}: {e}");
                    set_loading_header(ww, "Error loading albums");
                }
            }
        }

        LibraryLevel::Albums => {
            let Some(album) = state.albums.get(usize_idx) else { return };
            let album_id   = album.id.clone();
            let album_name = album.name.clone();
            set_loading_header(ww, "Loading…");
            info!("Fetching tracks for {album_name}…");
            match subsonic.get_album_tracks(&album_id).await {
                Ok(tracks) if !tracks.is_empty() => {
                    let items = tracks_to_items(&tracks);
                    state.tracks            = tracks;
                    state.current_album_name = album_name.clone();
                    state.level             = LibraryLevel::Tracks;
                    update_library_in_place(ww, items, album_name, lib_count);
                }
                Ok(_) => {
                    warn!("Album {album_name} has no tracks");
                    set_loading_header(ww, "No tracks found");
                }
                Err(e) => {
                    error!("Failed to load tracks for {album_id}: {e}");
                    set_loading_header(ww, "Error loading tracks");
                }
            }
        }

        LibraryLevel::Tracks | LibraryLevel::Starred => {
            if state.tracks.get(usize_idx).is_none() {
                return;
            }
            let tracks      = state.tracks.clone();
            let start_index = usize_idx;
            let _ = cmd_tx.send(PlayerCommand::PlayQueue { tracks, start_index }).await;
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() { w.set_current_view(AppView::NowPlaying); }
            })
            .ok();
        }

        // ── Settings root ─────────────────────────────────────────────────────

        LibraryLevel::SettingsRoot => {
            match usize_idx {
                0 => {
                    // WiFi submenu — show toggle item immediately, scan if already enabled
                    let cur = system_rx.borrow().clone();
                    state.wifi_enabled = cur.wifi.enabled;
                    state.wifi_networks.clear();
                    state.level = LibraryLevel::WifiList;
                    // Show toggle item + scanning indicator right away
                    show_wifi_scanning_state(ww, state.wifi_enabled, lib_count);
                    if state.wifi_enabled {
                        state.wifi_networks = system::scan_wifi().await;
                    }
                    let items = build_wifi_items(state.wifi_enabled, &state.wifi_networks);
                    update_library_in_place(ww, items, "WiFi".into(), lib_count);
                }
                1 => {
                    // Bluetooth submenu — show toggle + loading, then fetch devices
                    let cur = system_rx.borrow().clone();
                    state.bt_enabled = cur.bluetooth.enabled;
                    state.bt_devices = vec![];
                    state.level = LibraryLevel::BtDeviceList;
                    show_bt_loading_state(ww, state.bt_enabled, lib_count);
                    if state.bt_enabled {
                        state.bt_devices = system::list_bt_devices().await;
                    }
                    let items = build_bt_items(state.bt_enabled, &state.bt_devices);
                    update_library_in_place(ww, items, "Bluetooth".into(), lib_count);
                }
                _ => {}
            }
        }

        // ── WiFi list ─────────────────────────────────────────────────────────

        LibraryLevel::WifiList => {
            match usize_idx {
                // Toggle WiFi on/off
                0 => {
                    let enable = !state.wifi_enabled;
                    if let Err(e) = system::wifi_toggle(enable).await {
                        error!("WiFi toggle failed: {e}");
                    }
                    state.wifi_enabled = enable;
                    state.wifi_networks.clear();
                    if enable {
                        // Show "Enabling…" state while adapter comes up, then scan
                        show_wifi_scanning_state(ww, true, lib_count);
                        set_loading_header(ww, "Enabling WiFi…");
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        set_loading_header(ww, "Scanning…");
                        state.wifi_networks = system::scan_wifi().await;
                    }
                    let items = build_wifi_items(state.wifi_enabled, &state.wifi_networks);
                    update_library_in_place(ww, items, "WiFi".into(), lib_count);
                    refresh_system_status(system_tx).await;
                }
                // Select a network → navigate to detail view
                i if state.wifi_enabled && i >= 1 => {
                    let net_idx = (i - 1) as usize;
                    if let Some(network) = state.wifi_networks.get(net_idx) {
                        state.selected_wifi_idx = net_idx;
                        let items  = build_wifi_network_detail_items(network);
                        let header = network.ssid.clone();
                        state.level = LibraryLevel::WifiNetworkDetail;
                        update_library_in_place(ww, items, header, lib_count);
                    }
                }
                _ => {}
            }
        }

        // ── WiFi network detail ───────────────────────────────────────────────

        LibraryLevel::WifiNetworkDetail => {
            let Some(network) = state.wifi_networks.get(state.selected_wifi_idx).cloned() else { return };
            match usize_idx {
                0 | 1 => {} // Security / Signal — informational, no action
                2 => {
                    if network.connected {
                        set_loading_header(ww, "Disconnecting…");
                        match system::wifi_disconnect().await {
                            Ok(_) => {
                                state.wifi_networks = system::scan_wifi().await;
                                refresh_system_status(system_tx).await;
                                // Refresh detail view with updated state
                                if let Some(updated) = state.wifi_networks.get(state.selected_wifi_idx) {
                                    let items  = build_wifi_network_detail_items(updated);
                                    let header = updated.ssid.clone();
                                    update_library_in_place(ww, items, header, lib_count);
                                } else {
                                    // Network gone — fall back to list
                                    let items = build_wifi_items(state.wifi_enabled, &state.wifi_networks);
                                    state.level = LibraryLevel::WifiList;
                                    update_library_in_place(ww, items, "WiFi".into(), lib_count);
                                }
                            }
                            Err(e) => {
                                error!("WiFi disconnect failed: {e}");
                                set_loading_header(ww, "Disconnect failed");
                            }
                        }
                    } else {
                        set_loading_header(ww, "Connecting…");
                        match system::wifi_connect(&network.ssid).await {
                            Ok(_) => {
                                state.wifi_networks = system::scan_wifi().await;
                                refresh_system_status(system_tx).await;
                                if let Some(updated) = state.wifi_networks.get(state.selected_wifi_idx) {
                                    let items  = build_wifi_network_detail_items(updated);
                                    let header = updated.ssid.clone();
                                    update_library_in_place(ww, items, header, lib_count);
                                } else {
                                    let items = build_wifi_items(true, &state.wifi_networks);
                                    state.level = LibraryLevel::WifiList;
                                    update_library_in_place(ww, items, "WiFi".into(), lib_count);
                                }
                            }
                            Err(e) => {
                                error!("WiFi connect failed: {e}");
                                set_loading_header(ww, "Connection failed");
                            }
                        }
                    }
                }
                3 if network.secured && !network.connected => {
                    let ssid = network.ssid.clone();
                    state.keyboard_wifi_ssid = Some(ssid.clone());
                    let prompt = format!("WiFi Password: {ssid}");
                    let ww2 = ww.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(w) = ww2.upgrade() {
                            w.set_keyboard_text("".into());
                            w.set_keyboard_selected_index(0);
                            w.set_keyboard_prompt(prompt.into());
                            w.set_keyboard_visible(true);
                        }
                    })
                    .ok();
                }
                _ => {}
            }
        }

        // ── Bluetooth device list ─────────────────────────────────────────────

        LibraryLevel::BtDeviceList => {
            match usize_idx {
                // Toggle Bluetooth on/off
                0 => {
                    let enable = !state.bt_enabled;
                    if let Err(e) = system::bt_toggle(enable).await {
                        error!("BT toggle failed: {e}");
                    }
                    state.bt_enabled = enable;
                    state.bt_devices = vec![];
                    show_bt_loading_state(ww, enable, lib_count);
                    if enable {
                        state.bt_devices = system::list_bt_devices().await;
                    }
                    let items = build_bt_items(state.bt_enabled, &state.bt_devices);
                    update_library_in_place(ww, items, "Bluetooth".into(), lib_count);
                    refresh_system_status(system_tx).await;
                }
                // Scan (only reachable when BT is on — items[1] exists only then)
                1 if state.bt_enabled => {
                    set_loading_header(ww, "Scanning… (~5s)");
                    set_library_empty_text(ww, "Scanning for devices…");
                    state.bt_devices = system::bt_scan().await;
                    let items = build_bt_items(true, &state.bt_devices);
                    update_library_in_place(ww, items, "Bluetooth".into(), lib_count);
                }
                // Select a device → navigate to detail view
                i if state.bt_enabled && i >= 2 => {
                    let dev_idx = i - 2;
                    if let Some(dev) = state.bt_devices.get(dev_idx) {
                        state.selected_bt_idx = dev_idx;
                        let items  = build_bt_device_detail_items(dev);
                        let header = dev.name.clone();
                        state.level = LibraryLevel::BtDeviceDetail;
                        update_library_in_place(ww, items, header, lib_count);
                    }
                }
                _ => {}
            }
        }

        // ── Bluetooth device detail ───────────────────────────────────────────

        LibraryLevel::BtDeviceDetail => {
            let Some(dev) = state.bt_devices.get(state.selected_bt_idx).cloned() else { return };
            match usize_idx {
                0 => {} // Address — informational
                1 => {
                    if dev.connected {
                        set_loading_header(ww, "Disconnecting…");
                        match system::bt_disconnect(&dev.address).await {
                            Ok(_) => {
                                state.bt_devices = system::list_bt_devices().await;
                                refresh_system_status(system_tx).await;
                                if let Some(updated) = state.bt_devices.get(state.selected_bt_idx) {
                                    let items  = build_bt_device_detail_items(updated);
                                    let header = updated.name.clone();
                                    update_library_in_place(ww, items, header, lib_count);
                                }
                            }
                            Err(e) => {
                                error!("BT disconnect failed: {e}");
                                set_loading_header(ww, "Disconnect failed");
                            }
                        }
                    } else {
                        set_loading_header(ww, "Connecting…");
                        match system::bt_connect(&dev.address).await {
                            Ok(_) => {
                                state.bt_devices = system::list_bt_devices().await;
                                refresh_system_status(system_tx).await;
                                if let Some(updated) = state.bt_devices.get(state.selected_bt_idx) {
                                    let items  = build_bt_device_detail_items(updated);
                                    let header = updated.name.clone();
                                    update_library_in_place(ww, items, header, lib_count);
                                }
                            }
                            Err(e) => {
                                error!("BT connect failed: {e}");
                                set_loading_header(ww, "Connection failed");
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// ── Back navigation ───────────────────────────────────────────────────────────

fn handle_back(
    ww:        &slint::Weak<AppWindow>,
    state:     &mut NavState,
    lib_count: &Arc<Mutex<usize>>,
    system_rx: &watch::Receiver<SystemStatus>,
) {
    match state.level {
        LibraryLevel::Tracks => {
            let items  = albums_to_items(&state.albums);
            let header = state.current_artist_name.clone();
            state.level = LibraryLevel::Albums;
            update_library_in_place(ww, items, header, lib_count);
        }
        LibraryLevel::Albums => {
            let items = artists_to_items(&state.artists);
            state.level = LibraryLevel::Artists;
            update_library_in_place(ww, items, "Artists".into(), lib_count);
        }
        LibraryLevel::Artists | LibraryLevel::Starred | LibraryLevel::SettingsRoot => {
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() { w.set_current_view(AppView::Menu); }
            })
            .ok();
        }
        LibraryLevel::WifiNetworkDetail => {
            // Back to WiFi list (no rescan — use cached networks).
            let items   = build_wifi_items(state.wifi_enabled, &state.wifi_networks);
            let sel_idx = (state.selected_wifi_idx + 1) as i32; // +1 for toggle row
            state.level = LibraryLevel::WifiList;
            update_library_in_place(ww, items, "WiFi".into(), lib_count);
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() { w.set_library_selected_index(sel_idx); }
            })
            .ok();
        }
        LibraryLevel::BtDeviceDetail => {
            // Back to BT device list (no rescan — use cached devices).
            let items   = build_bt_items(state.bt_enabled, &state.bt_devices);
            let sel_idx = (state.selected_bt_idx + 2) as i32; // +2 for toggle + scan rows
            state.level = LibraryLevel::BtDeviceList;
            update_library_in_place(ww, items, "Bluetooth".into(), lib_count);
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() { w.set_library_selected_index(sel_idx); }
            })
            .ok();
        }
        LibraryLevel::WifiList | LibraryLevel::BtDeviceList => {
            // Back to Settings root — show fresh status subtitles.
            let status = system_rx.borrow().clone();
            let items  = settings_items(&status);
            state.level = LibraryLevel::SettingsRoot;
            update_library_in_place(ww, items, "Settings".into(), lib_count);
        }
    }
}
