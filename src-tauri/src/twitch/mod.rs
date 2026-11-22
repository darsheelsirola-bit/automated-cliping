mod auth;
mod chat;
mod detector;

use crate::db::Database;
use reqwest::Client;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::{
    sync::{mpsc, watch, Mutex, Notify},
    task::JoinHandle,
};

const CLIENT_KEY: &str = "twitch_client_id";
const ENABLED_KEY: &str = "monitoring_enabled";

pub struct Twitch {
    client: Client,
    flow: Mutex<Option<(String, auth::DeviceCode)>>,
    credential_lock: Mutex<()>,
    shutdown: watch::Sender<bool>,
    stopped: Notify,
}

impl Twitch {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: Client::builder().timeout(Duration::from_secs(15)).build()?,
            flow: Mutex::new(None),
            credential_lock: Mutex::new(()),
            shutdown: watch::channel(false).0,
            stopped: Notify::new(),
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignIn {
    user_code: String,
    verification_uri: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    client_id: String,
    connected: bool,
    monitoring: bool,
}

#[tauri::command]
pub fn twitch_status(db: State<'_, Database>) -> Result<Status, String> {
    let client_id = setting(&db, CLIENT_KEY)?.unwrap_or_default();
    let connected = auth::credential(&client_id)
        .and_then(|entry| entry.get_password().map_err(|error| error.to_string()))
        .is_ok();
    Ok(Status {
        client_id,
        connected,
        monitoring: enabled(&db)?,
    })
}

#[tauri::command]
pub async fn twitch_begin(twitch: State<'_, Twitch>, client_id: String) -> Result<SignIn, String> {
    let client_id = client_id.trim().to_owned();
    let device = auth::begin(&twitch.client, &client_id).await?;
    let result = SignIn {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
    };
    *twitch.flow.lock().await = Some((client_id, device));
    Ok(result)
}

#[tauri::command]
pub async fn twitch_finish(
    db: State<'_, Database>,
    twitch: State<'_, Twitch>,
) -> Result<(), String> {
    let (client_id, device) = twitch
        .flow
        .lock()
        .await
        .take()
        .ok_or("Start Twitch sign-in first.")?;
    // Finish may wait for user approval. The old monitoring session stays usable.
    auth::finish(&twitch.client, &client_id, device).await?;
    put_setting(&db, CLIENT_KEY, &client_id)
}

#[tauri::command]
pub fn set_monitoring(
    app: AppHandle,
    db: State<'_, Database>,
    monitoring: bool,
) -> Result<(), String> {
    if monitoring && !twitch_status(db.clone())?.connected {
        return Err("Connect Twitch in Settings before starting monitoring.".into());
    }
    if monitoring && !crate::production::model_ready(app)? {
        return Err("Install Whisper Large-v3 and media dependencies in Settings before starting monitoring.".into());
    }
    put_setting(&db, ENABLED_KEY, if monitoring { "true" } else { "false" })
}

fn setting(db: &Database, key: &str) -> Result<Option<String>, String> {
    db.connection()
        .and_then(|connection| {
            connection
                .query_row(
                    "SELECT value FROM app_settings WHERE key = ?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()
        })
        .map_err(|error| error.to_string())
}

fn put_setting(db: &Database, key: &str, value: &str) -> Result<(), String> {
    db.connection()
        .and_then(|connection| {
            connection.execute(
                "INSERT INTO app_settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn enabled(db: &Database) -> Result<bool, String> {
    Ok(setting(db, ENABLED_KEY)?.as_deref() == Some("true"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Deserialize)]
struct Streams {
    data: Vec<Stream>,
}

#[derive(Deserialize)]
struct Stream {
    id: String,
    user_login: String,
    title: String,
}

async fn live(
    client: &Client,
    authorization: &auth::Authorization,
    names: &[String],
) -> Result<Vec<Stream>, String> {
    let mut streams = Vec::new();
    for chunk in names.chunks(100) {
        let query: Vec<_> = chunk.iter().map(|name| ("user_login", name)).collect();
        let response = client
            .get("https://api.twitch.tv/helix/streams")
            .header("Client-Id", &authorization.client_id)
            .bearer_auth(&authorization.tokens.access_token)
            .query(&[("first", "100")])
            .query(&query)
            .send()
            .await
            .map_err(|_| "Could not reach Twitch live status.")?;
        let status = response.status();
        let batch: Streams = response
            .error_for_status()
            .map_err(|_| format!("Twitch live status failed ({status})."))?
            .json()
            .await
            .map_err(|_| "Twitch returned invalid live status data.")?;
        streams.extend(batch.data);
    }
    Ok(streams)
}

struct Running {
    stream_id: String,
    worker: JoinHandle<()>,
    recording_stop: watch::Sender<bool>,
    recorder: Option<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.worker.abort();
        let _ = self.recording_stop.send(true);
    }
}

fn retire(mut task: Running, recorders: &mut Vec<JoinHandle<()>>) {
    let _ = task.recording_stop.send(true);
    recorders.extend(task.recorder.take());
}

pub async fn supervise(app: AppHandle, db: Database) {
    let twitch = app.state::<Twitch>();
    let mut shutdown = twitch.shutdown.subscribe();
    let mut tasks: HashMap<i64, Running> = HashMap::new();
    let mut recorders = Vec::new();
    let mut backoff = 5_u64;
    while !*shutdown.borrow() {
        let refreshed = tokio::select! {
            _ = shutdown.changed() => break,
            result = refresh(&app, &db, &twitch, &mut tasks, &mut recorders) => result,
        };
        let delay = match refreshed {
            Ok(()) => {
                backoff = 5;
                15
            }
            Err(error) => {
                // A failed Helix poll alone does not invalidate an active chat socket.
                let _ = update_waiting(&db, "reconnecting", Some(&error));
                let _ = db.connection().and_then(|connection| {
                    connection.execute(
                        "INSERT INTO error_logs(subsystem, message) VALUES ('twitch', ?1)",
                        [&error],
                    )
                });
                let delay = backoff;
                backoff = (backoff * 2).min(120);
                delay
            }
        };
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
        }
    }
    for (_, task) in tasks.drain() {
        retire(task, &mut recorders);
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        for task in &mut recorders {
            let _ = task.await;
        }
    })
    .await;
    twitch.stopped.notify_one();
}

pub async fn stop(app: &AppHandle) {
    let twitch = app.state::<Twitch>();
    twitch.shutdown.send_replace(true);
    let _ = tokio::time::timeout(Duration::from_secs(12), twitch.stopped.notified()).await;
}

fn update_waiting(db: &Database, status: &str, error: Option<&str>) -> rusqlite::Result<usize> {
    db.connection()?.execute(
        "UPDATE creators SET status=CASE WHEN status IN ('live','connecting','reconnecting')
         THEN ?1 ELSE status END, last_error=?2
         WHERE enabled=1 AND status NOT IN ('monitoring','recording')",
        params![status, error],
    )
}

async fn refresh(
    app: &AppHandle,
    db: &Database,
    twitch: &Twitch,
    tasks: &mut HashMap<i64, Running>,
    recorders: &mut Vec<JoinHandle<()>>,
) -> Result<(), String> {
    recorders.retain(|recorder| !recorder.is_finished());
    if !enabled(db)? {
        for (_, task) in tasks.drain() {
            retire(task, recorders);
        }
        db.connection()
            .and_then(|connection| {
                connection.execute(
                    "UPDATE creators SET status='idle', stream_title=NULL WHERE status!='idle'",
                    [],
                )?;
                connection.execute(
                    "UPDATE stream_sessions SET ended_at=unixepoch() WHERE ended_at IS NULL",
                    [],
                )
            })
            .map_err(|error| error.to_string())?;
        return Ok(());
    }

    let creators: Vec<(i64, String)> = (|| -> rusqlite::Result<_> {
        let connection = db.connection()?;
        let mut query = connection.prepare("SELECT id, username FROM creators WHERE enabled=1")?;
        let rows = query.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    })()
    .map_err(|error| error.to_string())?;
    let active: HashSet<_> = creators.iter().map(|(id, _)| *id).collect();
    let inactive: Vec<_> = tasks
        .iter()
        .filter_map(|(id, running)| {
            (!active.contains(id) || running.worker.is_finished()).then_some(*id)
        })
        .collect();
    for id in inactive {
        if let Some(task) = tasks.remove(&id) {
            retire(task, recorders);
        }
    }
    if creators.is_empty() {
        return Ok(());
    }

    let client_id = setting(db, CLIENT_KEY)?.ok_or("Connect Twitch in Settings.")?;
    let authorization = {
        let _lock = twitch.credential_lock.lock().await;
        auth::authorize(&twitch.client, &client_id).await?
    };
    let names: Vec<_> = creators.iter().map(|(_, name)| name.clone()).collect();
    let streams = live(&twitch.client, &authorization, &names).await?;
    let by_name: HashMap<_, _> = streams
        .iter()
        .map(|stream| (stream.user_login.as_str(), stream))
        .collect();
    for (id, creator) in creators {
        if let Some(stream) = by_name.get(creator.as_str()) {
            let session_id = start_session(db, id, stream).map_err(|error| error.to_string())?;
            if tasks
                .get(&id)
                .is_some_and(|running| running.stream_id == stream.id)
            {
                continue;
            }
            if let Some(task) = tasks.remove(&id) {
                retire(task, recorders);
            }
            let (recording_stop, stop) = watch::channel(false);
            let (recording_state, recording) = watch::channel(crate::recording::Signal::default());
            let recorder = tokio::spawn(crate::recording::record(
                db.clone(),
                session_id,
                creator.clone(),
                crate::production::runtime(app)?,
                stop,
                recording_state,
            ));
            let db = db.clone();
            let app = app.clone();
            let login = authorization.identity.login.clone();
            let access_token = authorization.tokens.access_token.clone();
            let worker = tokio::spawn(async move {
                monitor_chat(
                    app,
                    db,
                    id,
                    session_id,
                    creator,
                    login,
                    access_token,
                    recording,
                )
                .await;
            });
            tasks.insert(
                id,
                Running {
                    stream_id: stream.id.clone(),
                    worker,
                    recording_stop,
                    recorder: Some(recorder),
                },
            );
        } else {
            if let Some(task) = tasks.remove(&id) {
                retire(task, recorders);
            }
            let connection = db.connection().map_err(|error| error.to_string())?;
            connection
                .execute(
                    "UPDATE stream_sessions SET ended_at=unixepoch()
                WHERE creator_id=?1 AND ended_at IS NULL",
                    [id],
                )
                .map_err(|error| error.to_string())?;
            connection
                .execute(
                    "UPDATE creators SET status='offline', stream_title=NULL, last_error=NULL
                WHERE id=?1 AND enabled=1",
                    [id],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn start_session(db: &Database, creator_id: i64, stream: &Stream) -> rusqlite::Result<i64> {
    let mut connection = db.connection()?;
    let tx = connection.transaction()?;
    tx.execute(
        "UPDATE stream_sessions SET ended_at=unixepoch()
        WHERE creator_id=?1 AND source_id!=?2 AND ended_at IS NULL",
        params![creator_id, stream.id],
    )?;
    let id = tx.query_row(
        "INSERT INTO stream_sessions (creator_id,source_id,started_at,title)
        VALUES (?1,?2,unixepoch(),?3)
        ON CONFLICT(creator_id, source_id) DO UPDATE SET title=excluded.title, ended_at=NULL
        RETURNING id",
        params![creator_id, stream.id, stream.title],
        |row| row.get(0),
    )?;
    tx.execute(
        "UPDATE creators SET stream_title=?2, last_live_at=unixepoch()
        WHERE id=?1 AND enabled=1",
        params![creator_id, stream.title],
    )?;
    tx.commit()?;
    Ok(id)
}

fn chat_status(db: &Database, id: i64, status: &str, error: Option<&str>) {
    if let Err(problem) = db.connection().and_then(|connection| {
        connection.execute(
            "UPDATE creators SET status=?2, last_error=?3 WHERE id=?1 AND enabled=1",
            params![id, status, error],
        )
    }) {
        eprintln!("ClipForge monitoring status: {problem}");
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Highlight {
    pub session_id: i64,
    pub trigger_ms: i64,
    pub score: f64,
}

async fn monitor_chat(
    app: AppHandle,
    db: Database,
    creator_id: i64,
    session_id: i64,
    creator: String,
    login: String,
    token: String,
    recording: watch::Receiver<crate::recording::Signal>,
) {
    let mut detector = detector::Detector::default();
    let mut last_audio_ms = 0;
    let mut backoff = 2_u64;
    loop {
        let (send, mut receive) = mpsc::channel(4096);
        chat_status(&db, creator_id, "connecting", None);
        let connection = chat::listen(&creator, &login, &token, send);
        tokio::pin!(connection);
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut joined = false;
        let error = loop {
            tokio::select! {
                result = &mut connection => break result.err().unwrap_or_else(|| "Twitch chat closed.".into()),
                Some(event) = receive.recv() => match event {
                    chat::Event::Joined => {
                        joined = true;
                        backoff = 2;
                        detector.suppress_until(now_ms() + 30_000);
                        chat_status(&db, creator_id, "monitoring", None);
                    }
                    chat::Event::Raid => detector.suppress_until(now_ms() + 90_000),
                    chat::Event::Message { chatter, text, emotes, sent_ms } if joined => {
                        let received = now_ms();
                        let timestamp = sent_ms.filter(|time| *time >= received - 30_000)
                            .unwrap_or(received).min(received);
                        detector.message(timestamp, chatter, &text, emotes);
                    }
                    _ => {}
                },
                _ = ticker.tick(), if joined => {
                    let now = now_ms();
                    let signal = recording.borrow().clone();
                    let capturing = signal.latest_segment_ms.is_some_and(|latest| now - latest <= 18_000);
                    chat_status(&db, creator_id, if capturing { "recording" } else { "monitoring" },
                        signal.error.as_deref());
                    let threshold = db.settings().map(|settings| f64::from(settings.detection_threshold)).unwrap_or(60.0);
                    let audio = signal.take_fresh_audio(now, &mut last_audio_ms);
                    let mut sample = detector.sample(now, audio.map(|(_, strength)| strength), threshold);
                    if !capturing { sample.score = None; }
                    let event_ms = audio.filter(|(_, strength)| *strength >= 0.78).map_or(now, |(timestamp, _)| timestamp.min(now));
                    match save_sample(&db, creator_id, session_id, now, event_ms, &sample) {
                        Ok(Some(highlight)) => {
                            if let Err(problem) = app.emit("twitch-highlight", highlight) {
                                eprintln!("ClipForge highlight event: {problem}");
                            }
                        }
                        Ok(None) => {}
                        Err(problem) => break format!("Could not save Twitch activity: {problem}"),
                    }
                }
            }
        };
        chat_status(&db, creator_id, "reconnecting", Some(&error));
        // Restart through the supervisor to pick up refreshed credentials.
        if error.contains("rejected the session") {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(90);
    }
}

fn save_sample(
    db: &Database,
    creator: i64,
    session: i64,
    now: i64,
    event_ms: i64,
    sample: &detector::Sample,
) -> rusqlite::Result<Option<Highlight>> {
    let mut connection = db.connection()?;
    let tx = connection.transaction()?;
    tx.execute(
        "INSERT INTO chat_activity_samples
        (session_id,sampled_at,messages,unique_chatters,emotes,repeats,per_second,per_thirty)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT DO NOTHING",
        params![
            session,
            now / 1000,
            sample.per_five,
            sample.unique,
            sample.emotes,
            sample.repeated,
            sample.per_second,
            sample.per_thirty
        ],
    )?;
    tx.execute(
        "UPDATE creators SET baseline=?2 WHERE id=?1",
        params![creator, sample.baseline],
    )?;
    let highlight = if let Some(score) = sample.score {
        let inserted = tx.execute(
            "INSERT INTO highlight_events(session_id,triggered_at,score,triggered_at_ms)
            SELECT ?1,?2,?3,?4 WHERE NOT EXISTS (
                SELECT 1 FROM highlight_events WHERE session_id=?1 AND triggered_at>=?2-45
            ) ON CONFLICT DO NOTHING",
            params![session, event_ms / 1000, score, event_ms],
        )?;
        (inserted == 1).then_some(Highlight {
            session_id: session,
            trigger_ms: event_ms,
            score,
        })
    } else {
        None
    };
    tx.commit()?;
    Ok(highlight)
}
