use super::{account, token_request};
use crate::db::Database;
use reqwest::Client;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use std::{fs, io, path::Path};

fn public_and_processed(video: &serde_json::Value, id: &str, channel: &str) -> bool {
    video["id"] == id
        && video["snippet"]["channelId"] == channel
        && video["status"]["privacyStatus"] == "public"
        && video["status"]["uploadStatus"] == "processed"
}

fn remove_outputs(folder: &Path, id: i64, source: &Path, output: &Path) -> io::Result<()> {
    let clips = folder.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let root = clips.parent().ok_or(io::ErrorKind::InvalidInput)?;
    if !root.is_absolute()
        || folder != root.join("clips").join(id.to_string())
        || source != folder.join("source.mkv")
        || (output != folder.join("short.mp4")
            && output != folder.join("short-reviewed.mp4")
            && output != folder.join("short-reedited.mp4"))
        || folder.canonicalize()? != root.canonicalize()?.join("clips").join(id.to_string())
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let artifacts = [
        "source.mkv",
        "short.mp4",
        "short-reviewed.mp4",
        "short-reedited.mp4",
        "metadata-reedited.json",
        "thumbnail-reedited.jpg",
        "transcript-reedited.json",
        "captions-reedited.srt",
        "captions-reedited.ass",
        "thumbnail.jpg",
        "captions.ass",
        "captions.srt",
        "transcript.json",
        "metadata.json",
        "job.json",
        "result.json",
    ];
    // Validate the entire set first so a bad sidecar never causes partial cleanup.
    for name in artifacts {
        match fs::symlink_metadata(folder.join(name)) {
            Ok(meta) if meta.is_file() => (),
            Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    for name in artifacts {
        let file = folder.join(name);
        match fs::symlink_metadata(&file) {
            Ok(meta) if meta.is_file() => fs::remove_file(file)?,
            Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// Only an explicit policy rejection can release a publishing job for disposal.
const SAFE_TO_DISCARD: &str = "
    NOT EXISTS(SELECT 1 FROM publishing_jobs p WHERE p.clip_id=j.id
        AND (p.status<>'rejected' OR p.may_have_committed<>0 OR p.upload_session IS NOT NULL))
    AND NOT EXISTS(SELECT 1 FROM uploaded_videos u WHERE u.clip_id=j.id)
    AND NOT EXISTS(SELECT 1 FROM clip_review r WHERE r.clip_id=j.id AND r.decision='hold')";

pub(super) fn discard_one(db: &Database) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = db.connection()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let settings = db.settings_with(&tx)?;
    if !settings.auto_publish {
        return Ok(());
    }
    let minimum = settings.minimum_confidence.max(80);
    // A current, positive assessment still applies when the required score changes.
    // The upload worker checks the recorded metadata before opening any session.
    tx.execute(
        "INSERT INTO publishing_jobs(clip_id,status)
         SELECT j.id,'queued' FROM clip_jobs j
         JOIN highlight_events h ON h.id=j.highlight_id
         WHERE j.status='ready' AND j.editor_version=1 AND j.editorial_pass=1
           AND j.output_path IS NOT NULL AND h.confidence>=?1
           AND NOT EXISTS(SELECT 1 FROM publishing_jobs p WHERE p.clip_id=j.id)
           AND NOT EXISTS(SELECT 1 FROM uploaded_videos u WHERE u.clip_id=j.id)
           AND NOT EXISTS(SELECT 1 FROM clip_review r WHERE r.clip_id=j.id AND r.decision='hold')",
        [minimum],
    )?;
    // Older queued exports sometimes have a recorded editorial failure. A job
    // that has never attempted a transfer can be rejected without YouTube.
    tx.execute(
        "UPDATE publishing_jobs SET status='rejected',retry_at=NULL,
            error='The automatic review rejected this clip.'
         WHERE status='queued' AND attempts=0 AND uploaded_bytes=0 AND may_have_committed=0
           AND upload_session IS NULL AND file_size IS NULL AND channel_id IS NULL
           AND EXISTS(SELECT 1 FROM clip_jobs j JOIN highlight_events h ON h.id=j.highlight_id
               WHERE j.id=publishing_jobs.clip_id AND j.status='ready'
                 AND (j.editorial_pass=0 OR (j.editor_version=1 AND j.editorial_pass=1 AND h.confidence<?1))
                 AND j.output_path NOT LIKE '%short-reviewed.mp4')
           AND NOT EXISTS(SELECT 1 FROM uploaded_videos u WHERE u.clip_id=publishing_jobs.clip_id)
           AND NOT EXISTS(SELECT 1 FROM clip_review r WHERE r.clip_id=publishing_jobs.clip_id AND r.decision='hold')",
        [minimum],
    )?;
    let job = tx
        .query_row(
            &format!(
                "SELECT j.id,j.work_dir,j.source_path,j.output_path FROM clip_jobs j
        JOIN highlight_events h ON h.id=j.highlight_id
        WHERE j.work_dir IS NOT NULL AND j.source_path IS NOT NULL AND j.output_path IS NOT NULL
          AND j.output_path NOT LIKE '%short-reviewed.mp4'
          AND {SAFE_TO_DISCARD}
          AND (j.retry_at IS NULL OR j.retry_at<=unixepoch())
          AND (j.status='discarding' OR (
             j.status='ready' AND (j.editorial_pass=0 OR (
                 j.editor_version=1 AND j.editorial_pass IS NOT NULL AND h.confidence IS NOT NULL
                 AND (h.confidence<?1 OR EXISTS(
                     SELECT 1 FROM publishing_jobs p WHERE p.clip_id=j.id AND p.status='rejected'))))))
        ORDER BY (j.status='discarding') DESC,j.id LIMIT 1"
            ),
            [minimum],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((id, folder, source, output)) = job else {
        tx.commit()?;
        return Ok(());
    };
    // Claim permanently before touching files. An interrupted cleanup resumes
    // with the same paths and cannot enter the render, review, or upload queue.
    tx.execute(
        "UPDATE clip_jobs SET status='discarding',retry_at=unixepoch()+900,
        updated_at=unixepoch() WHERE id=?1",
        [id],
    )?;
    tx.commit()?;
    remove_outputs(
        Path::new(&folder),
        id,
        Path::new(&source),
        Path::new(&output),
    )?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    clear_local_paths(&tx, id, "discarded")?;
    tx.commit()?;
    Ok(())
}

fn clear_local_paths(tx: &rusqlite::Transaction<'_>, id: i64, state: &str) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE clip_jobs SET status=?2,source_path=NULL,output_path=NULL,thumbnail_path=NULL,
        work_dir=NULL,saved_bytes=0,retry_at=NULL,updated_at=unixepoch() WHERE id=?1",
        params![id, state],
    )?;
    tx.execute(
        "UPDATE render_jobs SET output_path=NULL WHERE clip_id=?1",
        [id],
    )?;
    tx.execute(
        "UPDATE transcription_jobs SET transcript_path=NULL,srt_path=NULL,ass_path=NULL
        WHERE clip_id=?1",
        [id],
    )?;
    Ok(())
}

pub(super) async fn check_one(
    db: &Database,
    client: &Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !db.settings()?.auto_publish {
        return Ok(());
    }
    let Some(acct) = account()? else {
        return Ok(());
    };
    let conn = db.connection()?;
    let job = conn.query_row("SELECT c.clip_id,u.video_id,p.channel_id,j.work_dir,j.source_path,j.output_path
        FROM publication_cleanup c
        JOIN uploaded_videos u USING(clip_id) JOIN publishing_jobs p USING(clip_id) JOIN clip_jobs j ON j.id=c.clip_id
        WHERE p.status='uploaded' AND j.work_dir IS NOT NULL AND j.source_path IS NOT NULL
          AND j.output_path IS NOT NULL AND p.channel_id=?1 AND c.check_after<=unixepoch()
        ORDER BY c.check_after,c.clip_id LIMIT 1", [&acct.channel_id], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?))).optional()?;
    let Some((id, video, channel, folder, source, output)) = job else {
        return Ok(());
    };
    conn.execute(
        "UPDATE publication_cleanup SET check_after=unixepoch()+900 WHERE clip_id=?1",
        [id],
    )?;
    drop(conn);
    let access = token_request(
        client,
        &acct.desktop,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &acct.refresh_token),
        ],
    )
    .await?;
    let response: serde_json::Value = client
        .get("https://www.googleapis.com/youtube/v3/videos")
        .query(&[
            ("part", "snippet,status"),
            ("id", &video),
            (
                "fields",
                "items(id,snippet/channelId,status(privacyStatus,uploadStatus))",
            ),
        ])
        .bearer_auth(access.access_token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !public_and_processed(&response["items"][0], &video, &channel) {
        return Ok(());
    }
    if !db.settings()?.auto_publish
        || account()?.as_ref().map(|acct| acct.channel_id.as_str()) != Some(channel.as_str())
    {
        return Ok(());
    }
    remove_outputs(
        Path::new(&folder),
        id,
        Path::new(&source),
        Path::new(&output),
    )?;
    let mut conn = db.connection()?;
    let tx = conn.transaction()?;
    clear_local_paths(&tx, id, "published")?;
    tx.execute("DELETE FROM publication_cleanup WHERE clip_id=?1", [id])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_cleanup_discards_only_evaluated_unattempted_reels() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("reels.db");
        let db = Database::open(&path).unwrap();
        let conn = db.connection().unwrap();
        conn.execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at) VALUES(1,1,1);"
        ).unwrap();
        for (id, editorial, confidence, version) in [
            (1, Some(false), 95, 1),
            (2, None, 95, 1),
            (3, Some(false), 95, 1),
            (4, Some(false), 95, 1),
            (5, Some(true), 70, 1),
            (6, Some(true), 95, 1),
            (7, Some(false), 95, 0),
            (11, Some(true), 70, 0),
            (12, Some(false), 90, 1),
            (8, Some(false), 95, 1),
            (9, Some(false), 95, 1),
            (10, Some(false), 95, 1),
        ] {
            let folder = root.path().join("clips").join(id.to_string());
            fs::create_dir_all(&folder).unwrap();
            let source = folder.join("source.mkv");
            let output = folder.join(if id == 12 {
                "short-reviewed.mp4"
            } else {
                "short.mp4"
            });
            fs::write(&source, b"source").unwrap();
            fs::write(&output, b"output").unwrap();
            fs::write(folder.join("notes.txt"), b"keep").unwrap();
            conn.execute(
                "INSERT INTO highlight_events(id,session_id,triggered_at,score,confidence)
                VALUES(?1,1,?1,?2,?2)",
                params![id, confidence],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO clip_jobs(id,highlight_id,status,work_dir,source_path,output_path,
                editorial_pass,editor_version,saved_bytes) VALUES(?1,?1,'ready',?2,?3,?4,?5,?6,12)",
                params![
                    id,
                    folder.to_str(),
                    source.to_str(),
                    output.to_str(),
                    editorial,
                    version
                ],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO publishing_jobs(clip_id,status,upload_session,may_have_committed)
                 VALUES(3,'retry',NULL,0),(7,'queued',NULL,0),(8,'rejected',NULL,0),
                 (9,'rejected',NULL,1),(10,'failed',NULL,0),(12,'queued',NULL,0);
             INSERT INTO clip_review(clip_id,decision) VALUES(4,'hold'),(8,'approved');",
        )
        .unwrap();

        // Turning off automation leaves every local export untouched.
        let mut settings = db.settings().unwrap();
        settings.auto_publish = false;
        settings.minimum_confidence = 96;
        db.save_settings(&settings).unwrap();
        discard_one(&db).unwrap();
        assert!(root.path().join("clips/1/short.mp4").exists());

        settings.auto_publish = true;
        settings.minimum_confidence = 90;
        db.save_settings(&settings).unwrap();
        // A crash after removing the source and before the DB update is retryable.
        conn.execute("UPDATE clip_jobs SET status='discarding' WHERE id=1", [])
            .unwrap();
        fs::remove_file(root.path().join("clips/1/source.mkv")).unwrap();
        let db = Database::open(&path).unwrap();
        for _ in 0..4 {
            discard_one(&db).unwrap();
        }
        let rows: Vec<_> = conn
            .prepare(
                "SELECT id FROM clip_jobs WHERE status='discarded'
            AND saved_bytes=0 AND source_path IS NULL AND output_path IS NULL ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![1, 5, 7, 8]);
        let publishing_status = |id| {
            conn.query_row(
                "SELECT status FROM publishing_jobs WHERE clip_id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        assert_eq!(publishing_status(6), "queued");
        assert_eq!(publishing_status(7), "rejected");
        settings.minimum_confidence = 96;
        db.save_settings(&settings).unwrap();
        discard_one(&db).unwrap();
        assert_eq!(publishing_status(6), "rejected");
        assert!(!root.path().join("clips/6/short.mp4").exists());
        assert_eq!(publishing_status(12), "queued");
        assert!(root.path().join("clips/12/short-reviewed.mp4").exists());
        for id in [1, 5, 7, 8] {
            let folder = root.path().join("clips").join(id.to_string());
            assert!(!folder.join("source.mkv").exists());
            assert!(!folder.join("short.mp4").exists());
            assert!(folder.join("notes.txt").exists());
        }
        for id in [2, 3, 4, 9, 10, 11] {
            assert!(root
                .path()
                .join("clips")
                .join(id.to_string())
                .join("short.mp4")
                .exists());
        }
    }

    #[test]
    fn cleanup_preflights_every_artifact_before_removing_any() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("clips").join("1");
        fs::create_dir_all(folder.join("metadata.json")).unwrap();
        let source = folder.join("source.mkv");
        let output = folder.join("short.mp4");
        fs::write(&source, b"source").unwrap();
        fs::write(&output, b"output").unwrap();
        assert_eq!(
            remove_outputs(&folder, 1, &source, &output)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(source.exists());
        assert!(output.exists());
    }

    #[test]
    fn requires_confirmed_public_processed_matching_video() {
        let mut item = serde_json::json!({"id":"a","snippet":{"channelId":"b"},"status":{"privacyStatus":"public","uploadStatus":"processed"}});
        assert!(public_and_processed(&item, "a", "b"));
        assert!(!public_and_processed(&item, "a", "other"));
        item["status"]["privacyStatus"] = "private".into();
        assert!(!public_and_processed(&item, "a", "b"));
        item["status"]["privacyStatus"] = "public".into();
        item["status"]["uploadStatus"] = "uploaded".into();
        assert!(!public_and_processed(&item, "a", "b"));
    }
    #[test]
    fn migration_queues_only_existing_uploads_with_a_known_channel_and_work_dir() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("legacy.db");
        let db = Database::open(&path).unwrap();
        let conn = db.connection().unwrap();
        conn.execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at) VALUES(1,1,1);
             INSERT INTO highlight_events(id,session_id,triggered_at,score) VALUES(1,1,1,90),(2,1,2,90),(3,1,3,90);
             INSERT INTO clip_jobs(id,highlight_id,status,work_dir)
               VALUES(1,1,'ready','fixture'),(2,2,'ready','fixture'),(3,3,'published',NULL);
             INSERT INTO publishing_jobs(clip_id,status,channel_id)
               VALUES(1,'uploaded','channel'),(2,'uploaded',NULL),(3,'uploaded','channel');
             INSERT INTO uploaded_videos(clip_id,video_id,title,published_at)
               VALUES(1,'first','one',1),(2,'second','two',1),(3,'third','three',1);
             DROP TABLE clip_review;
             DROP TABLE youtube_channel_cooldowns;
             ALTER TABLE clip_jobs DROP COLUMN editorial_pass;
             ALTER TABLE clip_jobs DROP COLUMN editorial_notes;
             ALTER TABLE clip_jobs DROP COLUMN editor_version;
             PRAGMA user_version=6;"
        ).unwrap();
        drop(conn);
        let reopened = Database::open(&path).unwrap();
        let (count, id): (u32, i64) = reopened
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*),min(clip_id) FROM publication_cleanup",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, id), (1, 1));
    }

    #[test]
    fn removal_is_repeatable_and_preserves_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("clips/1");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("short.mp4"), b"fixture").unwrap();
        fs::write(folder.join("notes.txt"), b"keep").unwrap();
        let source = folder.join("source.mkv");
        let output = folder.join("short.mp4");
        assert!(remove_outputs(&folder, 2, &source, &output).is_err());
        assert!(remove_outputs(&folder, 1, &source, &folder.join("notes.txt")).is_err());
        assert!(remove_outputs(&folder, 1, &root.path().join("source.mkv"), &output).is_err());
        assert!(output.exists());
        remove_outputs(&folder, 1, &source, &output).unwrap();
        remove_outputs(&folder, 1, &source, &output).unwrap();
        assert!(!folder.join("short.mp4").exists());
        assert!(folder.join("notes.txt").exists());
    }
}
