mod audio;
mod clickwheel;
mod config;
mod player;
mod subsonic;
mod ui;

use anyhow::Result;
use tracing::info;

slint::include_modules!();

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    info!("NaviPod starting up");

    let cfg = config::Config::load()?;
    info!("Config loaded: server = {}", cfg.server.url);

    let subsonic = subsonic::Client::new(cfg.server.clone());

    // Kick off the Slint UI — this blocks on the main thread (required by Slint)
    ui::run(subsonic, cfg).await?;

    Ok(())
}
