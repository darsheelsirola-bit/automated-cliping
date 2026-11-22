CREATE TABLE IF NOT EXISTS publication_cleanup (
    clip_id INTEGER PRIMARY KEY REFERENCES uploaded_videos(clip_id),
    check_after INTEGER NOT NULL DEFAULT 0
);
