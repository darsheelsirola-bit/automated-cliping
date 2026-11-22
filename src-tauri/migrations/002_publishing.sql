ALTER TABLE publishing_jobs ADD COLUMN channel_id TEXT;
ALTER TABLE publishing_jobs ADD COLUMN file_size INTEGER;
ALTER TABLE publishing_jobs ADD COLUMN may_have_committed INTEGER NOT NULL DEFAULT 0 CHECK (may_have_committed IN (0,1));
CREATE INDEX publishing_jobs_due_idx ON publishing_jobs(status, retry_at);
