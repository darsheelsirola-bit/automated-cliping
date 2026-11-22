use super::{account, token_request};
use crate::db::Database;
use reqwest::Client;
use rusqlite::{params, OptionalExtension};
use std::{fs, io, path::Path};

fn public_and_processed(video: &serde_json::Value, id: &str, channel: &str) -> bool {
    video["id"] == id
        && video["snippet"]["channelId"] == channel
        && video["status"]["privacyStatus"] == "public"
        && video["status"]["uploadStatus"] == "processed"
}

fn remove_outputs(folder: &Path, id: i64) -> io::Result<()> {
    let clips = folder.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let root = clips.parent().ok_or(io::ErrorKind::InvalidInput)?;
    if !root.is_absolute()
        || folder != root.join("clips").join(id.to_string())
        || folder.canonicalize()? != root.canonicalize()?.join("clips").join(id.to_string())
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    for name in [
        "source.mkv",
        "short.mp4",
        "thumbnail.jpg",
        "captions.ass",
        "captions.srt",
        "transcript.json",
        "metadata.json",
        "job.json",
        "result.json",
    ] {
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

pub(super) async fn check_one(
    db: &Database,
    client: &Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !db.settings()?.auto_publish {
        return Ok(());
    }
    let conn = db.connection()?;
    let job = conn.query_row("SELECT c.clip_id,u.video_id,p.channel_id,j.work_dir FROM publication_cleanup c
        JOIN uploaded_videos u USING(clip_id) JOIN publishing_jobs p USING(clip_id) JOIN clip_jobs j ON j.id=c.clip_id
        WHERE p.status='uploaded' AND j.work_dir IS NOT NULL AND p.channel_id IS NOT NULL AND c.check_after<=unixepoch()
        ORDER BY c.check_after,c.clip_id LIMIT 1", [], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()?;
    let Some((id, video, channel, folder)) = job else {
        return Ok(());
    };
    conn.execute(
        "UPDATE publication_cleanup SET check_after=unixepoch()+900 WHERE clip_id=?1",
        [id],
    )?;
    drop(conn);
    let Some(acct) = account()? else {
        return Ok(());
    };
    if acct.channel_id != channel {
        return Ok(());
    }
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
    if !db.settings()?.auto_publish {
        return Ok(());
    }
    remove_outputs(Path::new(&folder), id)?;
    let mut conn = db.connection()?;
    let tx = conn.transaction()?;
    tx.execute("UPDATE clip_jobs SET status='published',source_path=NULL,output_path=NULL,thumbnail_path=NULL,work_dir=NULL,saved_bytes=0 WHERE id=?1",params![id])?;
    tx.execute(
        "UPDATE render_jobs SET output_path=NULL WHERE clip_id=?1",
        [id],
    )?;
    tx.execute("UPDATE transcription_jobs SET transcript_path=NULL,srt_path=NULL,ass_path=NULL WHERE clip_id=?1",[id])?;
    tx.execute("DELETE FROM publication_cleanup WHERE clip_id=?1", [id])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn removal_is_repeatable_and_preserves_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("clips/1");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("short.mp4"), b"fixture").unwrap();
        fs::write(folder.join("notes.txt"), b"keep").unwrap();
        assert!(remove_outputs(&folder, 2).is_err());
        assert!(folder.join("short.mp4").exists());
        remove_outputs(&folder, 1).unwrap();
        remove_outputs(&folder, 1).unwrap();
        assert!(!folder.join("short.mp4").exists());
        assert!(folder.join("notes.txt").exists());
    }
}
