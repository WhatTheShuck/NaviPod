//! Clickwheel FFI bridge.
//!
//! This module wraps the C clickwheel driver program. There are two approaches
//! depending on how your C program exposes itself:
//!
//! **Option A — Shared library (.so)**
//! If your C program is compiled as a shared library, use the `extern "C"`
//! block below. Run `cargo build` with the library linked via build.rs.
//!
//! **Option B — Subprocess / named pipe**
//! If your C program is a standalone binary that writes events to stdout or a
//! named pipe, use the `spawn_reader` approach — no unsafe FFI needed, and
//! easier to iterate on.
//!
//! This file implements Option B by default (safer, easier to get started),
//! with the Option A stubs commented in for when you're ready to link directly.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// ── Event type ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickwheelEvent {
    /// Clockwise rotation tick (one detent)
    ScrollDown,
    /// Counter-clockwise rotation tick
    ScrollUp,
    /// Centre button press
    Select,
    /// Menu button (top of wheel)
    Menu,
    /// Play/pause button (bottom of wheel)
    PlayPause,
    /// Fast-forward (right of wheel)
    FastForward,
    /// Rewind (left of wheel)
    Rewind,
    /// Long-press of the centre button
    LongSelect,
    /// Long-press of the MENU button — dismisses keyboard modal
    LongMenu,
}

// ── Option B: subprocess reader ───────────────────────────────────────────────
//
// Your C program should print one event name per line to stdout, e.g.:
//   SCROLL_DOWN
//   SELECT
//   MENU
//
// Adjust `clickwheel_binary_path()` to point at your compiled C binary.

pub fn clickwheel_binary_path() -> &'static str {
    // Change this to the path of your compiled C clickwheel binary
    "/usr/local/bin/clickwheel-reader"
}

/// Spawns the clickwheel reader subprocess and returns a channel of events.
/// Call this once at startup and hand the receiver to your UI event loop.
pub async fn spawn_reader() -> Result<mpsc::Receiver<ClickwheelEvent>> {
    let (tx, rx) = mpsc::channel(32);

    let mut child = Command::new(clickwheel_binary_path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn clickwheel binary: {e}\nPath: {}", clickwheel_binary_path()))?;

    let stdout = child.stdout.take().expect("clickwheel stdout not captured");

    tokio::spawn(async move {
        // Keep child alive for the duration
        let _child = child;
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    match parse_event(line.trim()) {
                        Some(event) => {
                            if tx.send(event).await.is_err() {
                                break; // receiver dropped, shut down
                            }
                        }
                        None => {
                            warn!("Unknown clickwheel event: '{}'", line.trim());
                        }
                    }
                }
                Ok(None) => {
                    error!("Clickwheel binary exited unexpectedly");
                    break;
                }
                Err(e) => {
                    error!("Error reading clickwheel output: {e}");
                    break;
                }
            }
        }
    });

    Ok(rx)
}

// ── Option C: dev Unix socket listener ────────────────────────────────────────
//
// When the hardware binary is absent, NaviPod listens on a Unix socket so the
// clickwheel emulator (cargo run --bin clickwheel_emu) can connect and send the
// same line-delimited event strings.

pub const DEV_SOCKET_PATH: &str = "/tmp/navipod-cw.sock";

/// Bind a Unix socket and return a channel of events from any connecting client.
/// Intended for dev mode when the real clickwheel binary is not present.
pub async fn listen_dev_socket() -> Result<mpsc::Receiver<ClickwheelEvent>> {
    // Remove stale socket from a previous run
    let _ = std::fs::remove_file(DEV_SOCKET_PATH);

    let listener = UnixListener::bind(DEV_SOCKET_PATH)
        .map_err(|e| anyhow::anyhow!("Failed to bind dev socket {DEV_SOCKET_PATH}: {e}"))?;

    let (tx, rx) = mpsc::channel(32);

    tokio::spawn(async move {
        info!("Clickwheel dev socket listening on {DEV_SOCKET_PATH}");
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    info!("Clickwheel emulator connected");
                    tokio::spawn(relay_socket_client(stream, tx.clone()));
                }
                Err(e) => {
                    error!("Dev socket accept error: {e}");
                    break;
                }
            }
        }
    });

    Ok(rx)
}

async fn relay_socket_client(stream: UnixStream, tx: mpsc::Sender<ClickwheelEvent>) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_event(line.trim()) {
            Some(event) => {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            None => warn!("Unknown event from emulator: '{}'", line.trim()),
        }
    }
    info!("Clickwheel emulator disconnected");
}

fn parse_event(s: &str) -> Option<ClickwheelEvent> {
    match s {
        "SCROLL_DOWN"  => Some(ClickwheelEvent::ScrollDown),
        "SCROLL_UP"    => Some(ClickwheelEvent::ScrollUp),
        "SELECT"       => Some(ClickwheelEvent::Select),
        "MENU"         => Some(ClickwheelEvent::Menu),
        "PLAY_PAUSE"   => Some(ClickwheelEvent::PlayPause),
        "FAST_FORWARD"  => Some(ClickwheelEvent::FastForward),
        "REWIND"        => Some(ClickwheelEvent::Rewind),
        "LONG_SELECT"   => Some(ClickwheelEvent::LongSelect),
        "LONG_MENU"     => Some(ClickwheelEvent::LongMenu),
        _               => None,
    }
}

// ── Option A: direct FFI (uncomment when ready) ───────────────────────────────
//
// In build.rs, add:
//   println!("cargo:rustc-link-lib=clickwheel");
//   println!("cargo:rustc-link-search=native=/path/to/your/c/lib");
//
// Then expose this from your C header:
//   typedef void (*clickwheel_cb)(int event_code);
//   void clickwheel_init(clickwheel_cb callback);
//   void clickwheel_start();
//   void clickwheel_stop();
//
// #[repr(C)]
// pub enum CClickwheelEvent {
//     ScrollDown = 0,
//     ScrollUp   = 1,
//     Select     = 2,
//     Menu       = 3,
//     PlayPause  = 4,
//     Forward    = 5,
//     Rewind     = 6,
// }
//
// extern "C" {
//     fn clickwheel_init(callback: extern "C" fn(event: CClickwheelEvent));
//     fn clickwheel_start();
//     fn clickwheel_stop();
// }
