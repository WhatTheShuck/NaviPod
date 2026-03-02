use crate::config::ServerConfig;
use anyhow::{Context, Result};
use serde::Deserialize;

/// A minimal Subsonic REST API client.
/// Uses token-based auth (md5(password + salt)) per API spec.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    cfg: ServerConfig,
}

// ── Auth helpers ─────────────────────────────────────────────────────────────

fn make_token(password: &str, salt: &str) -> String {
    let input = format!("{}{}", password, salt);
    format!("{:x}", md5::compute(input.as_bytes()))
}

fn random_salt() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", t)
}

impl Client {
    pub fn new(cfg: ServerConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            cfg,
        }
    }

    /// Build base query params shared by every request.
    fn base_params(&self) -> Vec<(&'static str, String)> {
        let salt = random_salt();
        let token = make_token(&self.cfg.password, &salt);
        vec![
            ("u", self.cfg.username.clone()),
            ("t", token),
            ("s", salt),
            ("v", self.cfg.api_version.clone()),
            ("c", "navipod".into()),
            ("f", "json".into()),
        ]
    }

    fn url(&self, endpoint: &str) -> String {
        format!("{}/rest/{}", self.cfg.url.trim_end_matches('/'), endpoint)
    }

    // ── Artists ───────────────────────────────────────────────────────────────

    pub async fn get_artists(&self) -> Result<Vec<Artist>> {
        let url = self.url("getArtists");
        let resp: SubsonicResponse = self
            .http
            .get(&url)
            .query(&self.base_params())
            .send()
            .await
            .context("HTTP request to getArtists")?
            .json()
            .await
            .context("Parsing getArtists JSON")?;

        resp.check()?;

        Ok(resp
            .subsonic_response
            .artists
            .map(|a| a.index.into_iter().flat_map(|i| i.artist).collect())
            .unwrap_or_default())
    }

    // ── Albums ────────────────────────────────────────────────────────────────

    pub async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<Album>> {
        let url = self.url("getArtist");
        let mut params = self.base_params();
        params.push(("id", artist_id.to_string()));

        let resp: SubsonicResponse = self
            .http
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("HTTP request to getArtist")?
            .json()
            .await
            .context("Parsing getArtist JSON")?;

        resp.check()?;

        Ok(resp
            .subsonic_response
            .artist
            .map(|a| a.album)
            .unwrap_or_default())
    }

    // ── Tracks ────────────────────────────────────────────────────────────────

    pub async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let url = self.url("getAlbum");
        let mut params = self.base_params();
        params.push(("id", album_id.to_string()));

        let resp: SubsonicResponse = self
            .http
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("HTTP request to getAlbum")?
            .json()
            .await
            .context("Parsing getAlbum JSON")?;

        resp.check()?;

        Ok(resp
            .subsonic_response
            .album
            .map(|a| a.song)
            .unwrap_or_default())
    }

    // ── Streaming ─────────────────────────────────────────────────────────────

    /// Returns a fully-formed stream URL for a track.
    /// Rodio will open this URL directly.
    pub fn stream_url(&self, track_id: &str) -> String {
        let salt = random_salt();
        let token = make_token(&self.cfg.password, &salt);
        format!(
            "{}/rest/stream?u={}&t={}&s={}&v={}&c=navipod&id={}",
            self.cfg.url.trim_end_matches('/'),
            self.cfg.username,
            token,
            salt,
            self.cfg.api_version,
            track_id,
        )
    }

    /// Returns a cover art URL for a cover art ID.
    pub fn cover_art_url(&self, cover_id: &str, size: Option<u32>) -> String {
        let salt = random_salt();
        let token = make_token(&self.cfg.password, &salt);
        let size_param = size.map(|s| format!("&size={}", s)).unwrap_or_default();
        format!(
            "{}/rest/getCoverArt?u={}&t={}&s={}&v={}&c=navipod&id={}{}",
            self.cfg.url.trim_end_matches('/'),
            self.cfg.username,
            token,
            salt,
            self.cfg.api_version,
            cover_id,
            size_param,
        )
    }
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SubsonicResponse {
    #[serde(rename = "subsonic-response")]
    subsonic_response: SubsonicResponseInner,
}

impl SubsonicResponse {
    fn check(&self) -> Result<()> {
        if self.subsonic_response.status != "ok" {
            let msg = self
                .subsonic_response
                .error
                .as_ref()
                .map(|e| e.message.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Subsonic API error: {}", msg);
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct SubsonicResponseInner {
    status: String,
    error: Option<ApiError>,
    artists: Option<ArtistsWrapper>,
    artist: Option<ArtistDetail>,
    album: Option<AlbumDetail>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ArtistsWrapper {
    index: Vec<ArtistIndex>,
}

#[derive(Debug, Deserialize)]
struct ArtistIndex {
    artist: Vec<Artist>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(rename = "albumCount", default)]
    pub album_count: u32,
    #[serde(rename = "coverArt")]
    pub cover_art: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtistDetail {
    album: Vec<Album>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: Option<String>,
    pub year: Option<u32>,
    #[serde(rename = "songCount", default)]
    pub song_count: u32,
    #[serde(rename = "coverArt")]
    pub cover_art: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlbumDetail {
    song: Vec<Track>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<u32>,
    #[serde(rename = "coverArt")]
    pub cover_art: Option<String>,
    #[serde(rename = "track")]
    pub track_number: Option<u32>,
}
