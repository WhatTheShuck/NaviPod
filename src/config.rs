use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub url: String,
    pub username: String,
    pub password: String,
    /// Subsonic API version to advertise (default: "1.16.1")
    #[serde(default = "default_api_version")]
    pub api_version: String,
}

fn default_api_version() -> String {
    "1.16.1".into()
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UiConfig {
    /// "liquid_glass" | "material" | "classic_ipod"
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_theme() -> String {
    "liquid_glass".into()
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("Reading config from {:?}", path))?;
            let cfg: Config = toml::from_str(&raw)
                .with_context(|| format!("Parsing config from {:?}", path))?;
            Ok(cfg)
        } else {
            // Write a template config and bail with a friendly message
            let template = r#"[server]
url = "http://your-subsonic-server:4533"
username = "admin"
password = "changeme"

[ui]
theme = "liquid_glass"   # liquid_glass | material | classic_ipod
"#;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, template)
                .with_context(|| format!("Writing template config to {:?}", path))?;
            anyhow::bail!(
                "No config found — a template has been written to {:?}\nPlease fill it in and restart.",
                path
            )
        }
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("navipod")
        .join("config.toml")
}
