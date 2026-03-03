//! NaviPod Clickwheel Emulator
//!
//! A standalone Slint window that renders an iPod Classic clickwheel and sends
//! events to NaviPod over a Unix socket (`/tmp/navipod-cw.sock`).
//!
//! Usage:
//!   1. Start NaviPod:          cargo run
//!   2. Start the emulator:     cargo run --bin clickwheel_emu
//!
//! The emulator reconnects automatically if NaviPod is restarted.

use std::time::Duration;
use tokio::io::AsyncWriteExt;

const SOCKET_PATH: &str = "/tmp/navipod-cw.sock";

// ── Slint UI definition ───────────────────────────────────────────────────────

slint::slint! {
    export component ClickwheelEmu inherits Window {
        title: "NaviPod — Clickwheel";
        width: 300px;
        height: 370px;
        background: #1c1c1e;
        no-frame: false;

        in-out property <bool> connected: false;
        in-out property <string> last-event: "—";

        // Fires when any button/scroll action occurs; Rust wires this to the socket
        callback send-event(string);

        // ── Title ─────────────────────────────────────────────────────────────
        Text {
            x: 0; y: 10px; width: 300px;
            text: "NaviPod";
            color: #ffffff;
            font-size: 15px;
            font-weight: 700;
            horizontal-alignment: center;
        }
        Text {
            x: 0; y: 28px; width: 300px;
            text: "clickwheel emulator";
            color: #666666;
            font-size: 10px;
            horizontal-alignment: center;
        }

        // ── Outer bezel ring ──────────────────────────────────────────────────
        Rectangle {
            x: 24px; y: 54px; width: 252px; height: 252px;
            border-radius: 126px;
            background: #6e6e73;
        }

        // ── Clickwheel disc ───────────────────────────────────────────────────
        Rectangle {
            x: 30px; y: 60px; width: 240px; height: 240px;
            border-radius: 120px;
            background: #aeaeb2;
        }

        // Lighter inner face of the disc
        Rectangle {
            x: 34px; y: 64px; width: 232px; height: 232px;
            border-radius: 116px;
            background: #c7c7cc;
        }

        // ── Center button ─────────────────────────────────────────────────────
        Rectangle {
            x: 93px; y: 123px; width: 114px; height: 114px;
            border-radius: 57px;
            background: #e5e5ea;
            border-width: 1px;
            border-color: #aeaeb2;
        }
        // Subtle highlight on top half of center button
        Rectangle {
            x: 93px; y: 123px; width: 114px; height: 57px;
            border-radius: 57px;
            background: rgba(255, 255, 255, 0.3);
        }

        // ── Button labels ─────────────────────────────────────────────────────
        // MENU — top
        Text {
            x: 90px; y: 80px; width: 120px;
            text: "MENU";
            color: #3a3a3c;
            font-size: 11px;
            font-weight: 700;
            horizontal-alignment: center;
        }
        // ▶⏸ — bottom
        Text {
            x: 90px; y: 260px; width: 120px;
            text: "▶⏸";
            color: #3a3a3c;
            font-size: 13px;
            horizontal-alignment: center;
        }
        // ⏮ — left
        Text {
            x: 30px; y: 172px; width: 63px;
            text: "⏮";
            color: #3a3a3c;
            font-size: 13px;
            horizontal-alignment: center;
        }
        // ⏭ — right
        Text {
            x: 207px; y: 172px; width: 63px;
            text: "⏭";
            color: #3a3a3c;
            font-size: 13px;
            horizontal-alignment: center;
        }

        // ── Touch areas (z-ordered: scroll ring → cardinals → center) ─────────

        // Outer ring — scroll wheel
        TouchArea {
            x: 30px; y: 60px; width: 240px; height: 240px;
            scroll-event(e) => {
                if e.delta-y < 0 {
                    root.last-event = "SCROLL_UP";
                    root.send-event("SCROLL_UP");
                } else {
                    root.last-event = "SCROLL_DOWN";
                    root.send-event("SCROLL_DOWN");
                }
                EventResult.accept
            }
        }

        // MENU — top sector (left-click = MENU, right-click = LONG_MENU)
        TouchArea {
            x: 90px; y: 60px; width: 120px; height: 63px;
            clicked => {
                root.last-event = "MENU";
                root.send-event("MENU");
            }
            pointer-event(e) => {
                if e.button == PointerEventButton.right && e.kind == PointerEventKind.up {
                    root.last-event = "LONG_MENU";
                    root.send-event("LONG_MENU");
                }
            }
        }

        // PLAY_PAUSE — bottom sector
        TouchArea {
            x: 90px; y: 237px; width: 120px; height: 63px;
            clicked => {
                root.last-event = "PLAY_PAUSE";
                root.send-event("PLAY_PAUSE");
            }
        }

        // REWIND — left sector
        TouchArea {
            x: 30px; y: 123px; width: 63px; height: 114px;
            clicked => {
                root.last-event = "REWIND";
                root.send-event("REWIND");
            }
        }

        // FAST_FORWARD — right sector
        TouchArea {
            x: 207px; y: 123px; width: 63px; height: 114px;
            clicked => {
                root.last-event = "FAST_FORWARD";
                root.send-event("FAST_FORWARD");
            }
        }

        // SELECT — center button (topmost)
        // Left-click = SELECT, Right-click = LONG_SELECT (shuffle/repeat modal)
        TouchArea {
            x: 93px; y: 123px; width: 114px; height: 114px;
            clicked => {
                root.last-event = "SELECT";
                root.send-event("SELECT");
            }
            pointer-event(e) => {
                if e.button == PointerEventButton.right && e.kind == PointerEventKind.up {
                    root.last-event = "LONG_SELECT";
                    root.send-event("LONG_SELECT");
                }
            }
        }

        // ── Status bar ────────────────────────────────────────────────────────
        Rectangle {
            x: 0; y: 318px; width: 300px; height: 52px;
            background: #111111;

            // Status dot
            Rectangle {
                x: 20px; y: 18px; width: 8px; height: 8px;
                border-radius: 4px;
                background: connected ? #30d158 : #ff9f0a;
            }

            Text {
                x: 34px; y: 14px; width: 200px;
                text: connected ? "Connected to NaviPod" : "Waiting for NaviPod…";
                color: connected ? #30d158 : #ff9f0a;
                font-size: 11px;
            }

            Text {
                x: 20px; y: 32px; width: 260px;
                text: "Last: " + last-event;
                color: #48484a;
                font-size: 10px;
            }
        }

        // Scroll hint
        Text {
            x: 0; y: 306px; width: 300px;
            text: "↕ mouse wheel scrolls";
            color: #48484a;
            font-size: 9px;
            horizontal-alignment: center;
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let window = ClickwheelEmu::new()?;

    // Channel: Slint callbacks → socket relay task
    let (event_tx, event_rx) =
        tokio::sync::mpsc::unbounded_channel::<String>();

    // Background thread: tokio runtime for async socket I/O
    let window_weak = window.as_weak();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(socket_relay(event_rx, window_weak));
    });

    // Wire the Slint callback → channel sender
    window.on_send_event({
        let tx = event_tx.clone();
        move |event| {
            let _ = tx.send(event.to_string());
        }
    });

    window.run()?;
    Ok(())
}

// ── Socket relay task ─────────────────────────────────────────────────────────

async fn socket_relay(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    window_weak: slint::Weak<ClickwheelEmu>,
) {
    loop {
        set_connected(&window_weak, false);

        match tokio::net::UnixStream::connect(SOCKET_PATH).await {
            Ok(mut stream) => {
                set_connected(&window_weak, true);

                // Relay events until the socket breaks or the channel closes
                loop {
                    match rx.recv().await {
                        Some(event) => {
                            let line = format!("{}\n", event);
                            if stream.write_all(line.as_bytes()).await.is_err() {
                                break; // NaviPod disconnected — reconnect
                            }
                        }
                        None => return, // emulator window closed
                    }
                }
            }
            Err(_) => {
                // NaviPod not running yet — retry after a short delay
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
    }
}

fn set_connected(window_weak: &slint::Weak<ClickwheelEmu>, connected: bool) {
    let ww = window_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(w) = ww.upgrade() {
            w.set_connected(connected);
        }
    })
    .ok();
}
