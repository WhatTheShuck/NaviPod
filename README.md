# NaviPod

A Subsonic media player for the iPod Classic shell / Raspberry Pi Zero W.
Built with Rust + Slint, featuring three switchable themes.

## Themes

| Theme | Description |
|---|---|
| `liquid_glass` | Approximated Apple liquid glass aesthetic — frosted surfaces, accent glows |
| `material` | Dark material design — clean and legible on the small screen |
| `classic_ipod` | Faithful recreation of the original iPod Classic UI |

## Project structure

```
navipod/
├── Cargo.toml
├── build.rs                  # Compiles .slint files
├── cross-compile.sh          # Helper for Pi Zero W builds
├── src/
│   ├── main.rs
│   ├── config.rs             # TOML config loader
│   ├── ui/mod.rs             # Slint ↔ Rust bridge, event routing
│   ├── subsonic/mod.rs       # Subsonic REST API client
│   ├── player/mod.rs         # Audio playback state machine (rodio)
│   ├── clickwheel/mod.rs     # FFI bridge to C clickwheel driver
│   └── audio/
│       ├── mod.rs
│       └── resampler.rs      # Future DSP / EQ
└── ui/
    ├── main.slint            # Root window, view stack
    ├── themes/
    │   └── theme.slint       # Centralised design tokens
    └── views/
        ├── menu.slint        # Main menu
        ├── library.slint     # Artist / album / track lists
        └── now_playing.slint # Now playing screen
```

## Getting started

### 1. Install dependencies

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Audio libs (Linux)
sudo apt install libasound2-dev pkg-config

# Cross-compilation toolchain (when you're ready for the Pi)
sudo apt install gcc-arm-linux-gnueabihf
rustup target add arm-unknown-linux-gnueabihf
```

### 2. Configure

On first run, NaviPod writes a config template to:
`~/.config/navipod/config.toml`

Edit it with your Subsonic server details:

```toml
[server]
url = "http://your-server:4533"
username = "admin"
password = "yourpassword"

[ui]
theme = "liquid_glass"   # liquid_glass | material | classic_ipod
```

### 3. Run on your dev machine

```bash
cargo run
```

### 4. Build for Pi Zero W

```bash
chmod +x cross-compile.sh
./cross-compile.sh
```

The script will build the binary and then interactively ask if you want to
deploy it to your Pi.

## Clickwheel integration

The clickwheel module (`src/clickwheel/mod.rs`) supports two integration modes:

**Option B (default) — subprocess:**
Your C program writes event names to stdout, one per line:
```
SCROLL_DOWN
SELECT
MENU
PLAY_PAUSE
```
Set the binary path in `clickwheel_binary_path()`.

**Option A — shared library FFI:**
Compile your C driver as a `.so`, uncomment the `extern "C"` block in
`clickwheel/mod.rs`, and add link directives to `build.rs`.

Supported event strings:
`SCROLL_DOWN`, `SCROLL_UP`, `SELECT`, `MENU`, `PLAY_PAUSE`, `FAST_FORWARD`, `REWIND`

## Pi Zero W display

The default window size is 320×240 (standard iPod Classic resolution).
Slint uses OpenGL ES for rendering — on the Pi, make sure the GPU memory
split is at least 64 MB:

```
# /boot/config.txt
gpu_mem=64
```

Run with the appropriate display backend:
```bash
SLINT_BACKEND=linuxkms ./navipod    # KMS/DRM — no X11 needed
# or
DISPLAY=:0 ./navipod                # X11 if you have it running
```
