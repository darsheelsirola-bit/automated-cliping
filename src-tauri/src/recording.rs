use crate::{db::Database, runtime::Runtime};
use rusqlite::params;
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{fs, io, process::Command, sync::watch, time};

const RETENTION_MS: i64 = 90_000;

#[derive(Clone, Default)]
pub struct Signal {
    pub latest_segment_ms: Option<i64>,
    audio: VecDeque<(i64, f64)>,
    pub error: Option<String>,
}

impl Signal {
    fn record_audio(&mut self, event_ms: i64, strength: f64) {
        self.audio.retain(|(t, _)| *t >= event_ms - 15_000);
        if strength > 0.0 {
            self.audio.push_back((event_ms, strength));
        }
    }

    pub fn take_fresh_audio(&self, now: i64, last_read: &mut i64) -> Option<(i64, f64)> {
        let fresh: Vec<_> = self
            .audio
            .iter()
            .copied()
            .filter(|(t, _)| *t > *last_read && (now - 15_000..=now).contains(t))
            .collect();
        if let Some(latest) = fresh.iter().map(|(t, _)| *t).max() {
            *last_read = latest;
        }
        fresh.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))
    }
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Debug, PartialEq)]
struct Segment {
    filename: String,
    start_ms: i64,
    end_ms: i64,
}

fn read_segment(line: &str) -> Option<Segment> {
    let mut values = line.split(',');
    let filename = values.next()?.trim_matches('"');
    if !filename.starts_with("segment-")
        || !filename.ends_with(".ts")
        || filename.contains(['/', '\\'])
    {
        return None;
    }
    let timestamp = |value: &str| {
        let seconds = value.parse::<f64>().ok()?;
        (seconds.is_finite() && (0.0..=172_800.0).contains(&seconds))
            .then(|| (seconds * 1000.0).round() as i64)
    };
    let start_ms = timestamp(values.next()?)?;
    let end_ms = timestamp(values.next()?)?;
    (end_ms > start_ms && values.next().is_none()).then(|| Segment {
        filename: filename.into(),
        start_ms,
        end_ms,
    })
}

pub async fn record(
    db: Database,
    session: i64,
    username: String,
    runtime: Runtime,
    mut stop: watch::Receiver<bool>,
    signal: watch::Sender<Signal>,
) {
    let mut backoff = 2;
    loop {
        if *stop.borrow() {
            return;
        }
        let result = async {
            let settings = db.settings()?;
            if budget_full(&db, settings.saved_budget_gb)? {
                return Err("Recording paused because the saved clip budget is full.".into());
            }
            let root = PathBuf::from(settings.storage_directory);
            capture(&db, session, &username, &root, &runtime, &mut stop, &signal).await
        }
        .await;
        let stopping = *stop.borrow() || stop.has_changed().is_err();
        if stopping && result.is_ok() {
            return;
        }
        let message = result
            .err()
            .unwrap_or_else(|| "Stream stopped delivering video.".into());
        signal.send_replace(Signal {
            error: Some(message.clone()),
            ..Signal::default()
        });
        if let Err(error) = db.connection().and_then(|connection| {
            connection.execute(
                "INSERT INTO error_logs (subsystem, message) VALUES ('recording', ?1)",
                [format!("{username}: {message}")],
            )
        }) {
            eprintln!("ClipForge recording log: {error}");
        }
        if stopping {
            return;
        }
        tokio::select! {
            _ = stop.changed() => return,
            _ = time::sleep(Duration::from_secs(backoff)) => {}
        }
        backoff = (backoff * 2).min(90);
    }
}

fn budget_full(db: &Database, budget: f64) -> Result<bool, String> {
    let saved: i64 = db
        .connection()
        .and_then(|connection| {
            connection.query_row(
                "SELECT coalesce(sum(saved_bytes),0) FROM clip_jobs",
                [],
                |row| row.get(0),
            )
        })
        .map_err(|error| error.to_string())?;
    Ok(saved >= (budget * 1_000_000_000.0) as i64)
}

async fn capture(
    db: &Database,
    session: i64,
    username: &str,
    root: &Path,
    runtime: &Runtime,
    stop: &mut watch::Receiver<bool>,
    signal: &watch::Sender<Signal>,
) -> Result<(), String> {
    fs::create_dir_all(root)
        .await
        .map_err(|error| format!("Cannot create the storage directory: {error}"))?;
    let free = fs2::available_space(root)
        .map_err(|error| format!("Cannot inspect disk space: {error}"))?;
    if free < 1024 * 1024 * 1024 {
        return Err("Recording needs at least 1 GiB of free disk space.".into());
    }

    let folder = root
        .join("recordings")
        .join(session.to_string())
        .join(unix_ms().to_string());
    fs::create_dir_all(&folder)
        .await
        .map_err(|error| format!("Cannot create recording folder: {error}"))?;
    let log_path = folder.join("capture.log");
    let log = std::fs::File::create(log_path).map_err(|error| error.to_string())?;
    let ffmpeg_log = log.try_clone().map_err(|error| error.to_string())?;

    let mut stream_command = Command::new(runtime.python());
    #[cfg(windows)]
    stream_command.creation_flags(0x08000000);
    let mut stream = stream_command
        .kill_on_drop(true)
        .args([
            "-m",
            "streamlink",
            "--stdout",
            "--loglevel",
            "warning",
            "--stream-timeout",
            "30",
            &format!("https://www.twitch.tv/{username}"),
            "best",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|error| format!("Cannot start Streamlink: {error}"))?;
    let mut source = stream
        .stdout
        .take()
        .ok_or("Streamlink output is unavailable.")?;
    let mut ffmpeg_command = Command::new(runtime.ffmpeg("ffmpeg"));
    #[cfg(windows)]
    ffmpeg_command.creation_flags(0x08000000);
    let mut ffmpeg = ffmpeg_command
        .kill_on_drop(true)
        .current_dir(&folder)
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-i",
            "pipe:0",
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c",
            "copy",
            "-f",
            "segment",
            "-segment_time",
            "5",
            "-segment_list",
            "segments.csv",
            "-segment_list_type",
            "csv",
            "-reset_timestamps",
            "1",
            "segment-%08d.ts",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(ffmpeg_log))
        .spawn()
        .map_err(|error| format!("Cannot start FFmpeg: {error}"))?;
    let mut sink = ffmpeg.stdin.take().ok_or("FFmpeg input is unavailable.")?;
    let first_byte = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut bytes = [0; 8192];
        let count = source.read(&mut bytes).await?;
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Streamlink returned no video",
            ));
        }
        sink.write_all(&bytes[..count]).await?;
        Ok::<i64, std::io::Error>(unix_ms())
    };
    let anchor = tokio::select! {
        _ = stop.changed() => return Ok(()),
        result = time::timeout(Duration::from_secs(45), first_byte) => result
            .map_err(|_| "Timed out while waiting for Twitch video.")?
            .map_err(|error| format!("Twitch video input failed: {error}"))?
    };
    fs::write(folder.join("anchor-ms"), anchor.to_string())
        .await
        .map_err(|error| format!("Cannot save recording time origin: {error}"))?;
    let mut transfer = Box::pin(async move {
        let result = io::copy(&mut source, &mut sink).await;
        drop(source);
        drop(sink);
        result
    });
    let mut transfer_finished = false;
    let mut known = HashSet::new();
    let mut ticker = time::interval(Duration::from_secs(2));
    let mut last_segment = time::Instant::now();
    let mut levels = VecDeque::new();
    let mut audio_task: Option<(i64, tokio::task::JoinHandle<Option<Vec<AudioFrame>>>)> = None;

    loop {
        tokio::select! {
            _ = stop.changed() => {
                drop(transfer);
                let _ = stream.start_kill();
                time::timeout(Duration::from_secs(8), ffmpeg.wait()).await
                    .map_err(|_| "FFmpeg took too long to finish the current segment.")?
                    .map_err(|error| error.to_string())?;
                index_segments(db, session, &folder, anchor, &mut known, false).await?;
                return Ok(());
            }
            result = &mut transfer, if !transfer_finished => {
                transfer_finished = true;
                result.map_err(|error| format!("Cannot transfer Twitch video: {error}"))?;
            }
            result = ffmpeg.wait() => {
                index_segments(db, session, &folder, anchor, &mut known, false).await?;
                let status = result.map_err(|error| error.to_string())?;
                return Err(format!("FFmpeg stopped recording ({status}). See capture.log."));
            }
            _ = ticker.tick() => {
                if audio_task.as_ref().is_some_and(|(_, task)| task.is_finished()) {
                    let (segment_start, task) = audio_task.take().unwrap();
                    if let Some(profile) = task.await.ok().flatten() {
                        let (offset, strength) = reaction(&profile, &mut levels);
                        signal.send_modify(|value| value.record_audio(segment_start + offset, strength));
                    }
                }
                if let Some((part, segment_start)) = index_segments(db, session, &folder, anchor, &mut known, false).await? {
                    last_segment = time::Instant::now();
                    if audio_task.is_none() {
                        audio_task = Some((segment_start, tokio::spawn(audio_profile(runtime.ffmpeg("ffmpeg"), part))));
                    }
                    signal.send_modify(|value| {value.latest_segment_ms = Some(unix_ms()); value.error = None;});
                }
                if last_segment.elapsed() > Duration::from_secs(50) {
                    return Err("Twitch stopped delivering complete video segments.".into());
                }
                if fs2::available_space(root).map_err(|error| error.to_string())? < 512 * 1024 * 1024 {
                    return Err("Recording paused because free disk space fell below 512 MiB.".into());
                }
                if budget_full(db, db.settings()?.saved_budget_gb)? {
                    return Err("Recording paused because the saved clip budget is full.".into());
                }
            }
        }
    }
}

// Each entry is 125 ms of voice-band RMS in dBFS. Require a sustained,
// modulated rise above the stream's recent level to nominate a reaction.
#[derive(Clone, Copy)]
struct AudioFrame {
    level: f64,
    voiced: bool,
}

fn analyze_frame(bytes: &[u8]) -> AudioFrame {
    let samples: Vec<_> = bytes
        .chunks_exact(2)
        .map(|v| f64::from(i16::from_le_bytes([v[0], v[1]])))
        .collect();
    let power = samples.iter().map(|v| v * v).sum::<f64>() / samples.len() as f64;
    let level = 10.0 * (power.max(1.0) / 32768.0_f64.powi(2)).log10();
    // Normalized autocorrelation of the center 32ms, covering voice pitches
    // of roughly 80–470 Hz. Broad-band effects generally lack periodicity.
    let center = &samples[350..606];
    let center_energy = center.iter().map(|v| v * v).sum::<f64>();
    let voiced = level > -48.0
        && (17..=100).any(|lag| {
            let delayed = &samples[350 - lag..606 - lag];
            let other = delayed.iter().map(|v| v * v).sum::<f64>();
            let correlation = center.iter().zip(delayed).map(|(a, b)| a * b).sum::<f64>();
            correlation > 0.55 * (center_energy * other).sqrt()
        });
    AudioFrame { level, voiced }
}

fn reaction(profile: &[AudioFrame], history: &mut VecDeque<f64>) -> (i64, f64) {
    let mut ordered: Vec<_> = profile.iter().map(|f| f.level).collect();
    ordered.sort_by(f64::total_cmp);
    let mut prior: Vec<_> = history.iter().copied().collect();
    prior.sort_by(f64::total_cmp);
    let mut best = (0, 0.0);
    if prior.len() >= 6 {
        let baseline = prior[prior.len() / 2].max(-58.0);
        for (i, group) in profile.windows(8).enumerate() {
            if group
                .iter()
                .filter(|f| f.voiced && f.level > baseline + 7.0)
                .count()
                < 4
            {
                continue;
            }
            let mut levels: Vec<_> = group.iter().map(|f| f.level).collect();
            levels.sort_by(f64::total_cmp);
            let loud = levels[4];
            if loud < -38.0 || loud - baseline < 7.0 || levels[6] - levels[1] < 2.5 {
                continue;
            }
            let strength = ((loud - baseline - 5.0) / 14.0).clamp(0.0, 1.0);
            if strength > best.1 {
                best = ((i as i64 + 4) * 125, strength);
            }
        }
    }
    if !ordered.is_empty() {
        history.push_back(ordered[ordered.len() / 2]);
        if history.len() > 24 {
            history.pop_front();
        }
    }
    best
}

async fn audio_profile(program: PathBuf, path: PathBuf) -> Option<Vec<AudioFrame>> {
    let mut command = Command::new(program);
    command
        .kill_on_drop(true)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-t",
            "15",
            "-vn",
            "-af",
            "highpass=f=250,lowpass=f=3200",
            "-ar",
            "8000",
            "-ac",
            "1",
            "-f",
            "s16le",
            "pipe:1",
        ]);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = time::timeout(Duration::from_secs(8), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let profile = output
        .stdout
        .chunks_exact(2000)
        .map(analyze_frame)
        .collect();
    Some(profile)
}

async fn index_segments(
    db: &Database,
    session: i64,
    folder: &Path,
    anchor: i64,
    known: &mut HashSet<String>,
    recovering: bool,
) -> Result<Option<(PathBuf, i64)>, String> {
    let list = match fs::read_to_string(folder.join("segments.csv")).await {
        Ok(list) => list,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot read recording segment list: {error}")),
    };
    let mut latest = None;
    for line in list
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
    {
        let Some(segment) = read_segment(line.trim()) else {
            continue;
        };
        if known.contains(&segment.filename) {
            continue;
        }
        let path = folder.join(&segment.filename);
        let bytes = match fs::metadata(&path).await {
            Ok(metadata) => metadata.len(),
            Err(error) if recovering && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if bytes == 0 {
            if recovering {
                continue;
            }
            return Err("FFmpeg produced an empty video segment.".into());
        }
        db.connection()
            .and_then(|connection| {
                connection.execute(
                    "INSERT INTO recording_segments(session_id,path,starts_at_ms,ends_at_ms,bytes)
             VALUES (?1,?2,?3,?4,?5) ON CONFLICT(path) DO NOTHING",
                    params![
                        session,
                        path.to_string_lossy(),
                        anchor + segment.start_ms,
                        anchor + segment.end_ms,
                        bytes
                    ],
                )
            })
            .map_err(|error| error.to_string())?;
        known.insert(segment.filename);
        latest = Some((path, anchor + segment.start_ms));
    }
    Ok(latest)
}

pub async fn recover(db: &Database) -> Result<(), String> {
    let sessions = PathBuf::from(db.settings()?.storage_directory).join("recordings");
    let mut sessions = match fs::read_dir(sessions).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    while let Some(session) = sessions
        .next_entry()
        .await
        .map_err(|error| error.to_string())?
    {
        let Ok(id) = session.file_name().to_string_lossy().parse::<i64>() else {
            continue;
        };
        if !session.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let mut folders = fs::read_dir(session.path())
            .await
            .map_err(|error| error.to_string())?;
        while let Some(folder) = folders
            .next_entry()
            .await
            .map_err(|error| error.to_string())?
        {
            if !folder.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = folder.path();
            if let Ok(anchor) = fs::read_to_string(path.join("anchor-ms")).await {
                if let Ok(anchor) = anchor.parse::<i64>() {
                    if let Err(error) =
                        index_segments(db, id, &path, anchor, &mut HashSet::new(), true).await
                    {
                        eprintln!(
                            "ClipForge could not recover segments from {}: {error}",
                            path.display()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn prune_expired(db: &Database) -> Result<(), String> {
    prune(db, unix_ms() - RETENTION_MS)
}

fn prune(db: &Database, cutoff: i64) -> Result<(), String> {
    let mut connection = db.connection().map_err(|error| error.to_string())?;
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let disposable: Vec<(i64, String)> = (|| {
        let mut query = tx.prepare("SELECT id,path FROM recording_segments AS segment
          WHERE ends_at_ms<?1 AND NOT EXISTS (
            SELECT 1 FROM highlight_events AS event LEFT JOIN clip_jobs AS job ON job.highlight_id=event.id
            WHERE event.session_id=segment.session_id AND job.source_path IS NULL
              AND coalesce(job.status,'queued') <> 'published'
              AND segment.ends_at_ms > coalesce(event.triggered_at_ms,event.triggered_at*1000)-21000
              AND segment.starts_at_ms < coalesce(event.triggered_at_ms,event.triggered_at*1000)+180000
          )")?;
        let rows = query.query_map([cutoff], |row| Ok((row.get(0)?,row.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()
    })().map_err(|error| error.to_string())?;
    for (id, path) in disposable {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Cannot clean up expired video segments: {error}")),
        }
        tx.execute("DELETE FROM recording_segments WHERE id=?1", [id])
            .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quieter_segments_before_poll_preserve_strong_reaction_and_media_time() {
        let mut signal = Signal::default();
        let mut read = 0;
        signal.record_audio(10_000, 0.94);
        signal.record_audio(12_000, 0.52);
        signal.record_audio(14_000, 0.0);
        assert_eq!(
            signal.take_fresh_audio(15_000, &mut read),
            Some((10_000, 0.94))
        );
        assert_eq!(signal.take_fresh_audio(16_000, &mut read), None);
        assert_eq!(signal.take_fresh_audio(31_000, &mut 0), None);
    }

    #[test]
    fn reaction_requires_voiced_modulation_and_prior_history() {
        let levels = [-30., -15., -28., -14., -22., -14., -19., -12.];
        let frames = levels.map(|level| AudioFrame {
            level,
            voiced: true,
        });
        let baseline = || VecDeque::from(vec![-40.0; 8]);
        assert_eq!(reaction(&frames, &mut VecDeque::new()).1, 0.0);
        assert!(reaction(&frames, &mut baseline()).1 >= 0.78);
        assert_eq!(
            reaction(
                &frames.map(|f| AudioFrame { voiced: false, ..f }),
                &mut baseline()
            )
            .1,
            0.0
        );
        assert_eq!(
            reaction(
                &[AudioFrame {
                    level: -12.,
                    voiced: true
                }; 8],
                &mut baseline()
            )
            .1,
            0.0
        );
        let mut spike = [AudioFrame {
            level: -40.,
            voiced: true,
        }; 8];
        spike[3].level = -4.;
        assert_eq!(reaction(&spike, &mut baseline()).1, 0.0);
        let waveform: Vec<u8> = (0..1000)
            .flat_map(|i| {
                ((i as f64 * std::f64::consts::TAU * 200.0 / 8000.0)
                    .sin()
                    .mul_add(8000., 0.) as i16)
                    .to_le_bytes()
            })
            .collect();
        assert!(analyze_frame(&waveform).voiced);
    }

    #[test]
    fn parses_fractional_media_time() {
        assert_eq!(
            read_segment("segment-00000001.ts,5.1005,10.3"),
            Some(Segment {
                filename: "segment-00000001.ts".into(),
                start_ms: 5101,
                end_ms: 10300
            })
        );
        assert!(read_segment("segment-../../escape.ts,0,1").is_none());
        assert!(read_segment("segment-00000001.ts,5,4").is_none());
        assert!(read_segment("segment-00000001.ts,NaN,9").is_none());
    }

    #[test]
    fn rolls_off_unneeded_media_and_preserves_pending_lead_in() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open(root.path().join("retention.db")).unwrap();
        let connection = db.connection().unwrap();
        connection.execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at,ended_at) VALUES(1,1,1,100);
             INSERT INTO highlight_events(id,session_id,triggered_at,triggered_at_ms,score)
             VALUES(1,1,60,60500,82);"
        ).unwrap();
        let stale = root.path().join("stale.ts");
        let lead = root.path().join("lead.ts");
        for (file, start, end) in [(&stale, 10_000, 15_000), (&lead, 40_000, 45_000)] {
            std::fs::write(file, b"fixture").unwrap();
            connection
                .execute(
                    "INSERT INTO recording_segments(session_id,path,starts_at_ms,ends_at_ms,bytes)
                VALUES(1,?1,?2,?3,7)",
                    params![file.to_string_lossy(), start, end],
                )
                .unwrap();
        }
        prune(&db, 200_000).unwrap();
        assert!(!stale.exists());
        assert!(lead.exists());
        connection.execute("INSERT INTO clip_jobs(highlight_id,status,source_path) VALUES(1,'ready','source.mkv')", []).unwrap();
        prune(&db, 200_000).unwrap();
        assert!(!lead.exists());
    }

    #[tokio::test]
    async fn restores_closed_segments_at_the_persisted_anchor() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::open(root.path().join("recovery.db")).unwrap();
        let connection = db.connection().unwrap();
        connection.execute_batch(
            "INSERT INTO creators(id,username,channel_url) VALUES(1,'test','https://www.twitch.tv/test');
             INSERT INTO stream_sessions(id,creator_id,started_at) VALUES(1,1,1);"
        ).unwrap();
        let directory =
            PathBuf::from(db.settings().unwrap().storage_directory).join("recordings/1/10000");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("anchor-ms"), "10000").unwrap();
        std::fs::write(directory.join("segment-00000001.ts"), b"closed fixture").unwrap();
        std::fs::write(
            directory.join("segments.csv"),
            "segment-00000000.ts,0,5\nsegment-00000001.ts,5,10.25\nsegment-00000002.ts,10.25",
        )
        .unwrap();
        recover(&db).await.unwrap();
        recover(&db).await.unwrap();
        let result: (i64, i64, i64) = connection
            .query_row(
                "SELECT count(*),min(starts_at_ms),max(ends_at_ms) FROM recording_segments",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(result, (1, 15000, 20250));
    }
}
