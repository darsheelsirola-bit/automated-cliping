use crate::model::{ChatActivity, Clip, Creator, Dashboard, ErrorEntry, Highlight, Settings};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

#[derive(Clone)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open(path: impl Into<PathBuf>) -> rusqlite::Result<Self> {
        let database = Self { path: path.into() };
        let mut conn = database.connection()?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 6 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        if version == 0 {
            transaction.execute_batch(include_str!("../migrations/001_initial.sql"))?;
            transaction.pragma_update(None, "user_version", 1)?;
        }
        if version < 2 {
            transaction.execute_batch(include_str!("../migrations/002_publishing.sql"))?;
            transaction.pragma_update(None, "user_version", 2)?;
        }
        if version < 3 {
            transaction
                .execute_batch(include_str!("../migrations/003_capture_and_processing.sql"))?;
            transaction.pragma_update(None, "user_version", 3)?;
        }
        if version < 4 {
            transaction.execute_batch(include_str!("../migrations/004_media_locations.sql"))?;
            transaction.pragma_update(None, "user_version", 4)?;
        }
        if version < 5 {
            transaction.execute_batch(include_str!("../migrations/005_adaptive_clip_range.sql"))?;
            transaction.pragma_update(None, "user_version", 5)?;
        }
        if version < 6 {
            transaction.execute_batch(include_str!("../migrations/006_publication_cleanup.sql"))?;
            transaction.pragma_update(None, "user_version", 6)?;
        }
        transaction.commit()?;
        Ok(database)
    }

    pub fn connection(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Ok(conn)
    }

    pub fn settings(&self) -> Result<Settings, String> {
        self.settings_with(&self.connection().map_err(db_error)?)
    }

    fn settings_with(&self, conn: &Connection) -> Result<Settings, String> {
        let json: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'preferences'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let mut settings: Settings = match json {
            Some(value) => serde_json::from_str(&value)
                .map_err(|err| format!("Settings could not be read. {err}"))?,
            None => Settings::default(),
        };
        if settings.storage_directory.is_empty() {
            settings.storage_directory = self
                .path
                .with_file_name("media")
                .to_string_lossy()
                .into_owned();
        }
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), String> {
        if !settings.saved_budget_gb.is_finite()
            || !(1.0..=1000.0).contains(&settings.saved_budget_gb)
        {
            return Err("Choose a saved clip budget between 1 and 1,000 GB.".into());
        }
        if settings.min_clip_seconds < 5
            || settings.max_clip_seconds > 180
            || settings.min_clip_seconds > settings.max_clip_seconds
        {
            return Err("Choose a clip duration range between 5 and 180 seconds.".into());
        }
        if settings.minimum_confidence > 100 || settings.detection_threshold > 100 {
            return Err("Choose a confidence threshold between 0 and 100.".into());
        }
        let folder = Path::new(settings.storage_directory.trim());
        if !folder.is_absolute() {
            return Err("Choose an absolute storage folder path.".into());
        }
        std::fs::create_dir_all(folder)
            .map_err(|err| format!("Could not create the storage folder. {err}"))?;
        let mut settings = settings.clone();
        settings.storage_directory = folder.to_string_lossy().into_owned();
        let json = serde_json::to_string(&settings).map_err(|err| err.to_string())?;
        self.connection()
            .map_err(db_error)?
            .execute(
                "INSERT INTO app_settings(key, value) VALUES ('preferences', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [json],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn add_creator(&self, value: &str) -> Result<(), String> {
        let username = normalize_creator(value)?;
        let url = format!("https://www.twitch.tv/{username}");
        let changed = self.connection().map_err(db_error)?.execute(
            "INSERT INTO creators(username, channel_url) VALUES (?1, ?2)
             ON CONFLICT(username) DO UPDATE SET archived = 0, enabled = 1, status = 'idle', last_error = NULL
             WHERE creators.archived = 1",
            params![username, url],
        ).map_err(db_error)?;
        if changed == 0 {
            return Err(format!("{username} is already on your creator list."));
        }
        Ok(())
    }

    pub fn set_creator_enabled(&self, id: i64, enabled: bool) -> Result<(), String> {
        let changed = self.connection().map_err(db_error)?.execute(
            "UPDATE creators SET enabled = ?2, status = 'idle', last_error = NULL WHERE id = ?1 AND archived = 0",
            params![id, enabled],
        ).map_err(db_error)?;
        if changed == 0 {
            return Err("This creator is no longer on the list.".into());
        }
        Ok(())
    }

    pub fn remove_creator(&self, id: i64) -> Result<(), String> {
        self.connection()
            .map_err(db_error)?
            .execute(
                "UPDATE creators SET archived = 1, enabled = 0, status = 'idle' WHERE id = ?1",
                [id],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn clip_output(&self, id: i64) -> Result<String, String> {
        self.connection()
            .map_err(db_error)?
            .query_row(
                "SELECT output_path FROM clip_jobs WHERE id = ?1 AND output_path IS NOT NULL",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| "This clip has no local export.".into())
    }

    pub fn dashboard(&self) -> Result<Dashboard, String> {
        let mut conn = self.connection().map_err(db_error)?;
        let tx = conn.transaction().map_err(db_error)?;
        let creators = collect(&tx, "SELECT c.id, c.username, c.channel_url, c.enabled, c.status, c.stream_title,
            c.last_live_at, c.last_error,
            (SELECT count(*) FROM highlight_events h JOIN stream_sessions s ON s.id = h.session_id WHERE s.creator_id = c.id),
            (SELECT count(*) FROM clip_jobs j JOIN highlight_events h ON h.id = j.highlight_id JOIN stream_sessions s ON s.id = h.session_id WHERE s.creator_id = c.id AND j.output_path IS NOT NULL),
            (SELECT count(*) FROM uploaded_videos u JOIN clip_jobs j ON j.id = u.clip_id JOIN highlight_events h ON h.id = j.highlight_id JOIN stream_sessions s ON s.id = h.session_id WHERE s.creator_id = c.id)
            FROM creators c WHERE c.archived = 0 ORDER BY c.username", |row| {
            Ok(Creator { id: row.get(0)?, username: row.get(1)?, channel_url: row.get(2)?,
                enabled: row.get(3)?, status: row.get(4)?, stream_title: row.get(5)?, last_live_at: row.get(6)?,
                last_error: row.get(7)?, highlights: row.get(8)?, clips: row.get(9)?, published: row.get(10)? })
        }).map_err(db_error)?;
        let highlights = collect(&tx, "SELECT h.id, c.username, h.triggered_at, h.score, h.confidence
            FROM highlight_events h JOIN stream_sessions s ON s.id = h.session_id JOIN creators c ON c.id = s.creator_id
            ORDER BY h.triggered_at DESC, h.id DESC LIMIT 100", |row| Ok(Highlight {
            id: row.get(0)?, username: row.get(1)?, triggered_at: row.get(2)?, score: row.get(3)?, confidence: row.get(4)?,
        })).map_err(db_error)?;
        let chat_activity = collect(&tx, "SELECT c.id, a.sampled_at, a.messages, a.unique_chatters
            FROM chat_activity_samples a JOIN stream_sessions s ON s.id = a.session_id JOIN creators c ON c.id = s.creator_id
            WHERE s.ended_at IS NULL AND c.archived = 0 AND c.enabled = 1
            AND c.status IN ('live','connecting','monitoring','recording','reconnecting')
            AND a.sampled_at >= unixepoch() - 300
            ORDER BY c.id, a.sampled_at", |row| Ok(ChatActivity {
            creator_id: row.get(0)?, sampled_at: row.get(1)?, messages: row.get(2)?, unique_chatters: row.get(3)?,
        })).map_err(db_error)?;
        let clips = collect(&tx, "SELECT j.id, c.username, j.status, j.duration_ms, j.created_at, j.output_path, j.thumbnail_path,
            j.error, t.status, r.status, p.status, p.error, p.uploaded_bytes, u.video_id, u.title, u.published_at
            FROM clip_jobs j JOIN highlight_events h ON h.id = j.highlight_id JOIN stream_sessions s ON s.id = h.session_id
            JOIN creators c ON c.id = s.creator_id LEFT JOIN transcription_jobs t ON t.clip_id = j.id
            LEFT JOIN render_jobs r ON r.clip_id = j.id LEFT JOIN publishing_jobs p ON p.clip_id = j.id
            LEFT JOIN uploaded_videos u ON u.clip_id = j.id ORDER BY j.created_at DESC, j.id DESC LIMIT 300", |row| Ok(Clip {
            id: row.get(0)?, username: row.get(1)?, status: row.get(2)?, duration_ms: row.get(3)?, created_at: row.get(4)?,
            output_path: row.get(5)?, thumbnail_path: row.get(6)?, error: row.get(7)?, transcription_status: row.get(8)?,
            render_status: row.get(9)?, publishing_status: row.get(10)?, publishing_error: row.get(11)?,
            uploaded_bytes: row.get(12)?, video_id: row.get(13)?, title: row.get(14)?, published_at: row.get(15)?,
        })).map_err(db_error)?;
        let errors = collect(
            &tx,
            "SELECT id, subsystem, message, occurred_at FROM error_logs ORDER BY id DESC LIMIT 25",
            |row| {
                Ok(ErrorEntry {
                    id: row.get(0)?,
                    subsystem: row.get(1)?,
                    message: row.get(2)?,
                    occurred_at: row.get(3)?,
                })
            },
        )
        .map_err(db_error)?;
        let count = |sql| {
            tx.query_row(sql, [], |row| row.get::<_, i64>(0))
                .map_err(db_error)
        };
        let settings = self.settings_with(&tx)?;
        let available_bytes = Path::new(&settings.storage_directory)
            .ancestors()
            .find_map(|path| fs2::available_space(path).ok());
        Ok(Dashboard {
            creators, clips, highlights, chat_activity, errors, settings, available_bytes,
            highlight_count: count("SELECT count(*) FROM highlight_events")?,
            queued_count: count("SELECT count(*) FROM clip_jobs WHERE status IN ('queued','retry','extracting','transcribing','rendering','metadata','processing')")?,
            published_count: count("SELECT count(*) FROM uploaded_videos")?,
            saved_bytes: count("SELECT coalesce(sum(saved_bytes), 0) FROM clip_jobs")?,
            temporary_bytes: count("SELECT coalesce(sum(bytes), 0) FROM recording_segments")?,
        })
    }
}

fn collect<T>(
    conn: &Connection,
    sql: &str,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([], map)?;
    rows.collect()
}

fn normalize_creator(value: &str) -> Result<String, String> {
    let value = value.trim();
    let candidate = if value.contains("://") {
        let url = Url::parse(value).map_err(|_| "Enter a valid Twitch channel URL.")?;
        if !matches!(
            url.host_str(),
            Some("twitch.tv" | "www.twitch.tv" | "m.twitch.tv")
        ) || !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return Err("Enter a twitch.tv channel URL.".into());
        }
        url.path()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_owned()
    } else {
        value.to_owned()
    };
    if !(1..=25).contains(&candidate.len())
        || !candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("Use a Twitch username or a direct Twitch channel URL.".into());
    }
    Ok(candidate.to_ascii_lowercase())
}

fn db_error(error: rusqlite::Error) -> String {
    format!("ClipForge could not read or update its local database. {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_twitch_usernames_and_deduplicates_case() {
        assert_eq!(
            normalize_creator(" https://WWW.TWITCH.TV/Example_Name/?source=share ").unwrap(),
            "example_name"
        );
        assert_eq!(normalize_creator("example_NAME").unwrap(), "example_name");
        for invalid in [
            "",
            "https://twitch.tv.evil.test/my_name",
            "https://twitch.tv/my_name/videos",
            "a b",
            "https://name@twitch.tv/channel",
        ] {
            assert!(normalize_creator(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn migrates_only_the_previous_factory_duration_range() {
        for (min, max, expected) in [(25, 60, (10, 45)), (15, 60, (15, 60)), (10, 10, (10, 10))] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("migration.db");
            let db = Database::open(&path).unwrap();
            let mut settings = db.settings().unwrap();
            settings.min_clip_seconds = min;
            settings.max_clip_seconds = max;
            db.save_settings(&settings).unwrap();
            db.connection()
                .unwrap()
                .pragma_update(None, "user_version", 4)
                .unwrap();
            let updated = Database::open(&path).unwrap().settings().unwrap();
            assert_eq!(
                (updated.min_clip_seconds, updated.max_clip_seconds),
                expected
            );
            assert_eq!(updated.storage_directory, settings.storage_directory);
        }
    }

    #[test]
    fn migration_and_creator_state_survive_reopening() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("clipforge.db");
        let original = Database::open(&path).unwrap();
        original.add_creator("My_Streamer").unwrap();
        assert!(original.add_creator("my_streamer").is_err());
        let creator = original.dashboard().unwrap().creators.remove(0);
        original.set_creator_enabled(creator.id, false).unwrap();
        let original_settings = Settings {
            saved_budget_gb: 15.0,
            storage_directory: folder.path().join("clips").to_string_lossy().into_owned(),
            ..Settings::default()
        };
        original.save_settings(&original_settings).unwrap();
        let reopened = Database::open(&path).unwrap();
        let dashboard = reopened.dashboard().unwrap();
        assert!(!dashboard.creators[0].enabled);
        assert_eq!(dashboard.settings.saved_budget_gb, 15.0);
        reopened.remove_creator(creator.id).unwrap();
        assert!(reopened.dashboard().unwrap().creators.is_empty());
        reopened.add_creator("my_streamer").unwrap();
        assert_eq!(reopened.dashboard().unwrap().creators[0].id, creator.id);
        assert!(reopened
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO stream_sessions(creator_id, started_at) VALUES (999,1)",
                []
            )
            .is_err());
    }
}
