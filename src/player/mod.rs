use crate::subsonic::{Client, Track};
use rand::seq::SliceRandom;
use rodio::mixer::Mixer;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub enum RepeatMode {
    #[default]
    None,
    One,
    All,
}

/// Shared playback state, readable from the UI thread.
#[derive(Debug, Clone, Default)]
pub struct PlaybackState {
    pub track: Option<Track>,
    pub is_playing: bool,
    /// 0.0 – 1.0
    pub progress: f32,
    pub queue: Vec<Track>,
    pub queue_index: usize,
    /// 0.0 – 1.0, default 0.7
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: RepeatMode,
}

impl PlaybackState {
    fn initial() -> Self {
        Self { volume: 0.7, ..Default::default() }
    }
}

/// Controls sent from the UI / clickwheel to the player task.
#[derive(Debug)]
pub enum PlayerCommand {
    Play(Track),
    PlayQueue { tracks: Vec<Track>, start_index: usize },
    Pause,
    Resume,
    Next,
    Previous,
    SeekPercent(f32),
    Stop,
    SetVolume(f32),
    ToggleShuffle,
    CycleRepeat,
}

pub struct Player {
    subsonic: Client,
    pub state_tx: watch::Sender<PlaybackState>,
    pub state_rx: watch::Receiver<PlaybackState>,
    state: Arc<Mutex<PlaybackState>>,
}

impl Player {
    pub fn new(subsonic: Client) -> Self {
        let initial = PlaybackState::initial();
        let (state_tx, state_rx) = watch::channel(initial.clone());
        Self {
            subsonic,
            state_tx,
            state_rx,
            state: Arc::new(Mutex::new(initial)),
        }
    }

    pub fn state_receiver(&self) -> watch::Receiver<PlaybackState> {
        self.state_rx.clone()
    }

    /// Run the player command loop on a dedicated tokio task.
    pub async fn run(self, mut cmd_rx: tokio::sync::mpsc::Receiver<PlayerCommand>) {
        info!("Player task started");

        // Open audio device — must stay alive for the duration of run().
        let handle = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(h) => h,
            Err(e) => {
                error!("Failed to open audio output: {e}");
                return;
            }
        };
        let mixer = handle.mixer().clone();

        // Current rodio::Player — replaced each time a new track starts.
        let current_player: Arc<Mutex<Option<rodio::Player>>> = Arc::new(Mutex::new(None));

        let mut progress_tick = tokio::time::interval(Duration::from_millis(500));

        loop {
            tokio::select! {
                biased;

                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    handle_command(
                        cmd,
                        &self.subsonic,
                        &self.state,
                        &self.state_tx,
                        &mixer,
                        &current_player,
                    ).await;
                }

                _ = progress_tick.tick() => {
                    tick_progress(
                        &self.subsonic,
                        &self.state,
                        &self.state_tx,
                        &mixer,
                        &current_player,
                    ).await;
                }
            }
        }
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

async fn handle_command(
    cmd: PlayerCommand,
    subsonic: &Client,
    state: &Arc<Mutex<PlaybackState>>,
    state_tx: &watch::Sender<PlaybackState>,
    mixer: &Mixer,
    current_player: &Arc<Mutex<Option<rodio::Player>>>,
) {
    match cmd {
        PlayerCommand::Play(track) => {
            play_track(subsonic, state, state_tx, mixer, current_player, &track).await;
        }

        PlayerCommand::PlayQueue { tracks, start_index } => {
            let (final_tracks, final_index) = {
                let s = state.lock().unwrap();
                if s.shuffle && !tracks.is_empty() {
                    let mut rng = rand::thread_rng();
                    let mut t = tracks;
                    // Move the chosen track to front, then shuffle the rest.
                    t.swap(0, start_index);
                    t[1..].shuffle(&mut rng);
                    (t, 0)
                } else {
                    (tracks, start_index)
                }
            };
            {
                let mut s = state.lock().unwrap();
                s.queue = final_tracks.clone();
                s.queue_index = final_index;
            }
            play_track(subsonic, state, state_tx, mixer, current_player, &final_tracks[final_index]).await;
        }

        PlayerCommand::Pause => {
            if let Some(p) = current_player.lock().unwrap().as_ref() {
                p.pause();
            }
            let mut s = state.lock().unwrap();
            s.is_playing = false;
            state_tx.send(s.clone()).ok();
        }

        PlayerCommand::Resume => {
            if let Some(p) = current_player.lock().unwrap().as_ref() {
                p.play();
            }
            let mut s = state.lock().unwrap();
            s.is_playing = true;
            state_tx.send(s.clone()).ok();
        }

        PlayerCommand::Next => {
            let next = {
                let mut s = state.lock().unwrap();
                let repeat = s.repeat.clone();
                if s.queue_index + 1 < s.queue.len() {
                    s.queue_index += 1;
                    Some(s.queue[s.queue_index].clone())
                } else if repeat == RepeatMode::All && !s.queue.is_empty() {
                    s.queue_index = 0;
                    Some(s.queue[0].clone())
                } else {
                    None
                }
            };
            if let Some(track) = next {
                play_track(subsonic, state, state_tx, mixer, current_player, &track).await;
            }
        }

        PlayerCommand::Previous => {
            let prev = {
                let mut s = state.lock().unwrap();
                if s.queue_index > 0 {
                    s.queue_index -= 1;
                    Some(s.queue[s.queue_index].clone())
                } else if s.repeat == RepeatMode::All && !s.queue.is_empty() {
                    let last = s.queue.len() - 1;
                    s.queue_index = last;
                    Some(s.queue[last].clone())
                } else {
                    None
                }
            };
            if let Some(track) = prev {
                play_track(subsonic, state, state_tx, mixer, current_player, &track).await;
            }
        }

        PlayerCommand::Stop => {
            *current_player.lock().unwrap() = None;
            let mut s = state.lock().unwrap();
            s.is_playing = false;
            s.track = None;
            s.progress = 0.0;
            state_tx.send(s.clone()).ok();
        }

        PlayerCommand::SeekPercent(_pct) => {
            // rodio doesn't support seeking on HTTP streams yet.
            // Future: symphonia backend + seek support.
            info!("Seek not yet implemented");
        }

        PlayerCommand::SetVolume(v) => {
            let v = v.clamp(0.0, 1.0);
            if let Some(p) = current_player.lock().unwrap().as_ref() {
                p.set_volume(v);
            }
            let mut s = state.lock().unwrap();
            s.volume = v;
            state_tx.send(s.clone()).ok();
        }

        PlayerCommand::ToggleShuffle => {
            let mut s = state.lock().unwrap();
            s.shuffle = !s.shuffle;
            state_tx.send(s.clone()).ok();
        }

        PlayerCommand::CycleRepeat => {
            let mut s = state.lock().unwrap();
            s.repeat = match s.repeat {
                RepeatMode::None => RepeatMode::All,
                RepeatMode::All  => RepeatMode::One,
                RepeatMode::One  => RepeatMode::None,
            };
            state_tx.send(s.clone()).ok();
        }
    }
}

/// Called every 500 ms: update progress bar OR auto-advance when a track ends.
async fn tick_progress(
    subsonic: &Client,
    state: &Arc<Mutex<PlaybackState>>,
    state_tx: &watch::Sender<PlaybackState>,
    mixer: &Mixer,
    current_player: &Arc<Mutex<Option<rodio::Player>>>,
) {
    let (is_playing, duration_opt) = {
        let s = state.lock().unwrap();
        (s.is_playing, s.track.as_ref().and_then(|t| t.duration))
    };

    if !is_playing {
        return;
    }

    let is_empty = {
        let p = current_player.lock().unwrap();
        p.as_ref().map(|p| p.empty()).unwrap_or(true)
    };

    if is_empty {
        // Track finished — advance, repeat, or stop.
        let next = {
            let mut s = state.lock().unwrap();
            let repeat = s.repeat.clone();
            match repeat {
                RepeatMode::One => {
                    // Replay the same track
                    Some(s.queue[s.queue_index].clone())
                }
                RepeatMode::All => {
                    if s.queue_index + 1 < s.queue.len() {
                        s.queue_index += 1;
                    } else {
                        s.queue_index = 0;
                    }
                    s.queue.get(s.queue_index).cloned()
                }
                RepeatMode::None => {
                    if s.queue_index + 1 < s.queue.len() {
                        s.queue_index += 1;
                        Some(s.queue[s.queue_index].clone())
                    } else {
                        // End of queue
                        s.is_playing = false;
                        s.progress = 1.0;
                        state_tx.send(s.clone()).ok();
                        None
                    }
                }
            }
        };
        if let Some(track) = next {
            play_track(subsonic, state, state_tx, mixer, current_player, &track).await;
        }
        return;
    }

    // Update progress from player position.
    if let Some(total) = duration_opt {
        let pos = {
            let p = current_player.lock().unwrap();
            p.as_ref().map(|p| p.get_pos().as_secs_f32()).unwrap_or(0.0)
        };
        let progress = (pos / total as f32).clamp(0.0, 1.0);
        let mut s = state.lock().unwrap();
        s.progress = progress;
        state_tx.send(s.clone()).ok();
    }
}

async fn play_track(
    subsonic: &Client,
    state: &Arc<Mutex<PlaybackState>>,
    state_tx: &watch::Sender<PlaybackState>,
    mixer: &Mixer,
    current_player: &Arc<Mutex<Option<rodio::Player>>>,
    track: &Track,
) {
    info!("Playing: {} — {}", track.artist.as_deref().unwrap_or("?"), track.title);

    // Mark as not-playing and stop current audio before the network fetch
    // so tick_progress doesn't misinterpret the empty player as a finished track.
    let volume = {
        let mut s = state.lock().unwrap();
        s.is_playing = false;
        s.track = Some(track.clone());
        s.progress = 0.0;
        state_tx.send(s.clone()).ok();
        s.volume
    };
    *current_player.lock().unwrap() = None;

    let url = subsonic.stream_url(&track.id);

    match reqwest::get(&url).await {
        Ok(resp) => match resp.bytes().await {
            Ok(bytes) => {
                let cursor = std::io::Cursor::new(bytes.to_vec());
                match rodio::Decoder::new(cursor) {
                    Ok(source) => {
                        let player = rodio::Player::connect_new(mixer);
                        player.set_volume(volume);
                        player.append(source);
                        player.play();
                        *current_player.lock().unwrap() = Some(player);

                        let mut s = state.lock().unwrap();
                        s.is_playing = true;
                        s.progress = 0.0;
                        state_tx.send(s.clone()).ok();
                    }
                    Err(e) => error!("Failed to decode audio: {e}"),
                }
            }
            Err(e) => error!("Failed to read audio bytes: {e}"),
        },
        Err(e) => error!("Failed to fetch stream URL {url}: {e}"),
    }
}
