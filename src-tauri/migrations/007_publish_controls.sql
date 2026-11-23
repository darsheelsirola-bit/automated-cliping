ALTER TABLE clip_jobs ADD COLUMN editorial_pass INTEGER CHECK (editorial_pass IN (0, 1));
ALTER TABLE clip_jobs ADD COLUMN editorial_notes TEXT;

CREATE TABLE clip_review (
    clip_id INTEGER PRIMARY KEY REFERENCES clip_jobs(id),
    decision TEXT NOT NULL CHECK (decision IN ('hold', 'approved'))
);

CREATE TABLE youtube_channel_cooldowns (
    channel_id TEXT PRIMARY KEY,
    retry_at INTEGER NOT NULL
);

-- Earlier uploads use the same remote verification gate as new ones.
INSERT OR IGNORE INTO publication_cleanup(clip_id)
SELECT u.clip_id
FROM uploaded_videos u
JOIN publishing_jobs p USING(clip_id)
JOIN clip_jobs j ON j.id=u.clip_id
WHERE p.status='uploaded' AND p.channel_id IS NOT NULL AND j.work_dir IS NOT NULL;
