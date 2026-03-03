mod audio;
mod clickwheel;
mod config;
mod db;
mod player;
mod subsonic;
mod system;
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
    let database = db::Db::open()?;
    info!("Database opened");

    // System status channel — poll every 30 s, first tick is immediate.
    let (system_tx, system_rx) = tokio::sync::watch::channel(system::SystemStatus::default());
    {
        let tx = system_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                tx.send(system::poll_status().await).ok();
            }
        });
    }

    // Kick off the Slint UI — this blocks on the main thread (required by Slint)
    ui::run(subsonic, cfg, database, system_tx, system_rx).await?;

    Ok(())
}
