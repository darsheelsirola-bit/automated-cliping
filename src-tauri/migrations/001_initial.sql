CREATE TABLE creators (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL COLLATE NOCASE UNIQUE,
    channel_url TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    status TEXT NOT NULL DEFAULT 'idle',
    stream_title TEXT,
    last_live_at INTEGER,
    baseline REAL,
    last_error TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE stream_sessions (
    id INTEGER PRIMARY KEY,
    creator_id INTEGER NOT NULL REFERENCES creators(id),
    source_id TEXT,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    title TEXT,
    UNIQUE (creator_id, source_id)
);
CREATE INDEX stream_sessions_creator_idx ON stream_sessions(creator_id, started_at);
CREATE TABLE chat_activity_samples (
    session_id INTEGER NOT NULL REFERENCES stream_sessions(id),
    sampled_at INTEGER NOT NULL,
    messages INTEGER NOT NULL,
    unique_chatters INTEGER NOT NULL,
    emotes INTEGER NOT NULL DEFAULT 0,
    repeats INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(session_id, sampled_at)
);
CREATE TABLE highlight_events (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES stream_sessions(id),
    triggered_at INTEGER NOT NULL,
    score REAL NOT NULL CHECK (score BETWEEN 0 AND 100),
    confidence REAL CHECK (confidence BETWEEN 0 AND 100),
    starts_at INTEGER,
    ends_at INTEGER,
    UNIQUE (session_id, triggered_at)
);
CREATE TABLE recording_segments (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES stream_sessions(id),
    path TEXT NOT NULL UNIQUE,
    starts_at_ms INTEGER NOT NULL,
    ends_at_ms INTEGER NOT NULL CHECK (ends_at_ms > starts_at_ms),
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0, 1))
);
CREATE INDEX recording_segments_time_idx ON recording_segments(session_id, starts_at_ms, ends_at_ms);
CREATE TABLE clip_jobs (
    id INTEGER PRIMARY KEY,
    highlight_id INTEGER NOT NULL UNIQUE REFERENCES highlight_events(id),
    status TEXT NOT NULL DEFAULT 'queued',
    source_path TEXT,
    output_path TEXT,
    thumbnail_path TEXT,
    duration_ms INTEGER,
    saved_bytes INTEGER NOT NULL DEFAULT 0 CHECK (saved_bytes >= 0),
    error TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX clip_jobs_status_idx ON clip_jobs(status, created_at);
CREATE TABLE transcription_jobs (
    clip_id INTEGER PRIMARY KEY REFERENCES clip_jobs(id),
    status TEXT NOT NULL DEFAULT 'queued',
    transcript_path TEXT,
    srt_path TEXT,
    ass_path TEXT,
    error TEXT
);
CREATE TABLE render_jobs (
    clip_id INTEGER PRIMARY KEY REFERENCES clip_jobs(id),
    status TEXT NOT NULL DEFAULT 'queued',
    output_path TEXT,
    error TEXT
);
CREATE TABLE publishing_jobs (
    clip_id INTEGER PRIMARY KEY REFERENCES clip_jobs(id),
    status TEXT NOT NULL DEFAULT 'queued',
    upload_session TEXT,
    uploaded_bytes INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    retry_at INTEGER,
    error TEXT
);
CREATE TABLE uploaded_videos (
    clip_id INTEGER PRIMARY KEY REFERENCES clip_jobs(id),
    video_id TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    published_at INTEGER NOT NULL
);
CREATE TABLE app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE error_logs (
    id INTEGER PRIMARY KEY,
    subsystem TEXT NOT NULL,
    message TEXT NOT NULL,
    occurred_at INTEGER NOT NULL DEFAULT (unixepoch())
);

