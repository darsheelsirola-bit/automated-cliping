use crate::{db::Database, runtime::Runtime};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tauri::{AppHandle, Manager};
use tokio::{process::Command, sync::Mutex};

#[derive(Default)]
pub struct ModelSetup(Mutex<()>);

const MODEL_BYTES: u64 = 3_087_284_237;
const MODEL_FOLDER: &str = "models--Systran--faster-whisper-large-v3";
const MODEL_REVISION: &str = "edaa852ec7e145841d8ffdb056a99866b5f0a478";
const BUDGET_WAIT: &str =
    "Saved clip budget reached. Free saved clip space or increase the budget in Settings.";

#[tauri::command]
pub async fn setup_model(
    app: AppHandle,
    state: tauri::State<'_, ModelSetup>,
) -> Result<(), String> {
    let _guard = state
        .0
        .try_lock()
        .map_err(|_| "Whisper setup is already running.")?;
    let script = script_path(&app)?;
    let cache = model_cache(&app)?;
    tokio::fs::create_dir_all(&cache)
        .await
        .map_err(|error| error.to_string())?;
    let runtime = runtime(&app)?;
    runtime
        .setup(
            &script.with_file_name("requirements.txt"),
            &cache.join("dependencies.log"),
        )
        .await?;
    run_worker(
        &runtime,
        &script,
        "setup",
        &cache,
        &cache.join("setup.log"),
        7_200,
    )
    .await?;
    tokio::fs::write(cache.join("ready"), b"Whisper Large-v3")
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn model_ready(app: AppHandle) -> Result<bool, String> {
    Ok(model_files_available(&model_cache(&app)?))
}

fn model_files_available(cache: &Path) -> bool {
    let folder = cache
        .join(MODEL_FOLDER)
        .join("snapshots")
        .join(MODEL_REVISION);
    cache.join("ready").is_file()
        && folder
            .join("model.bin")
            .metadata()
            .is_ok_and(|model| model.len() == MODEL_BYTES)
        && folder.join("tokenizer.json").is_file()
        && folder.join("config.json").is_file()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProgress {
    ready: bool,
    weight_bytes: u64,
    total_weight_bytes: u64,
}

#[tauri::command]
pub fn model_progress(app: AppHandle) -> Result<ModelProgress, String> {
    let cache = model_cache(&app)?;
    let ready = model_files_available(&cache);
    let pattern = cache.join(MODEL_FOLDER);
    let partial = std::fs::read_dir(pattern.join("blobs"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .max()
        .unwrap_or_default();
    let weight = pattern
        .join("snapshots")
        .join(MODEL_REVISION)
        .join("model.bin");
    let weight_bytes = weight
        .metadata()
        .map(|value| value.len())
        .unwrap_or(partial)
        .min(MODEL_BYTES);
    Ok(ModelProgress {
        ready,
        weight_bytes,
        total_weight_bytes: MODEL_BYTES,
    })
}

fn script_path(app: &AppHandle) -> Result<PathBuf, String> {
    let packaged = app
        .path()
        .resource_dir()
        .map_err(|error| error.to_string())?
        .join("worker")
        .join("media.py");
    if packaged.is_file() {
        return Ok(packaged);
    }
    #[cfg(debug_assertions)]
    {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("worker")
            .join("media.py");
        if source.is_file() {
            return Ok(source);
        }
    }
    Err("ClipForge's media worker is missing. Reinstall the application.".into())
}

pub fn runtime(app: &AppHandle) -> Result<Runtime, String> {
    Ok(Runtime::new(
        app.path()
            .app_local_data_dir()
            .map_err(|error| error.to_string())?,
    ))
}

fn model_cache(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?
        .join("models"))
}

async fn run_worker(
    runtime: &Runtime,
    script: &Path,
    action: &str,
    input: &Path,
    log: &Path,
    timeout: u64,
) -> Result<(), String> {
    let stderr = std::fs::File::create(log).map_err(|error| error.to_string())?;
    let stdout = stderr.try_clone().map_err(|error| error.to_string())?;
    let mut command = Command::new(runtime.python());
    command
        .kill_on_drop(true)
        .args(["-u"])
        .arg(script)
        .arg(action)
        .arg(input)
        .env("PATH", runtime.media_path()?)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Cannot run the ClipForge Python worker: {error}"))?;
    let status = tokio::time::timeout(Duration::from_secs(timeout), child.wait())
        .await
        .map_err(|_| format!("Media worker timed out. See {}.", log.display()))?
        .map_err(|error| error.to_string())?;
    if status.success() {
        return Ok(());
    }
    if action == "produce" && status.code() == Some(75) {
        return Err(BUDGET_WAIT.into());
    }
    Err(format!(
        "Media worker exited with {status}. See {}.",
        log.display()
    ))
}

#[derive(Serialize)]
struct Job {
    clip_id: i64,
    creator: String,
    stream_title: String,
    folder: PathBuf,
    model_cache: PathBuf,
    offset_ms: i64,
    min_seconds: u32,
    max_seconds: u32,
    remaining_saved_bytes: i64,
    segments: Vec<String>,
}

#[derive(Debug)]
struct Highlight {
    id: i64,
    session: i64,
    trigger_ms: i64,
    creator: String,
    title: String,
    score: f64,
}

#[derive(Deserialize)]
struct Output {
    reaction_strength: u32,
    reaction_range_ms: Option<(i64, i64)>,
    start_offset_ms: i64,
    duration_ms: i64,
    saved_bytes: i64,
    metadata: serde_json::Value,
}

#[derive(Default)]
pub(crate) struct RenderSlot(pub tokio::sync::Mutex<()>);

pub async fn supervise(app: AppHandle, db: Database) {
    loop {
        if let Err(error) = crate::recording::prune_expired(&db) {
            eprintln!("ClipForge cleanup: {error}");
        }
        if let Err(error) = reconcile_saved_sizes(&db) {
            eprintln!("ClipForge saved storage: {error}");
        }
        if let Err(error) = process_one(&app, &db).await {
            let logged = db.connection().and_then(|connection| {
                connection.execute(
                    "INSERT INTO error_logs(subsystem,message)
                 SELECT 'processing',?1 WHERE NOT EXISTS (
                   SELECT 1 FROM error_logs WHERE subsystem='processing' AND message=?1
                   AND occurred_at>unixepoch()-300
                 )",
                    [&error],
                )
            });
            if let Err(problem) = logged {
                eprintln!("ClipForge processing error: {error}; log error: {problem}");
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn process_one(app: &AppHandle, db: &Database) -> Result<(), String> {
    let cache = model_cache(app)?;
    if !model_files_available(&cache) {
        return Ok(());
    }
    let script = script_path(app)?;
    let settings = db.settings()?;
    let pending = next_job(db, settings.max_clip_seconds).map_err(|error| error.to_string())?;
    let Some(highlight) = pending else {
        return Ok(());
    };
    let budget = (settings.saved_budget_gb * 1_000_000_000.0) as i64;
    let start = highlight.trigger_ms - 20_000;
    let end = highlight.trigger_ms + i64::from(settings.max_clip_seconds) * 1000;
    let selected =
        segments(db, highlight.session, start, end).map_err(|error| error.to_string())?;
    let clip_id = create_job(db, highlight.id).map_err(|error| error.to_string())?;
    let Some((paths, offset, estimated_bytes)) =
        contiguous(selected, start, settings.min_clip_seconds.max(21))
    else {
        failure(
            db,
            clip_id,
            "The recording did not contain a continuous lead-in and reaction window.",
        )?;
        return Ok(());
    };
    let slot = app.state::<RenderSlot>();
    let _render = slot.0.lock().await;
    let (folder, already_saved) =
        work_folder(db, clip_id, PathBuf::from(settings.storage_directory))
            .map_err(|error| error.to_string())?;
    let available = budget.saturating_sub(
        saved_bytes(db)
            .map_err(|error| error.to_string())?
            .saturating_sub(already_saved),
    );
    let predicted_master = folder
        .join("source.mkv")
        .metadata()
        .map(|file| file.len().min(i64::MAX as u64) as i64)
        .unwrap_or(estimated_bytes);
    if predicted_master >= available {
        defer(db, clip_id, "The saved clip budget has insufficient space for this source; free space or raise the budget.")?;
        return Ok(());
    }
    let job = Job {
        clip_id,
        creator: highlight.creator.clone(),
        stream_title: highlight.title.clone(),
        folder,
        model_cache: cache,
        offset_ms: offset,
        min_seconds: settings.min_clip_seconds,
        max_seconds: settings.max_clip_seconds,
        remaining_saved_bytes: available,
        segments: paths,
    };
    let result = produce(
        db,
        &runtime(app)?,
        &script,
        &job,
        highlight,
        start,
        settings.minimum_confidence,
    )
    .await;
    if let Err(error) = result {
        if error == BUDGET_WAIT {
            defer(db, clip_id, &error)?;
        } else {
            retry_processing(db, clip_id, &error)?;
        }
    }
    Ok(())
}

fn work_folder(db: &Database, id: i64, root: PathBuf) -> rusqlite::Result<(PathBuf, i64)> {
    let folder = root.join("clips").join(id.to_string());
    db.connection()?.query_row(
        "UPDATE clip_jobs SET work_dir=coalesce(work_dir,?2)
        WHERE id=?1 RETURNING work_dir,saved_bytes",
        params![id, folder.to_string_lossy()],
        |row| Ok((PathBuf::from(row.get::<_, String>(0)?), row.get(1)?)),
    )
}

fn reconcile_saved_sizes(db: &Database) -> Result<(), String> {
    let connection = db.connection().map_err(|error| error.to_string())?;
    let mut query = connection
        .prepare(
            "SELECT id,work_dir,source_path,output_path,saved_bytes FROM clip_jobs
        WHERE work_dir IS NOT NULL OR source_path IS NOT NULL OR output_path IS NOT NULL",
        )
        .map_err(|error| error.to_string())?;
    let rows = query
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let entries = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    drop(query);
    for (id, folder, master, export, prior) in entries {
        let path = |explicit: Option<String>, name: &str| {
            explicit
                .map(PathBuf::from)
                .or_else(|| folder.as_ref().map(|work| Path::new(work).join(name)))
        };
        let size = |path: Option<PathBuf>| -> Result<i64, String> {
            let Some(path) = path else {
                return Ok(0);
            };
            match path.metadata() {
                Ok(metadata) => Ok(metadata.len().min(i64::MAX as u64) as i64),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
                Err(error) => Err(error.to_string()),
            }
        };
        let local_export = path(export.clone(), "short.mp4");
        let retained = |file: &str| local_export.as_ref().filter(|path| {
            path.file_name().is_some_and(|name| name != file && matches!(name.to_str(), Some("short-reviewed.mp4" | "short-reedited.mp4")))
        }).map(|path| path.with_file_name(file));
        let originals = size(retained("short.mp4"))?.saturating_add(
            if local_export.as_ref().is_some_and(|path| path.ends_with("short-reedited.mp4")) {
                size(retained("short-reviewed.mp4"))?
            } else { 0 }
        );
        let actual = size(path(master, "source.mkv"))?
            .saturating_add(size(local_export)?)
            .saturating_add(originals);
        if prior != actual {
            connection
                .execute(
                    "UPDATE clip_jobs SET saved_bytes=?2 WHERE id=?1 AND saved_bytes=?3 AND output_path IS ?4",
                    params![id, actual, prior, export],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn defer(db: &Database, id: i64, error: &str) -> Result<(), String> {
    db.connection()
        .and_then(|connection| {
            connection.execute(
                "UPDATE clip_jobs SET status='retry',retry_at=unixepoch()+60,error=?2 WHERE id=?1",
                params![id, error],
            )
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn retry_processing(db: &Database, id: i64, error: &str) -> Result<(), String> {
    let mut connection = db.connection().map_err(|error| error.to_string())?;
    let tx = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    tx.execute(
        "UPDATE clip_jobs SET attempts=attempts+1,
        status=CASE WHEN attempts<2 THEN 'retry' ELSE 'failed' END,
        retry_at=unixepoch()+60*(attempts+1),error=?2,updated_at=unixepoch() WHERE id=?1",
        params![id, error],
    )
    .map_err(|error| error.to_string())?;
    tx.execute("UPDATE transcription_jobs SET status='failed',error=?2 WHERE clip_id=?1 AND status='processing'",
        params![id,error]).map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

fn next_job(db: &Database, max_seconds: u32) -> rusqlite::Result<Option<Highlight>> {
    db.connection()?.query_row(
        "SELECT h.id,h.session_id,coalesce(h.triggered_at_ms,h.triggered_at*1000),c.username,coalesce(s.title,''),h.score
         FROM highlight_events h JOIN stream_sessions s ON s.id=h.session_id JOIN creators c ON c.id=s.creator_id
         LEFT JOIN clip_jobs j ON j.highlight_id=h.id
         WHERE (j.id IS NULL OR j.status IN ('queued','processing','retry'))
           AND (j.retry_at IS NULL OR j.retry_at <= unixepoch())
           AND (s.ended_at < unixepoch()-8 OR h.triggered_at + ?1 + 40 < unixepoch() OR EXISTS (
             SELECT 1 FROM recording_segments part WHERE part.session_id=h.session_id
             AND part.ends_at_ms >= coalesce(h.triggered_at_ms,h.triggered_at*1000) + ?1*1000
           ))
         ORDER BY h.triggered_at,h.id LIMIT 1",
        [max_seconds], |row| Ok(Highlight {
            id: row.get(0)?, session: row.get(1)?, trigger_ms: row.get(2)?, creator: row.get(3)?,
            title: row.get(4)?, score: row.get(5)?,
        })
    ).optional()
}

fn saved_bytes(db: &Database) -> rusqlite::Result<i64> {
    db.connection()?.query_row(
        "SELECT coalesce(sum(saved_bytes),0) FROM clip_jobs",
        [],
        |row| row.get(0),
    )
}

struct Part {
    path: String,
    start: i64,
    end: i64,
    bytes: i64,
}

fn segments(db: &Database, session: i64, start: i64, end: i64) -> rusqlite::Result<Vec<Part>> {
    let connection = db.connection()?;
    let mut query = connection.prepare(
        "SELECT path,starts_at_ms,ends_at_ms,bytes FROM recording_segments
        WHERE session_id=?1 AND ends_at_ms>?2 AND starts_at_ms<?3 ORDER BY starts_at_ms,id",
    )?;
    let rows = query.query_map(params![session, start, end], |row| {
        Ok(Part {
            path: row.get(0)?,
            start: row.get(1)?,
            end: row.get(2)?,
            bytes: row.get(3)?,
        })
    })?;
    rows.collect()
}

fn contiguous(parts: Vec<Part>, requested: i64, minimum: u32) -> Option<(Vec<String>, i64, i64)> {
    let first = parts.first()?;
    if first.start > requested {
        return None;
    }
    let offset = requested - first.start;
    let mut bytes: i64 = 0;
    let mut paths = Vec::new();
    let mut previous_end = first.start;
    let base = Path::new(&first.path).parent()?.to_owned();
    for part in parts {
        if (part.start - previous_end).abs() > 350
            || Path::new(&part.path).parent() != Some(base.as_path())
        {
            break;
        }
        previous_end = part.end;
        bytes = bytes.saturating_add(part.bytes);
        paths.push(part.path);
    }
    (previous_end - requested >= i64::from(minimum) * 1000).then_some((paths, offset, bytes))
}

fn create_job(db: &Database, highlight: i64) -> rusqlite::Result<i64> {
    db.connection()?.query_row("INSERT INTO clip_jobs(highlight_id,status) VALUES(?1,'processing')
        ON CONFLICT(highlight_id) DO UPDATE SET status='processing',updated_at=unixepoch(),error=NULL,retry_at=NULL
        RETURNING id", [highlight], |row| row.get(0))
}

fn failure(db: &Database, clip_id: i64, error: &str) -> Result<(), String> {
    let mut connection = db.connection().map_err(|error| error.to_string())?;
    let tx = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    tx.execute(
        "UPDATE clip_jobs SET status='failed',error=?2,updated_at=unixepoch() WHERE id=?1",
        params![clip_id, error],
    )
    .map_err(|error| error.to_string())?;
    tx.execute("UPDATE transcription_jobs SET status='failed',error=?2 WHERE clip_id=?1 AND status='processing'",
        params![clip_id,error]).map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

async fn produce(
    db: &Database,
    runtime: &Runtime,
    script: &Path,
    job: &Job,
    highlight: Highlight,
    start: i64,
    minimum: u32,
) -> Result<(), String> {
    tokio::fs::create_dir_all(&job.folder)
        .await
        .map_err(|error| error.to_string())?;
    let request = job.folder.join("job.json");
    let json = serde_json::to_vec(job).map_err(|error| error.to_string())?;
    tokio::fs::write(&request, json)
        .await
        .map_err(|error| error.to_string())?;
    db.connection()
        .and_then(|connection| {
            connection.execute(
                "INSERT INTO transcription_jobs(clip_id,status) VALUES(?1,'processing')
         ON CONFLICT(clip_id) DO UPDATE SET status='processing',error=NULL",
                [job.clip_id],
            )
        })
        .map_err(|error| error.to_string())?;
    run_worker(
        runtime,
        script,
        "produce",
        &request,
        &job.folder.join("processing.log"),
        7_200,
    )
    .await?;
    let result = tokio::fs::read(job.folder.join("result.json"))
        .await
        .map_err(|error| error.to_string())?;
    let result: Output = serde_json::from_slice(&result).map_err(|error| error.to_string())?;

    let mut connection = db.connection().map_err(|error| error.to_string())?;
    let tx = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let source = job.folder.join("source.mkv").to_string_lossy().into_owned();
    let short = job.folder.join("short.mp4").to_string_lossy().into_owned();
    let thumbnail = job
        .folder
        .join("thumbnail.jpg")
        .to_string_lossy()
        .into_owned();
    if !contains_selected_event(&result, highlight.trigger_ms - start) {
        return Err("Selected clip does not contain its verified reaction or fallback trigger.".into());
    }
    let start = start + result.start_offset_ms;
    let corroborated = result.metadata["quality_version"].as_u64() == Some(2);
    let confidence = publication_confidence(highlight.score, result.reaction_strength, corroborated);
    let editorial_pass = result.metadata["editorial_pass"].as_bool().unwrap_or(false);
    let editorial_notes = result.metadata["editorial_reasons"].as_array()
        .map(|items| items.iter().filter_map(|item| item.as_str()).collect::<Vec<_>>().join(" · "))
        .unwrap_or_default();
    let metadata = serde_json::to_vec(&result.metadata).map_err(|error| error.to_string())?;
    std::fs::write(job.folder.join("metadata.json"), metadata).map_err(|error| error.to_string())?;
    tx.execute(
        "UPDATE clip_jobs SET status='ready', source_path=?2,output_path=?3,
        thumbnail_path=?4,duration_ms=?5,saved_bytes=?6,editorial_pass=?7,editorial_notes=?8,editor_version=1,updated_at=unixepoch() WHERE id=?1",
        params![
            job.clip_id,
            source,
            short,
            thumbnail,
            result.duration_ms,
            result.saved_bytes,
            editorial_pass,
            editorial_notes,
        ],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        "UPDATE highlight_events SET confidence=?2,starts_at=?3,ends_at=?4 WHERE id=?1",
        params![
            highlight.id,
            confidence,
            start / 1000,
            (start + result.duration_ms) / 1000
        ],
    )
    .map_err(|error| error.to_string())?;
    tx.execute("UPDATE transcription_jobs SET status='complete', transcript_path=?2,srt_path=?3,ass_path=?4 WHERE clip_id=?1",
        params![job.clip_id,job.folder.join("transcript.json").to_string_lossy(),
            job.folder.join("captions.srt").to_string_lossy(),job.folder.join("captions.ass").to_string_lossy()])
        .map_err(|error| error.to_string())?;
    tx.execute(
        "INSERT INTO render_jobs(clip_id,status,output_path) VALUES(?1,'complete',?2)
        ON CONFLICT(clip_id) DO UPDATE SET status='complete',output_path=excluded.output_path",
        params![job.clip_id, short],
    )
    .map_err(|error| error.to_string())?;
    if confidence >= f64::from(minimum.max(80)) && corroborated && editorial_pass {
        tx.execute("INSERT INTO publishing_jobs(clip_id,status) VALUES(?1,'queued') ON CONFLICT DO NOTHING", [job.clip_id])
            .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn reedit_clip(
    app: AppHandle,
    database: tauri::State<'_, Database>,
    id: i64,
) -> Result<(), String> {
    let db = database.inner().clone();
    let slot = app.state::<RenderSlot>();
    let _render = slot.0.try_lock().map_err(|_| "A reel is already rendering. Try again once it finishes.")?;
    let runtime = runtime(&app)?;
    let script = script_path(&app)?;
    let cache = model_cache(&app)?;
    if !model_files_available(&cache) {
        return Err("Install the transcription model in Settings before re-editing.".into());
    }
    let error = |err: rusqlite::Error| err.to_string();
    let query = format!(
        "SELECT j.output_path,j.work_dir,j.source_path,h.id,h.score FROM clip_jobs j
        JOIN highlight_events h ON h.id=j.highlight_id WHERE {} AND j.editor_version=0",
        crate::review::MUTABLE_REEL
    );
    let (output, folder, source, highlight, live_score): (String, String, String, i64, f64) =
        db.connection().map_err(error)?.query_row(&query, [id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        }).map_err(|_| "This local reel is unavailable for re-editing.")?;
    crate::review::export_path(id, &output, &folder)?;
    let folder = PathBuf::from(folder);
    let target = folder.join("short-reedited.mp4");
    if Path::new(&output) == target {
        return Err("This reel already uses the new editor.".into());
    }
    let source = PathBuf::from(source);
    if source != folder.join("source.mkv") || !std::fs::symlink_metadata(&source)
        .map_err(|_| "The original recording is unavailable.")?.file_type().is_file() {
        return Err("The original recording is unavailable.".into());
    }
    let mut job: serde_json::Value = serde_json::from_slice(&std::fs::read(folder.join("job.json"))
        .map_err(|_| "This reel has no saved edit request.")?).map_err(|err| err.to_string())?;
    let staging = tempfile::Builder::new().prefix(".reedit-").tempdir_in(&folder)
        .map_err(|err| err.to_string())?;
    std::fs::hard_link(&source, staging.path().join("source.mkv"))
        .map_err(|err| format!("Could not stage the original recording. {err}"))?;
    let budget = (db.settings()?.saved_budget_gb * 1_000_000_000.0) as i64;
    let source_size = std::fs::metadata(&source).map_err(|err| err.to_string())?.len() as i64;
    let available = budget.saturating_sub(saved_bytes(&db).map_err(error)?);
    if available <= 0 {
        return Err("There is no space for a revised reel within your clip storage limit.".into());
    }
    job["model_cache"] = serde_json::json!(cache);
    job["folder"] = serde_json::json!(staging.path());
    job["remaining_saved_bytes"] = serde_json::json!(available.saturating_add(source_size));
    job["segments"] = serde_json::json!([]);
    let request = staging.path().join("job.json");
    std::fs::write(&request, serde_json::to_vec(&job).map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;
    crate::review::set_clip_review(database, id, false)?;

    run_worker(&runtime, &script, "produce", &request, &staging.path().join("processing.log"), 7200).await?;
    let result_bytes = std::fs::read(staging.path().join("result.json")).map_err(|err| err.to_string())?;
    let result: Output = serde_json::from_slice(&result_bytes).map_err(|err| err.to_string())?;
    if !contains_selected_event(&result, 20_000) {
        return Err("The re-edit could not retain the selected reaction.".into());
    }
    let candidate = staging.path().join("short.mp4");
    let bytes = std::fs::metadata(&candidate).map_err(|err| err.to_string())?.len() as i64;
    let corroborated = result.metadata["quality_version"].as_u64() == Some(2);
    let confidence = publication_confidence(live_score, result.reaction_strength, corroborated);
    let editorial_pass = result.metadata["editorial_pass"].as_bool().unwrap_or(false);
    let mut conn = db.connection().map_err(error)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(error)?;
    let current: String = tx.query_row(&format!(
        "SELECT j.output_path FROM clip_jobs j JOIN clip_review r ON r.clip_id=j.id
        WHERE {} AND r.decision='hold'", crate::review::MUTABLE_REEL), [id], |row| row.get(0))
        .map_err(|_| "This reel changed while the re-edit was rendering.")?;
    if current != output {
        return Err("This reel changed while the re-edit was rendering.".into());
    }
    let used: i64 = tx.query_row("SELECT coalesce(sum(saved_bytes),0) FROM clip_jobs", [], |row| row.get(0)).map_err(error)?;
    let budget = (db.settings_with(&tx)?.saved_budget_gb * 1_000_000_000.0) as i64;
    if used.saturating_add(bytes) > budget {
        return Err("The re-edit would exceed your saved clip storage limit.".into());
    }

    let metadata_path = crate::youtube::metadata_path(&target)?;
    std::fs::write(metadata_path, serde_json::to_vec(&result.metadata).map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;
    // A failed commit leaves the original assets selected. Only verified staged assets become active.
    let promote = |input: &str, output: &str| -> Result<PathBuf, String> {
        let path = folder.join(output);
        std::fs::rename(staging.path().join(input), &path).map_err(|err| err.to_string())?;
        Ok(path)
    };
    let thumbnail = promote("thumbnail.jpg", "thumbnail-reedited.jpg")?;
    let transcript = promote("transcript.json", "transcript-reedited.json")?;
    let srt = promote("captions.srt", "captions-reedited.srt")?;
    let ass = promote("captions.ass", "captions-reedited.ass")?;
    std::fs::rename(candidate, &target).map_err(|err| err.to_string())?;
    let notes = result.metadata["editorial_reasons"].as_array()
        .map(|items| items.iter().filter_map(|item| item.as_str()).collect::<Vec<_>>().join(" · "));
    tx.execute("UPDATE clip_jobs SET output_path=?2,duration_ms=?3,saved_bytes=saved_bytes+?4,
        editorial_pass=?5,editorial_notes=?6,thumbnail_path=?7,editor_version=1,updated_at=unixepoch() WHERE id=?1",
        params![id, target.to_string_lossy(), result.duration_ms, bytes, editorial_pass, notes, thumbnail.to_string_lossy()]).map_err(error)?;
    tx.execute("UPDATE transcription_jobs SET transcript_path=?2,srt_path=?3,ass_path=?4 WHERE clip_id=?1",
        params![id, transcript.to_string_lossy(), srt.to_string_lossy(), ass.to_string_lossy()]).map_err(error)?;
    tx.execute("UPDATE render_jobs SET output_path=?2 WHERE clip_id=?1",
        params![id, target.to_string_lossy()]).map_err(error)?;
    tx.execute("UPDATE highlight_events SET confidence=?2,
        starts_at=(triggered_at_ms-20000+?3)/1000,
        ends_at=(triggered_at_ms-20000+?3+?4)/1000 WHERE id=?1",
        params![highlight, confidence, result.start_offset_ms, result.duration_ms]).map_err(error)?;
    tx.commit().map_err(error)
}

fn contains_selected_event(result: &Output, trigger: i64) -> bool {
    let start = result.start_offset_ms;
    let Some(end) = start.checked_add(result.duration_ms) else {
        return false;
    };
    if start < 0 || end <= start {
        return false;
    }
    match result.reaction_range_ms {
        Some((onset, release)) => {
            result.reaction_strength > 0
                && onset >= trigger.saturating_sub(17_000)
                && onset <= trigger.saturating_add(2_000)
                && start <= onset
                && onset < release
                && release < end
        }
        None => result.reaction_strength == 0 && start <= trigger && trigger < end,
    }
}

fn publication_confidence(live_score: f64, reaction_strength: u32, corroborated: bool) -> f64 {
    let live = live_score.clamp(0.0, 100.0);
    let reaction = f64::from(reaction_strength.min(100));
    if reaction == 0.0 || !corroborated {
        return (live * 0.55).round();
    }
    (0.35 * live + 0.65 * reaction).round()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_reaction_can_exclude_delayed_chat_trigger() {
        let mut result = Output {
            reaction_strength: 90,
            reaction_range_ms: Some((8_000, 10_000)),
            start_offset_ms: 2_000,
            duration_ms: 10_000,
            saved_bytes: 0,
            metadata: serde_json::Value::Null,
        };
        assert!(contains_selected_event(&result, 20_000));
        result.reaction_range_ms = Some((1_999, 10_000));
        assert!(!contains_selected_event(&result, 20_000));
        result.reaction_range_ms = Some((8_000, 12_000));
        assert!(!contains_selected_event(&result, 20_000));
        result.reaction_strength = 0;
        result.reaction_range_ms = None;
        assert!(!contains_selected_event(&result, 20_000));
        assert!(contains_selected_event(&result, 10_000));
    }

    #[test]
    fn publishing_confidence_needs_a_reaction_and_strong_live_evidence() {
        assert!(publication_confidence(100.0, 0, true) < 80.0);
        assert!(publication_confidence(100.0, 100, false) <= 55.0);
        assert!(publication_confidence(30.0, 100, true) < 80.0);
        assert!(publication_confidence(80.0, 90, true) >= 80.0);
        assert!(publication_confidence(100.0, 77, true) < 90.0);
    }

    #[test]
    fn rejects_missing_lead_and_disconnected_media() {
        let part = |file: &str, start, end| Part {
            path: file.into(),
            start,
            end,
            bytes: 100,
        };
        assert!(contiguous(vec![part("a/1.ts", 500, 6000)], 0, 5).is_none());
        assert!(contiguous(
            vec![part("a/1.ts", 0, 5000), part("b/2.ts", 5000, 10000)],
            1000,
            7
        )
        .is_none());
        assert_eq!(
            contiguous(
                vec![part("a/1.ts", 0, 5000), part("a/2.ts", 5000, 10000)],
                1000,
                7
            ),
            Some((vec!["a/1.ts".into(), "a/2.ts".into()], 1000, 200))
        );
    }

    #[test]
    fn recovers_an_interrupted_job_and_persists_retry_delay() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("jobs.db");
        let db = Database::open(&path).unwrap();
        db.connection().unwrap().execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at,ended_at) VALUES(1,1,1,100);
             INSERT INTO highlight_events(id,session_id,triggered_at,triggered_at_ms,score)
             VALUES(1,1,60,60500,82);"
        ).unwrap();
        let id = create_job(&db, 1).unwrap();
        let resumed = Database::open(&path).unwrap();
        let recovered = next_job(&resumed, 60).unwrap().unwrap();
        assert_eq!(recovered.trigger_ms, 60_500);
        assert_eq!(create_job(&resumed, recovered.id).unwrap(), id);
        retry_processing(&resumed, id, "transient").unwrap();
        assert!(next_job(&resumed, 60).unwrap().is_none());
        resumed
            .connection()
            .unwrap()
            .execute("UPDATE clip_jobs SET retry_at=0", [])
            .unwrap();
        assert_eq!(next_job(&resumed, 60).unwrap().unwrap().id, recovered.id);
    }

    #[test]
    fn accounts_for_a_retained_master_without_consuming_a_budget_wait_attempt() {
        let folder = tempfile::tempdir().unwrap();
        let db = Database::open(folder.path().join("budget.db")).unwrap();
        let connection = db.connection().unwrap();
        connection.execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at) VALUES(1,1,1);
             INSERT INTO highlight_events(id,session_id,triggered_at,score) VALUES(1,1,60,82);"
        ).unwrap();
        let id = create_job(&db, 1).unwrap();
        let (work, _) = work_folder(&db, id, folder.path().into()).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let master = work.join("source.mkv");
        std::fs::write(&master, b"saved source").unwrap();
        defer(&db, id, BUDGET_WAIT).unwrap();
        reconcile_saved_sizes(&db).unwrap();
        assert_eq!(saved_bytes(&db).unwrap(), 12);
        let attempts: i64 = connection
            .query_row(
                "SELECT attempts FROM clip_jobs WHERE id=?1 AND status='retry'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 0);
        std::fs::remove_file(master).unwrap();
        reconcile_saved_sizes(&db).unwrap();
        assert_eq!(saved_bytes(&db).unwrap(), 0);
        let original = work.join("short.mp4");
        let trimmed = work.join("short-reviewed.mp4");
        std::fs::write(original, b"original").unwrap();
        std::fs::write(&trimmed, b"trim").unwrap();
        connection.execute("UPDATE clip_jobs SET output_path=?2 WHERE id=?1",
            params![id, trimmed.to_string_lossy()]).unwrap();
        reconcile_saved_sizes(&db).unwrap();
        assert_eq!(saved_bytes(&db).unwrap(), 12);
    }
}
