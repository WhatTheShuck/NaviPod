use crate::{
    clickwheel::{self, ClickwheelEvent},
    config::Config,
    player::{Player, PlayerCommand, RepeatMode},
    subsonic::{self, Client},
    AppTheme, AppView, AppWindow, LibraryItem,
};
use anyhow::Result;
use slint::{ComponentHandle, Rgb8Pixel, SharedPixelBuffer, VecModel};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const MENU_ITEM_COUNT: i32 = 4;

// ── Library navigation state ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum LibraryLevel {
    Artists,
    Albums,
    Tracks,
}

struct NavState {
    /// Cached artist list so back-navigation doesn't require a re-fetch.
    artists: Vec<subsonic::Artist>,
    /// Cached album list for the currently-selected artist.
    albums: Vec<subsonic::Album>,
    /// Cached track list for the currently-selected album.
    tracks: Vec<subsonic::Track>,
    /// Artist name shown as the Albums header (and on Albums → back nav).
    current_artist_name: String,
    /// Album name shown as the Tracks header.
    current_album_name: String,
    level: LibraryLevel,
}

impl Default for NavState {
    fn default() -> Self {
        Self {
            artists: vec![],
            albums: vec![],
            tracks: vec![],
            current_artist_name: String::new(),
            current_album_name: String::new(),
            level: LibraryLevel::Artists,
        }
    }
}

enum NavCommand {
    MenuSelect(i32),
    LibrarySelect(i32),
    Back,
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
            title: a.name.clone().into(),
            subtitle: format!("{} albums", a.album_count).into(),
        })
        .collect()
}

fn albums_to_items(albums: &[subsonic::Album]) -> Vec<LibraryItem> {
    albums
        .iter()
        .map(|a| LibraryItem {
            title: a.name.clone().into(),
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
            let num = t.track_number.unwrap_or((i + 1) as u32);
            let duration = t.duration.map(|d| format_time(d as f32)).unwrap_or_else(|| "—".into());
            LibraryItem {
                title: format!("{}. {}", num, t.title).into(),
                subtitle: format!("{} • {}", t.artist.as_deref().unwrap_or("?"), duration).into(),
            }
        })
        .collect()
}

// ── Main entry point ──────────────────────────────────────────────────────────

pub async fn run(subsonic: Client, cfg: Config) -> Result<()> {
    // ── Player ────────────────────────────────────────────────────────────────
    let player = Player::new(subsonic.clone());
    let state_rx = player.state_receiver();
    let (cmd_tx, cmd_rx) = mpsc::channel::<PlayerCommand>(16);

    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("player runtime")
            .block_on(player.run(cmd_rx))
    });

    // ── Clickwheel (hardware → socket fallback) ───────────────────────────────
    let cw_rx = match clickwheel::spawn_reader().await {
        Ok(rx) => {
            info!("Clickwheel hardware reader started");
            Some(rx)
        }
        Err(e) => {
            warn!("Clickwheel hardware reader not available: {e}");
            match clickwheel::listen_dev_socket().await {
                Ok(rx) => {
                    info!(
                        "Dev socket ready — run `cargo run --bin clickwheel_emu` to connect"
                    );
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
    let (nav_tx, nav_rx) = mpsc::channel::<NavCommand>(8);
    let library_item_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

    // ── Callbacks ─────────────────────────────────────────────────────────────

    window.on_scroll_up({
        let ww = window.as_weak();
        let cmd_tx = cmd_tx.clone();
        let state_rx = state_rx.clone();
        let lib_count = library_item_count.clone();
        move || {
            let Some(w) = ww.upgrade() else { return };
            match w.get_current_view() {
                AppView::Menu => {
                    let idx = w.get_menu_selected_index();
                    if idx > 0 {
                        w.set_menu_selected_index(idx - 1);
                    }
                }
                AppView::Library => {
                    let idx = w.get_library_selected_index();
                    if idx > 0 {
                        w.set_library_selected_index(idx - 1);
                    }
                }
                AppView::NowPlaying => {
                    // Scroll up = volume up
                    let vol = (state_rx.borrow().volume + 0.05).min(1.0);
                    let _ = cmd_tx.try_send(PlayerCommand::SetVolume(vol));
                }
            }
            let _ = lib_count; // keep alive
        }
    });

    window.on_scroll_down({
        let ww = window.as_weak();
        let cmd_tx = cmd_tx.clone();
        let state_rx = state_rx.clone();
        let lib_count = library_item_count.clone();
        move || {
            let Some(w) = ww.upgrade() else { return };
            match w.get_current_view() {
                AppView::Menu => {
                    let idx = w.get_menu_selected_index();
                    if idx < MENU_ITEM_COUNT - 1 {
                        w.set_menu_selected_index(idx + 1);
                    }
                }
                AppView::Library => {
                    let count = *lib_count.lock().unwrap() as i32;
                    let idx = w.get_library_selected_index();
                    if idx < count - 1 {
                        w.set_library_selected_index(idx + 1);
                    }
                }
                AppView::NowPlaying => {
                    // Scroll down = volume down
                    let vol = (state_rx.borrow().volume - 0.05).max(0.0);
                    let _ = cmd_tx.try_send(PlayerCommand::SetVolume(vol));
                }
            }
        }
    });

    window.on_select({
        let ww = window.as_weak();
        let nav_tx = nav_tx.clone();
        let cmd_tx = cmd_tx.clone();
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
                    // Centre button toggles play/pause
                    if state_rx.borrow().is_playing {
                        let _ = cmd_tx.try_send(PlayerCommand::Pause);
                    } else {
                        let _ = cmd_tx.try_send(PlayerCommand::Resume);
                    }
                }
            }
        }
    });

    // ‹ button: go back one level in Library, or all the way to Menu
    window.on_menu_pressed({
        let ww = window.as_weak();
        let nav_tx = nav_tx.clone();
        move || {
            let Some(w) = ww.upgrade() else { return };
            if w.get_current_view() == AppView::Library {
                let _ = nav_tx.try_send(NavCommand::Back);
            } else {
                w.set_current_view(AppView::Menu);
            }
        }
    });

    window.on_play_pause({
        let cmd_tx = cmd_tx.clone();
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
        move || {
            let _ = cmd_tx.try_send(PlayerCommand::Next);
        }
    });

    window.on_prev_track({
        let cmd_tx = cmd_tx.clone();
        move || {
            let _ = cmd_tx.try_send(PlayerCommand::Previous);
        }
    });

    window.on_toggle_shuffle({
        let cmd_tx = cmd_tx.clone();
        move || {
            let _ = cmd_tx.try_send(PlayerCommand::ToggleShuffle);
        }
    });

    window.on_cycle_repeat({
        let cmd_tx = cmd_tx.clone();
        move || {
            let _ = cmd_tx.try_send(PlayerCommand::CycleRepeat);
        }
    });

    window.on_theme_changed({
        let ww = window.as_weak();
        move |theme| {
            if let Some(w) = ww.upgrade() {
                w.set_current_theme(theme);
            }
        }
    });

    // ── Playback state → UI bridge ────────────────────────────────────────────
    {
        let ww = window.as_weak();
        let subsonic_art = subsonic.clone();
        let mut rx = state_rx;
        tokio::spawn(async move {
            let mut last_cover_id: Option<String> = None;
            loop {
                rx.changed().await.ok();
                let state = rx.borrow().clone();

                // Format time strings from progress + duration
                let duration_secs = state.track.as_ref().and_then(|t| t.duration).unwrap_or(0);
                let elapsed_secs = state.progress * duration_secs as f32;
                let elapsed_str = format_time(elapsed_secs);
                let total_str = format_time(duration_secs as f32);

                // Queue position string, e.g. "3 / 10"
                let queue_pos = if !state.queue.is_empty() {
                    format!("{} / {}", state.queue_index + 1, state.queue.len())
                } else {
                    String::new()
                };

                // Repeat mode label
                let repeat_str: &'static str = match state.repeat {
                    RepeatMode::None => "off",
                    RepeatMode::One  => "one",
                    RepeatMode::All  => "all",
                };

                let volume = state.volume;
                let shuffle = state.shuffle;

                // Detect track change → fetch new album art
                let new_cover = state.track.as_ref().and_then(|t| t.cover_art.clone());
                let cover_changed = new_cover != last_cover_id;
                if cover_changed {
                    last_cover_id = new_cover.clone();
                    if let Some(cover_id) = new_cover {
                        let art_url = subsonic_art.cover_art_url(&cover_id, Some(80));
                        let ww2 = ww.clone();
                        tokio::spawn(async move {
                            fetch_and_set_album_art(art_url, ww2).await;
                        });
                    } else {
                        let ww2 = ww.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(w) = ww2.upgrade() {
                                w.set_album_art(Default::default());
                            }
                        })
                        .ok();
                    }
                }

                // Push all state to the UI
                let ww2 = ww.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(w) = ww2.upgrade() else { return };
                    let title = state
                        .track
                        .as_ref()
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| "Not playing".into());
                    let artist = state
                        .track
                        .as_ref()
                        .and_then(|t| t.artist.clone())
                        .unwrap_or_default();
                    let album = state
                        .track
                        .as_ref()
                        .and_then(|t| t.album.clone())
                        .unwrap_or_default();
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
    ));

    // ── Clickwheel event loop ─────────────────────────────────────────────────
    if let Some(mut rx) = cw_rx {
        let ww = window.as_weak();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let ww2 = ww.clone();
                slint::invoke_from_event_loop(move || {
                    let Some(w) = ww2.upgrade() else { return };
                    match event {
                        ClickwheelEvent::ScrollUp    => w.invoke_scroll_up(),
                        ClickwheelEvent::ScrollDown  => w.invoke_scroll_down(),
                        ClickwheelEvent::Select      => w.invoke_select(),
                        ClickwheelEvent::Menu        => w.invoke_menu_pressed(),
                        ClickwheelEvent::PlayPause   => w.invoke_play_pause(),
                        ClickwheelEvent::FastForward => w.invoke_next_track(),
                        ClickwheelEvent::Rewind      => w.invoke_prev_track(),
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
        let raw = img.into_raw();
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
    ww: slint::Weak<AppWindow>,
    cmd_tx: mpsc::Sender<PlayerCommand>,
    subsonic: Client,
    lib_count: Arc<Mutex<usize>>,
) {
    let mut state = NavState::default();
    while let Some(cmd) = nav_rx.recv().await {
        match cmd {
            NavCommand::MenuSelect(idx) => {
                handle_menu_select(idx, &ww, &cmd_tx, &subsonic, &mut state, &lib_count).await;
            }
            NavCommand::LibrarySelect(idx) => {
                handle_library_select(idx, &ww, &cmd_tx, &subsonic, &mut state, &lib_count).await;
            }
            NavCommand::Back => {
                handle_back(&ww, &mut state, &lib_count);
            }
        }
    }
}

fn update_library_in_place(
    ww: &slint::Weak<AppWindow>,
    items: Vec<LibraryItem>,
    header: String,
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
    })
    .ok();
}

fn set_loading_header(ww: &slint::Weak<AppWindow>, msg: &'static str) {
    let ww = ww.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(w) = ww.upgrade() {
            w.set_library_header(msg.into());
        }
    })
    .ok();
}

// ── Menu handler ──────────────────────────────────────────────────────────────

async fn handle_menu_select(
    idx: i32,
    ww: &slint::Weak<AppWindow>,
    _cmd_tx: &mpsc::Sender<PlayerCommand>,
    subsonic: &Client,
    state: &mut NavState,
    lib_count: &Arc<Mutex<usize>>,
) {
    match idx {
        // Music → show artists
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
                    state.level = LibraryLevel::Artists;
                    update_library_in_place(ww, items, "Artists".into(), lib_count);
                }
                Err(e) => {
                    error!("Failed to load artists: {e}");
                    set_loading_header(ww, "Error loading artists");
                }
            }
        }

        // Now Playing
        1 => {
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() {
                    w.set_current_view(AppView::NowPlaying);
                }
            })
            .ok();
        }

        // Settings — not yet implemented
        2 => info!("Settings not yet implemented"),

        // Theme — cycle LiquidGlass → Material → ClassicIPod → …
        3 => {
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

// ── Library handler ───────────────────────────────────────────────────────────

async fn handle_library_select(
    idx: i32,
    ww: &slint::Weak<AppWindow>,
    cmd_tx: &mpsc::Sender<PlayerCommand>,
    subsonic: &Client,
    state: &mut NavState,
    lib_count: &Arc<Mutex<usize>>,
) {
    let usize_idx = idx as usize;

    match state.level {
        LibraryLevel::Artists => {
            let Some(artist) = state.artists.get(usize_idx) else { return };
            let artist_id = artist.id.clone();
            let artist_name = artist.name.clone();

            set_loading_header(ww, "Loading…");
            info!("Fetching albums for {artist_name}…");

            match subsonic.get_artist_albums(&artist_id).await {
                Ok(albums) => {
                    let items = albums_to_items(&albums);
                    state.albums = albums;
                    state.current_artist_name = artist_name.clone();
                    state.level = LibraryLevel::Albums;
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
            let album_id = album.id.clone();
            let album_name = album.name.clone();

            set_loading_header(ww, "Loading…");
            info!("Fetching tracks for {album_name}…");

            match subsonic.get_album_tracks(&album_id).await {
                Ok(tracks) if !tracks.is_empty() => {
                    let items = tracks_to_items(&tracks);
                    state.tracks = tracks;
                    state.current_album_name = album_name.clone();
                    state.level = LibraryLevel::Tracks;
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

        LibraryLevel::Tracks => {
            let Some(_) = state.tracks.get(usize_idx) else { return };

            let tracks = state.tracks.clone();
            let start_index = usize_idx;

            let _ = cmd_tx
                .send(PlayerCommand::PlayQueue { tracks, start_index })
                .await;

            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() {
                    w.set_current_view(AppView::NowPlaying);
                }
            })
            .ok();
        }
    }
}

// ── Back navigation ───────────────────────────────────────────────────────────

fn handle_back(ww: &slint::Weak<AppWindow>, state: &mut NavState, lib_count: &Arc<Mutex<usize>>) {
    match state.level {
        LibraryLevel::Tracks => {
            // Back to cached albums — no network call needed
            let items = albums_to_items(&state.albums);
            let header = state.current_artist_name.clone();
            state.level = LibraryLevel::Albums;
            update_library_in_place(ww, items, header, lib_count);
        }
        LibraryLevel::Albums => {
            // Back to cached artists
            let items = artists_to_items(&state.artists);
            state.level = LibraryLevel::Artists;
            update_library_in_place(ww, items, "Artists".into(), lib_count);
        }
        LibraryLevel::Artists => {
            let ww2 = ww.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(w) = ww2.upgrade() {
                    w.set_current_view(AppView::Menu);
                }
            })
            .ok();
        }
    }
}
