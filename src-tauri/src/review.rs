use crate::{db::Database, production};
use rusqlite::{params, TransactionBehavior};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};

fn message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(crate) fn export_path(id: i64, output: &str, folder: &str) -> Result<PathBuf, String> {
    let path = Path::new(output);
    let folder = Path::new(folder);
    let name = path.file_name().and_then(|name| name.to_str());
    let root = folder
        .parent()
        .and_then(Path::parent)
        .ok_or("Missing clip storage root.")?;
    if !folder.is_absolute()
        || folder.file_name().and_then(|name| name.to_str()) != Some(&id.to_string())
        || folder
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("clips")
        || path.parent() != Some(folder)
        || !matches!(name, Some("short.mp4" | "short-reviewed.mp4" | "short-reedited.mp4"))
        || std::fs::symlink_metadata(path)
            .map_err(message)?
            .file_type()
            .is_symlink()
    {
        return Err("The local reel path could not be verified.".into());
    }
    let canonical = path.canonicalize().map_err(message)?;
    if canonical
        != root
            .canonicalize()
            .map_err(message)?
            .join("clips")
            .join(id.to_string())
            .join(name.unwrap())
    {
        return Err("The local reel path could not be verified.".into());
    }
    Ok(canonical)
}

#[tauri::command]
pub fn preview_clip(
    app: AppHandle,
    database: State<'_, Database>,
    id: i64,
) -> Result<String, String> {
    let (output, folder): (String, String) = database
        .connection()
        .map_err(message)?
        .query_row(
            "SELECT output_path,work_dir FROM clip_jobs WHERE id=?1 AND output_path IS NOT NULL",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(message)?;
    let canonical = export_path(id, &output, &folder)?;
    app.asset_protocol_scope()
        .allow_file(&canonical)
        .map_err(message)?;
    // Tauri's Windows asset URLs expect drive paths without the Win32 verbatim prefix.
    let path = Path::new(&output).to_string_lossy();
    app.asset_protocol_scope()
        .allow_file(Path::new(&output))
        .map_err(message)?;
    Ok(path.into_owned())
}

pub(crate) const MUTABLE_REEL: &str = "j.id=?1 AND j.status='ready' AND j.output_path IS NOT NULL
    AND NOT EXISTS(SELECT 1 FROM uploaded_videos u WHERE u.clip_id=j.id)
    AND NOT EXISTS(SELECT 1 FROM publishing_jobs q WHERE q.clip_id=j.id AND
      (q.status IN ('uploading','uploaded','uncertain') OR q.upload_session IS NOT NULL OR q.may_have_committed))";

#[tauri::command]
pub fn set_clip_review(
    database: State<'_, Database>,
    id: i64,
    approved: bool,
) -> Result<(), String> {
    let mut connection = database.connection().map_err(message)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(message)?;
    let eligible = tx
        .query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM clip_jobs j WHERE {MUTABLE_REEL})"),
            [id],
            |row| row.get::<_, bool>(0),
        )
        .map_err(message)?;
    if !eligible {
        return Err("This reel is unavailable or an upload has already started.".into());
    }
    if approved {
        let (output, editorial_pass): (String, Option<bool>) = tx
            .query_row(
                "SELECT output_path,editorial_pass FROM clip_jobs WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(message)?;
        if editorial_pass != Some(true) {
            return Err("This reel needs a passing automatic editorial check before it can publish.".into());
        }
        let path = crate::youtube::metadata_path(Path::new(&output))?;
        let metadata: serde_json::Value = serde_json::from_slice(
            &std::fs::read(path).map_err(|_| "This export needs a fresh quality check.")?,
        )
        .map_err(message)?;
        let confidence: Option<f64> = tx.query_row("SELECT h.confidence FROM clip_jobs j JOIN highlight_events h ON j.highlight_id=h.id WHERE j.id=?1", [id], |row| row.get(0)).map_err(message)?;
        if metadata["quality_version"].as_u64() != Some(2)
            || !confidence.is_some_and(|score| score >= 80.0)
        {
            return Err("This reel did not pass the reaction quality check.".into());
        }
        tx.execute("INSERT INTO publishing_jobs(clip_id,status) VALUES(?1,'queued')
            ON CONFLICT(clip_id) DO UPDATE SET status='queued', retry_at=NULL, error=NULL, attempts=0",
            [id]).map_err(message)?;
    }
    tx.execute(
        "INSERT INTO clip_review(clip_id,decision) VALUES(?1,?2)
        ON CONFLICT(clip_id) DO UPDATE SET decision=excluded.decision",
        params![id, if approved { "approved" } else { "hold" }],
    )
    .map_err(message)?;
    tx.commit().map_err(message)
}

fn held_export(conn: &rusqlite::Connection, id: i64) -> Result<(String, String, i64), String> {
    conn.query_row(
        &format!("SELECT j.output_path,j.work_dir,j.duration_ms FROM clip_jobs j JOIN clip_review v ON v.clip_id=j.id WHERE {MUTABLE_REEL} AND v.decision='hold'"),
        [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).map_err(|_| "Hold this reel before applying a trim.".into())
}

#[tauri::command]
pub async fn trim_clip(
    app: AppHandle,
    database: State<'_, Database>,
    id: i64,
    start_ms: i64,
    end_ms: i64,
) -> Result<(), String> {
    let slot = app.state::<production::RenderSlot>();
    let _render = slot
        .0
        .try_lock()
        .map_err(|_| "A reel is already rendering. Try saving the trim once it finishes.")?;
    let db = database.inner().clone();
    let ffmpeg = production::runtime(&app)?.ffmpeg("ffmpeg");
    tauri::async_runtime::spawn_blocking(move || {
        let mut conn = db.connection().map_err(message)?;
        let (output, folder, duration) = held_export(&conn, id)?;
        if start_ms < 0 || end_ms > duration || end_ms.saturating_sub(start_ms) < 1000
            || (start_ms == 0 && end_ms == duration)
        {
            return Err("Choose a shorter range of at least one second.".into());
        }
        let input = export_path(id, &output, &folder)?;
        if input.file_name().and_then(|name| name.to_str()) != Some("short.mp4") {
            return Err("A trim has already been applied to this reel.".into());
        }
        let result = std::fs::read(Path::new(&folder).join("result.json")).map_err(message)?;
        let result: serde_json::Value = serde_json::from_slice(&result).map_err(message)?;
        if let Some(([onset, release], original_start)) = result["reaction_range_ms"].as_array()
            .and_then(|range| Some([range.first()?.as_i64()?, range.get(1)?.as_i64()?]))
            .zip(result["start_offset_ms"].as_i64())
        {
            if start_ms > onset - original_start || end_ms < release - original_start {
                return Err("Keep the detected reaction inside the trim.".into());
            }
        }
        let temp = tempfile::Builder::new().suffix(".mp4").tempfile_in(&folder).map_err(message)?.into_temp_path();
        let mut command = std::process::Command::new(ffmpeg);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let result = command.args(["-v","error","-xerror","-y","-ss"])
            .arg(format!("{:.3}", start_ms as f64 / 1000.0)).arg("-i").arg(&input)
            .arg("-t").arg(format!("{:.3}", (end_ms-start_ms) as f64 / 1000.0))
            .args(["-map","0:v:0","-map","0:a:0","-c:v","libx264","-preset","fast","-crf","18",
                "-c:a","aac","-movflags","+faststart"]).arg(&temp).output().map_err(message)?;
        if !result.status.success() {
            return Err("FFmpeg could not save the trimmed reel.".into());
        }
        let bytes = std::fs::metadata(&temp).map_err(message)?.len();
        let target = Path::new(&folder).join("short-reviewed.mp4");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(message)?;
        if held_export(&tx, id)?.0 != output {
            return Err("This reel changed while the trim was rendering.".into());
        }
        let used: u64 = tx.query_row("SELECT coalesce(sum(saved_bytes),0) FROM clip_jobs", [], |row| row.get(0)).map_err(message)?;
        let budget = (db.settings_with(&tx)?.saved_budget_gb * 1_000_000_000.0) as u64;
        if used.saturating_add(bytes) > budget {
            return Err("The new export would exceed your saved clip storage limit.".into());
        }
        temp.persist(&target).map_err(message)?;
        tx.execute("UPDATE clip_jobs SET output_path=?2,duration_ms=?3,saved_bytes=saved_bytes+?4,editorial_pass=NULL,editorial_notes=NULL WHERE id=?1",
            params![id, target.to_string_lossy(), end_ms-start_ms, bytes]).map_err(message)?;
        tx.execute("UPDATE render_jobs SET output_path=?2 WHERE clip_id=?1", params![id, target.to_string_lossy()]).map_err(message)?;
        tx.commit().map_err(message)
    }).await.map_err(message)?
}
