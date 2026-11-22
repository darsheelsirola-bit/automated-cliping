ALTER TABLE highlight_events ADD COLUMN triggered_at_ms INTEGER;
UPDATE highlight_events SET triggered_at_ms=triggered_at*1000;
ALTER TABLE chat_activity_samples ADD COLUMN per_second INTEGER NOT NULL DEFAULT 0;
ALTER TABLE chat_activity_samples ADD COLUMN per_thirty INTEGER NOT NULL DEFAULT 0;
ALTER TABLE clip_jobs ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE clip_jobs ADD COLUMN retry_at INTEGER;
CREATE INDEX clip_jobs_retry_idx ON clip_jobs(status,retry_at);
