use super::{account, http, metadata_path, token_request, Account};
use crate::db::Database;
use reqwest::{
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE, RETRY_AFTER},
    Client, Response,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use url::Url;

const CHUNK_SIZE: usize = 8 * 1024 * 1024;

pub async fn supervise(db: Database) {
    let client = match http() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("ClipForge YouTube client failed: {error}");
            return;
        }
    };
    loop {
        if let Err(error) = super::cleanup::discard_one(&db) {
            eprintln!("ClipForge rejected clip cleanup: {error}");
        }
        if let Err(error) = attempt_one(&db, &client).await {
            eprintln!("ClipForge upload worker: {error}");
        }
        if let Err(error) = super::cleanup::check_one(&db, &client).await {
            eprintln!("ClipForge publication cleanup: {error}");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[derive(Debug)]
struct Job {
    id: i64,
    path: PathBuf,
    uri: Option<String>,
    channel: Option<String>,
    size: Option<u64>,
    may_have_committed: bool,
    attempts: u32,
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(default)]
    quality_version: u32,
    #[serde(default)]
    editorial_pass: bool,
    title: String,
    description: String,
}

enum TransferError {
    Retry(String, Option<u64>),
    ChannelLimit(u64),
    Stop(String),
    Rejected(String),
    Uncertain,
    Local(String),
}

pub(super) fn channel_retry_at(conn: &Connection, channel: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT retry_at FROM youtube_channel_cooldowns WHERE channel_id=?1 AND retry_at>unixepoch()",
        [channel], |row| row.get(0),
    ).optional()
}

fn claim_next_job(db: &Database, channel: &str) -> rusqlite::Result<Option<Job>> {
    let mut conn = db.connection()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if channel_retry_at(&tx, channel)?.is_some() {
        return Ok(None);
    }
    let job = tx
        .query_row(
            "SELECT p.clip_id,j.output_path,p.upload_session,
        p.channel_id,p.file_size,p.may_have_committed,p.attempts
        FROM publishing_jobs p JOIN clip_jobs j ON j.id=p.clip_id
        LEFT JOIN clip_review r ON r.clip_id=j.id
        WHERE p.status IN ('queued','retry','uploading')
         AND (p.retry_at IS NULL OR p.retry_at <= unixepoch())
         AND (p.channel_id IS NULL OR p.channel_id=?1)
         AND (r.decision IS NULL OR r.decision<>'hold' OR p.may_have_committed=1)
         AND j.status = 'ready' AND j.output_path IS NOT NULL
         AND NOT EXISTS(SELECT 1 FROM uploaded_videos u WHERE u.clip_id=p.clip_id)
        ORDER BY p.may_have_committed DESC,p.clip_id LIMIT 1",
            [channel],
            |row| {
                Ok(Job {
                    id: row.get(0)?,
                    path: PathBuf::from(row.get::<_, String>(1)?),
                    uri: row.get(2)?,
                    channel: row.get(3)?,
                    size: row.get(4)?,
                    may_have_committed: row.get(5)?,
                    attempts: row.get(6)?,
                })
            },
        )
        .optional()?;
    if let Some(job) = &job {
        tx.execute(
            "UPDATE publishing_jobs SET status='uploading' WHERE clip_id=?1",
            [job.id],
        )?;
    }
    tx.commit()?;
    Ok(job)
}

fn set_channel_cooldown(conn: &Connection, channel: &str, delay: u64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO youtube_channel_cooldowns(channel_id,retry_at) VALUES(?1,unixepoch()+?2)
        ON CONFLICT(channel_id) DO UPDATE SET retry_at=max(retry_at,excluded.retry_at)",
        params![channel, delay],
    )?;
    Ok(())
}

async fn attempt_one(db: &Database, client: &Client) -> Result<(), String> {
    if !db.settings()?.auto_publish {
        return Ok(());
    }
    let Some(account) = account()? else {
        return Ok(());
    };
    let Some(mut job) =
        claim_next_job(db, &account.channel_id).map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let result = upload(db, client, &account, &mut job).await;
    let mut channel_delay = None;
    let (state, message, delay) = match result {
        Ok(()) => return Ok(()),
        Err(TransferError::Local(error)) => return Err(error),
        Err(TransferError::Uncertain) => (
            "uncertain",
            "The prior upload may have completed. Check YouTube Studio before taking any action."
                .into(),
            None,
        ),
        Err(TransferError::Stop(error)) => ("failed", error, None),
        Err(TransferError::Rejected(error)) => ("rejected", error, None),
        Err(TransferError::ChannelLimit(delay)) => {
            channel_delay = Some(delay);
            (
                "retry",
                "The channel reached its YouTube upload limit.".into(),
                Some(delay),
            )
        }
        Err(TransferError::Retry(error, delay)) => {
            let delay = delay.unwrap_or_else(|| (15 * 2_u64.pow(job.attempts.min(8))).min(3600));
            ("retry", error, Some(delay.min(86_400)))
        }
    };
    db.connection()
        .and_then(|mut connection| {
            let tx = connection.transaction()?;
            if let Some(delay) = channel_delay {
                set_channel_cooldown(&tx, &account.channel_id, delay)?;
            }
            tx.execute(
                "UPDATE publishing_jobs
        SET status=?2,error=?3,retry_at=CASE WHEN ?4 IS NULL THEN NULL ELSE unixepoch()+?4 END,
        attempts=attempts+1 WHERE clip_id=?1",
                params![job.id, state, message, delay],
            )?;
            tx.commit()
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn upload(
    db: &Database,
    client: &Client,
    account: &Account,
    job: &mut Job,
) -> Result<(), TransferError> {
    use TransferError::{Local, Retry, Stop, Uncertain};
    if job
        .channel
        .as_ref()
        .is_some_and(|id| id != &account.channel_id)
    {
        return Err(Stop(
            "This upload belongs to another YouTube channel.".into(),
        ));
    }
    let mut file = tokio::fs::File::open(&job.path)
        .await
        .map_err(|_| Stop("The validated Short file is missing.".into()))?;
    let size = file
        .metadata()
        .await
        .map_err(|error| Stop(error.to_string()))?
        .len();
    if size == 0 || job.size.is_some_and(|expected| expected != size) {
        return Err(Stop("The Short file changed after uploading began.".into()));
    }
    let metadata = read_metadata(&job.path).await?;
    let token = token_request(
        client,
        &account.desktop,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", account.refresh_token.as_str()),
        ],
    )
    .await
    .map_err(|error| Retry(error, Some(300)))?
    .access_token;

    let mut offset = 0;
    let mut uri = if let Some(uri) = &job.uri {
        let uri = session_url(uri)?;
        let response = client
            .put(uri.clone())
            .bearer_auth(&token)
            .header(CONTENT_LENGTH, 0)
            .header(CONTENT_RANGE, format!("bytes */{size}"))
            .send()
            .await
            .map_err(|_| Retry("Cannot reach YouTube to resume the upload.".into(), None))?;
        match response.status().as_u16() {
            200 | 201 => return complete(db, job.id, &metadata.title, response).await,
            308 => {
                offset = received(&response, size)?;
                if !job.may_have_committed && offset < size {
                    if let Err(error) = check_upload_policy(db, job.id, &metadata) {
                        if matches!(error, TransferError::Stop(_) | TransferError::Rejected(_)) {
                            release_uncommitted_session(db, job.id)?;
                        }
                        return Err(error);
                    }
                }
                track(
                    db,
                    job.id,
                    offset,
                    Some(uri.as_str()),
                    job.may_have_committed,
                )?;
                uri
            }
            404 | 410 if job.may_have_committed => return Err(Uncertain),
            404 | 410 => match start(db, client, account, job.id, &token, &metadata, size).await {
                Ok(uri) => uri,
                Err(error) => {
                    if matches!(error, TransferError::Stop(_) | TransferError::Rejected(_)) {
                        release_uncommitted_session(db, job.id)?;
                    }
                    return Err(error);
                }
            },
            _ => return Err(api_error(response).await),
        }
    } else {
        start(db, client, account, job.id, &token, &metadata, size).await?
    };

    let mut buffer = vec![0; CHUNK_SIZE];
    while offset < size {
        if !db.settings().map_err(Local)?.auto_publish {
            return Ok(());
        }
        if super::account()
            .map_err(Local)?
            .as_ref()
            .map(|value| value.channel_id.as_str())
            != Some(account.channel_id.as_str())
        {
            return Err(Stop(
                "The connected YouTube channel changed during upload.".into(),
            ));
        }
        let length = (size - offset).min(CHUNK_SIZE as u64) as usize;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|error| Stop(error.to_string()))?;
        file.read_exact(&mut buffer[..length])
            .await
            .map_err(|error| Stop(error.to_string()))?;
        let end = offset + length as u64;
        let final_chunk = end == size;
        if final_chunk {
            // A crash after this point could leave a committed video and no local response.
            track(db, job.id, offset, Some(uri.as_str()), true)?;
            job.may_have_committed = true;
        }
        let response = client
            .put(uri.clone())
            .bearer_auth(&token)
            .header(CONTENT_TYPE, "video/mp4")
            .header(CONTENT_LENGTH, length)
            .header(CONTENT_RANGE, format!("bytes {offset}-{}/{size}", end - 1))
            .body(buffer[..length].to_vec())
            .send()
            .await
            .map_err(|_| {
                Retry(
                    "Upload interrupted. ClipForge will check the same session before resuming."
                        .into(),
                    None,
                )
            })?;
        match response.status().as_u16() {
            200 | 201 => return complete(db, job.id, &metadata.title, response).await,
            308 => {
                if let Some(location) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                {
                    uri = session_url(location)?;
                }
                let received = received(&response, size)?;
                track(
                    db,
                    job.id,
                    received,
                    Some(uri.as_str()),
                    job.may_have_committed,
                )?;
                if received <= offset {
                    return Err(Retry("YouTube made no upload progress.".into(), None));
                }
                offset = received;
            }
            404 | 410 if job.may_have_committed => return Err(Uncertain),
            404 | 410 => {
                return Err(Retry(
                    "The upload session expired. ClipForge will renew it.".into(),
                    None,
                ))
            }
            _ => return Err(api_error(response).await),
        }
    }
    Err(Retry("YouTube is finalizing the upload.".into(), None))
}

async fn read_metadata(path: &Path) -> Result<Metadata, TransferError> {
    let path = metadata_path(path).map_err(TransferError::Stop)?;
    let json = tokio::fs::read(path)
        .await
        .map_err(|_| TransferError::Stop("YouTube metadata is missing.".into()))?;
    serde_json::from_slice(&json)
        .map_err(|_| TransferError::Stop("YouTube metadata is invalid.".into()))
}

fn passes_minimum(confidence: Option<f64>, minimum: u32) -> bool {
    confidence.is_some_and(|value| value >= f64::from(minimum.max(80)))
}

fn passes_editorial(editorial_pass: bool, decision: Option<&str>) -> bool {
    editorial_pass && decision != Some("hold")
}

fn check_upload_policy(db: &Database, id: i64, metadata: &Metadata) -> Result<(), TransferError> {
    let settings = db.settings().map_err(TransferError::Local)?;
    if !settings.auto_publish {
        return Err(TransferError::Retry(
            "Automatic publishing is paused.".into(),
            Some(300),
        ));
    }
    let (confidence, editorial_pass, decision, editor_version): (Option<f64>, Option<bool>, Option<String>, i64) =
        db.connection().and_then(|conn| conn.query_row(
            "SELECT h.confidence,j.editorial_pass,r.decision,j.editor_version FROM clip_jobs j
             JOIN highlight_events h ON h.id=j.highlight_id LEFT JOIN clip_review r ON r.clip_id=j.id
             WHERE j.id=?1",
            [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )).map_err(|err| TransferError::Local(err.to_string()))?;
    if decision.as_deref() == Some("hold") {
        return Err(TransferError::Retry(
            "This clip is held locally.".into(),
            Some(300),
        ));
    }
    if editorial_pass == Some(false)
        || (editor_version == 1
            && editorial_pass.is_some()
            && confidence.is_some()
            && !passes_minimum(confidence, settings.minimum_confidence))
    {
        return Err(TransferError::Rejected(
            "The automatic review rejected this clip.".into(),
        ));
    }
    if metadata.quality_version != 2 {
        return Err(TransferError::Stop(
            "This clip lacks a current highlight quality assessment.".into(),
        ));
    }
    if !passes_editorial(
        metadata.editorial_pass && editorial_pass == Some(true),
        decision.as_deref(),
    ) {
        return Err(TransferError::Stop(
            "This clip lacks a passing editorial assessment.".into(),
        ));
    }
    if !passes_minimum(confidence, settings.minimum_confidence) {
        return Err(TransferError::Retry(
            "Waiting for current publishing preferences to allow this clip.".into(),
            Some(300),
        ));
    }
    Ok(())
}

fn release_uncommitted_session(db: &Database, id: i64) -> Result<(), TransferError> {
    db.connection()
        .and_then(|conn| {
            conn.execute(
                "UPDATE publishing_jobs
         SET upload_session=NULL,uploaded_bytes=0,channel_id=NULL,file_size=NULL
         WHERE clip_id=?1 AND may_have_committed=0",
                [id],
            )
        })
        .map_err(|err| TransferError::Local(err.to_string()))?;
    Ok(())
}

async fn start(
    db: &Database,
    client: &Client,
    account: &Account,
    id: i64,
    token: &str,
    metadata: &Metadata,
    size: u64,
) -> Result<Url, TransferError> {
    check_upload_policy(db, id, metadata)?;
    let response = client
        .post("https://www.googleapis.com/upload/youtube/v3/videos")
        .query(&[("part", "snippet,status"), ("uploadType", "resumable")])
        .bearer_auth(token)
        .header("X-Upload-Content-Length", size)
        .header("X-Upload-Content-Type", "video/mp4")
        .json(&serde_json::json!({
            "snippet": {"title": metadata.title, "description": metadata.description},
            "status": {"privacyStatus": "public"},
        }))
        .send()
        .await
        .map_err(|_| {
            TransferError::Retry("Could not start the YouTube upload session.".into(), None)
        })?;
    if !response.status().is_success() {
        return Err(api_error(response).await);
    }
    let url = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            TransferError::Retry("YouTube omitted the resumable upload URL.".into(), None)
        })?;
    let uri = session_url(url)?;
    db.connection()
        .and_then(|connection| {
            connection.execute(
                "UPDATE publishing_jobs
        SET status='uploading', upload_session=?2, uploaded_bytes=0, channel_id=?3,
        file_size=?4, may_have_committed=0, error=NULL, retry_at=NULL WHERE clip_id=?1",
                params![id, uri.as_str(), account.channel_id, size],
            )
        })
        .map_err(|error| TransferError::Local(error.to_string()))?;
    Ok(uri)
}

fn track(
    db: &Database,
    id: i64,
    uploaded: u64,
    uri: Option<&str>,
    may_have_committed: bool,
) -> Result<(), TransferError> {
    db.connection().and_then(|connection| connection.execute("UPDATE publishing_jobs
        SET status='uploading', uploaded_bytes=?2,upload_session=?3,may_have_committed=?4 WHERE clip_id=?1",
        params![id,uploaded,uri,may_have_committed]))
        .map(|_| ()).map_err(|error| TransferError::Local(error.to_string()))
}

async fn complete(
    db: &Database,
    id: i64,
    title: &str,
    response: Response,
) -> Result<(), TransferError> {
    let video: serde_json::Value = response.json().await.map_err(|_| {
        TransferError::Retry(
            "YouTube returned an unreadable completion response.".into(),
            None,
        )
    })?;
    let video_id = video["id"]
        .as_str()
        .filter(|id| {
            id.len() == 11
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
        .ok_or_else(|| {
            TransferError::Retry("YouTube did not return a valid video ID.".into(), None)
        })?;
    let mut connection = db
        .connection()
        .map_err(|error| TransferError::Local(error.to_string()))?;
    let tx = connection
        .transaction()
        .map_err(|error| TransferError::Local(error.to_string()))?;
    tx.execute(
        "INSERT INTO uploaded_videos(clip_id,video_id,title,published_at)
        VALUES(?1,?2,?3,unixepoch()) ON CONFLICT(clip_id) DO NOTHING",
        params![id, video_id, title],
    )
    .map_err(|error| TransferError::Local(error.to_string()))?;
    tx.execute(
        "UPDATE publishing_jobs SET status='uploaded',upload_session=NULL,uploaded_bytes=file_size,
        retry_at=NULL,error=NULL WHERE clip_id=?1",
        [id],
    )
    .map_err(|error| TransferError::Local(error.to_string()))?;
    tx.execute(
        "INSERT OR IGNORE INTO publication_cleanup(clip_id) VALUES(?1)",
        [id],
    )
    .map_err(|error| TransferError::Local(error.to_string()))?;
    tx.commit()
        .map_err(|error| TransferError::Local(error.to_string()))
}

fn session_url(value: &str) -> Result<Url, TransferError> {
    let uri = Url::parse(value)
        .map_err(|_| TransferError::Stop("Invalid YouTube upload session URL.".into()))?;
    if uri.scheme() != "https"
        || uri.username() != ""
        || uri.password().is_some()
        || uri.port_or_known_default() != Some(443)
        || !matches!(
            uri.host_str(),
            Some("www.googleapis.com" | "youtube.googleapis.com")
        )
        || uri.path() != "/upload/youtube/v3/videos"
    {
        return Err(TransferError::Stop(
            "YouTube returned an unexpected upload session URL.".into(),
        ));
    }
    Ok(uri)
}

fn received(response: &Response, size: u64) -> Result<u64, TransferError> {
    let value = response
        .headers()
        .get(RANGE)
        .and_then(|value| value.to_str().ok());
    parse_range(value, size)
        .ok_or_else(|| TransferError::Stop("YouTube returned an invalid upload range.".into()))
}

fn parse_range(range: Option<&str>, size: u64) -> Option<u64> {
    let Some(range) = range else {
        return Some(0);
    };
    range
        .strip_prefix("bytes=0-")?
        .parse::<u64>()
        .ok()?
        .checked_add(1)
        .filter(|received| *received <= size)
}

async fn api_error(response: Response) -> TransferError {
    let code = response.status();
    let delay = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.parse().ok());
    let details: serde_json::Value = response.json().await.unwrap_or_default();
    let reason = details["error"]["errors"][0]["reason"]
        .as_str()
        .unwrap_or_default();
    match reason {
        "quotaExceeded" | "dailyLimitExceeded" => {
            TransferError::Retry("YouTube API quota exhausted.".into(), Some(3600))
        }
        "uploadLimitExceeded" => {
            TransferError::ChannelLimit(delay.unwrap_or(3600).clamp(60, 86_400))
        }
        "rateLimitExceeded" | "userRateLimitExceeded" => {
            TransferError::Retry("YouTube is rate limiting uploads.".into(), delay)
        }
        _ if code.is_server_error() || matches!(code.as_u16(), 401 | 429) => TransferError::Retry(
            format!("YouTube upload temporarily failed ({code})."),
            delay,
        ),
        _ => TransferError::Stop(format!(
            "YouTube rejected this upload ({code}). Check channel and API permissions."
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fresh_sessions_follow_current_confidence_and_floor() {
        assert!(!super::passes_minimum(Some(79.), 70));
        assert!(!super::passes_minimum(Some(89.), 90));
        assert!(super::passes_minimum(Some(89.), 85));
        assert!(!super::passes_minimum(None, 80));
    }

    use super::*;

    #[test]
    fn respects_server_acknowledgment_ranges() {
        assert_eq!(parse_range(None, 100), Some(0));
        assert_eq!(parse_range(Some("bytes=0-49"), 100), Some(50));
        assert_eq!(parse_range(Some("bytes=0-100"), 100), None);
        assert_eq!(parse_range(Some("bytes=4-100"), 100), None);
        assert!(
            session_url("https://www.googleapis.com/upload/youtube/v3/videos?upload_id=1").is_ok()
        );
        assert!(
            session_url("https://www.googleapis.com.evil.test/upload/youtube/v3/videos").is_err()
        );
    }

    #[test]
    fn previously_approved_clips_still_need_to_pass_automatic_editorial_review() {
        assert!(passes_editorial(true, None));
        assert!(!passes_editorial(false, None));
        assert!(!passes_editorial(true, Some("hold")));
        assert!(!passes_editorial(false, Some("approved")));
        assert!(passes_editorial(true, Some("approved")));
    }

    #[test]
    fn cooldown_is_channel_wide_persistent_and_respects_held_clips() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("channel-cooldown.db");
        let db = Database::open(&path).unwrap();
        let conn = db.connection().unwrap();
        conn.execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at) VALUES(1,1,1);
             INSERT INTO highlight_events(id,session_id,triggered_at,score) VALUES(1,1,1,90),(2,1,2,90);
             INSERT INTO clip_jobs(id,highlight_id,status,output_path)
               VALUES(1,1,'ready','short1.mp4'),(2,2,'ready','short2.mp4');
             INSERT INTO publishing_jobs(clip_id,status) VALUES(1,'queued'),(2,'queued');
             INSERT INTO clip_review(clip_id,decision) VALUES(1,'hold');"
        ).unwrap();
        set_channel_cooldown(&conn, "limited", 3600).unwrap();
        let first = channel_retry_at(&conn, "limited").unwrap().unwrap();
        set_channel_cooldown(&conn, "limited", 60).unwrap();
        assert_eq!(channel_retry_at(&conn, "limited").unwrap(), Some(first));
        drop(conn);
        let reopened = Database::open(&path).unwrap();
        assert!(claim_next_job(&reopened, "limited").unwrap().is_none());

        let job = claim_next_job(&reopened, "other").unwrap().unwrap();
        assert_eq!(job.id, 2);
        let conn = reopened.connection().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM publishing_jobs WHERE clip_id=2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "uploading");
        conn.execute(
            "UPDATE youtube_channel_cooldowns SET retry_at=unixepoch()-1",
            [],
        )
        .unwrap();
        assert_eq!(claim_next_job(&reopened, "limited").unwrap().unwrap().id, 2);
    }

    #[test]
    fn releasing_a_legacy_session_preserves_the_final_chunk_fence() {
        let folder = tempfile::tempdir().unwrap();
        let db = Database::open(folder.path().join("legacy-resume.db")).unwrap();
        let conn = db.connection().unwrap();
        conn.execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at) VALUES(1,1,1);
             INSERT INTO highlight_events(id,session_id,triggered_at,score,confidence)
               VALUES(1,1,1,95,95),(2,1,2,95,95);
             INSERT INTO clip_jobs(id,highlight_id,status,output_path)
               VALUES(1,1,'ready','short1.mp4'),(2,2,'ready','short2.mp4');
             INSERT INTO publishing_jobs(clip_id,status,upload_session,may_have_committed)
               VALUES(1,'uploading','uncommitted',0),(2,'uploading','possibly-committed',1);"
        ).unwrap();
        let legacy = Metadata {
            quality_version: 2,
            editorial_pass: false,
            title: "Test".into(),
            description: String::new(),
        };
        let mut settings = db.settings().unwrap();
        settings.auto_publish = true;
        db.save_settings(&settings).unwrap();
        assert!(matches!(
            check_upload_policy(&db, 1, &legacy),
            Err(TransferError::Stop(_))
        ));
        release_uncommitted_session(&db, 1)
            .map_err(|_| "failed to release")
            .unwrap();
        release_uncommitted_session(&db, 2)
            .map_err(|_| "failed to preserve fence")
            .unwrap();
        let uri = |id: i64| {
            conn.query_row(
                "SELECT upload_session FROM publishing_jobs WHERE clip_id=?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        assert!(uri(1).is_none());
        assert_eq!(uri(2).as_deref(), Some("possibly-committed"));
    }

    #[tokio::test]
    async fn persists_final_chunk_fence_and_records_success_once() {
        use tokio::io::AsyncWriteExt;
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("upload-test.db");
        let db = Database::open(&path).unwrap();
        db.connection().unwrap().execute_batch(
            "INSERT INTO creators (id, username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions (id,creator_id,started_at) VALUES(1,1,1);
             INSERT INTO highlight_events (id,session_id,triggered_at,score) VALUES(1,1,1,80);
             INSERT INTO clip_jobs (id,highlight_id,status,output_path) VALUES(1,1,'ready','short.mp4');
             INSERT INTO publishing_jobs (clip_id,status,file_size) VALUES(1,'uploading',100);"
        ).unwrap();
        track(
            &db,
            1,
            70,
            Some("https://www.googleapis.com/upload/youtube/v3/videos?upload_id=test"),
            true,
        )
        .map_err(|_| "cannot persist fence")
        .unwrap();
        let reopened = Database::open(&path).unwrap();
        let job = claim_next_job(&reopened, "test").unwrap().unwrap();
        assert!(job.may_have_committed);
        assert!(job.uri.unwrap().contains("upload_id=test"));
        for _ in 0..2 {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let mock = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request).await.unwrap();
                let body = r#"{"id":"abcdefghijk"}"#;
                let reply = format!(
                    "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
            });
            let response = http()
                .unwrap()
                .get(format!("http://{address}"))
                .send()
                .await
                .unwrap();
            complete(&reopened, 1, "Test", response)
                .await
                .map_err(|_| "cannot complete upload")
                .unwrap();
            mock.await.unwrap();
        }
        assert!(claim_next_job(&reopened, "test").unwrap().is_none());
        let count: i64 = reopened
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM uploaded_videos WHERE clip_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let pending: i64 = reopened
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM publication_cleanup WHERE clip_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
    }
}
