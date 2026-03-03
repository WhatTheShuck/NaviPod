use anyhow::Result;
use rusqlite::{params, Connection};
use std::sync::{Arc, Mutex};

use crate::subsonic::Track;

/// Persisted queue + playback preferences, loaded on startup.
pub struct ResumeState {
    pub queue_index: usize,
    /// 0.0 – 1.0 (informational; seek not yet supported)
    pub progress: f32,
    pub volume: f32,
    pub shuffle: bool,
    /// "None" | "One" | "All"
    pub repeat_mode: String,
}

/// Thin, cheaply-cloneable handle to the SQLite database.
///
/// All methods take `&self` and lock the inner `Mutex` for the duration of the
/// call only — holding the lock for microseconds at most.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// Open (or create) the database at `~/.local/share/navipod/navipod.db`.
    pub fn open() -> Result<Self> {
        let path = db_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;

        // Embedded-friendly pragmas: WAL for SD-card writes, tiny cache.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous   = NORMAL;
             PRAGMA cache_size    = -64;
             PRAGMA temp_store    = MEMORY;",
        )?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS queue_tracks (
                position     INTEGER PRIMARY KEY,
                track_id     TEXT    NOT NULL,
                title        TEXT    NOT NULL,
                artist       TEXT,
                album        TEXT,
                duration     INTEGER,
                cover_art    TEXT,
                track_number INTEGER
            );
            CREATE TABLE IF NOT EXISTS resume_state (
                id           INTEGER PRIMARY KEY CHECK(id = 1),
                queue_index  INTEGER NOT NULL DEFAULT 0,
                progress     REAL    NOT NULL DEFAULT 0.0,
                volume       REAL    NOT NULL DEFAULT 0.7,
                shuffle      INTEGER NOT NULL DEFAULT 0,
                repeat_mode  TEXT    NOT NULL DEFAULT 'None'
            );
            CREATE TABLE IF NOT EXISTS favourites (
                track_id     TEXT    PRIMARY KEY,
                title        TEXT    NOT NULL,
                artist       TEXT,
                album        TEXT,
                duration     INTEGER,
                cover_art    TEXT,
                track_number INTEGER,
                starred_at   INTEGER NOT NULL
            );",
        )?;

        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    // ── Queue ─────────────────────────────────────────────────────────────────

    pub fn save_queue(&self, tracks: &[Track]) -> Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute("DELETE FROM queue_tracks", [])?;
        let mut stmt = conn.prepare(
            "INSERT INTO queue_tracks
             (position, track_id, title, artist, album, duration, cover_art, track_number)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for (i, t) in tracks.iter().enumerate() {
            stmt.execute(params![
                i as i64,
                t.id,
                t.title,
                t.artist,
                t.album,
                t.duration,
                t.cover_art,
                t.track_number,
            ])?;
        }
        Ok(())
    }

    pub fn load_queue(&self) -> Result<Vec<Track>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT track_id, title, artist, album, duration, cover_art, track_number
             FROM queue_tracks
             ORDER BY position",
        )?;
        let tracks = stmt
            .query_map([], |row| {
                Ok(Track {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    artist: row.get(2)?,
                    album: row.get(3)?,
                    duration: row.get(4)?,
                    cover_art: row.get(5)?,
                    track_number: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tracks)
    }

    // ── Resume state ──────────────────────────────────────────────────────────

    pub fn save_resume(&self, state: &ResumeState) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT OR REPLACE INTO resume_state
             (id, queue_index, progress, volume, shuffle, repeat_mode)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)",
            params![
                state.queue_index as i64,
                state.progress as f64,
                state.volume as f64,
                state.shuffle as i64,
                state.repeat_mode,
            ],
        )?;
        Ok(())
    }

    pub fn load_resume(&self) -> Result<Option<ResumeState>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT queue_index, progress, volume, shuffle, repeat_mode
             FROM resume_state
             WHERE id = 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(ResumeState {
                queue_index: row.get::<_, i64>(0)? as usize,
                progress: row.get::<_, f64>(1)? as f32,
                volume: row.get::<_, f64>(2)? as f32,
                shuffle: row.get::<_, i64>(3)? != 0,
                repeat_mode: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    // ── Favourites ────────────────────────────────────────────────────────────

    pub fn star_track(&self, track: &Track) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.0.lock().unwrap().execute(
            "INSERT OR REPLACE INTO favourites
             (track_id, title, artist, album, duration, cover_art, track_number, starred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                track.id,
                track.title,
                track.artist,
                track.album,
                track.duration,
                track.cover_art,
                track.track_number,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn unstar_track(&self, track_id: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM favourites WHERE track_id = ?1",
            params![track_id],
        )?;
        Ok(())
    }

    pub fn is_starred(&self, track_id: &str) -> Result<bool> {
        let count: i64 = self.0.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM favourites WHERE track_id = ?1",
            params![track_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_starred(&self) -> Result<Vec<Track>> {
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT track_id, title, artist, album, duration, cover_art, track_number
             FROM favourites
             ORDER BY starred_at DESC",
        )?;
        let tracks = stmt
            .query_map([], |row| {
                Ok(Track {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    artist: row.get(2)?,
                    album: row.get(3)?,
                    duration: row.get(4)?,
                    cover_art: row.get(5)?,
                    track_number: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(tracks)
    }
}

fn db_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("navipod")
        .join("navipod.db")
}
