CREATE TABLE IF NOT EXISTS leetcode_submissions (
    id BIGSERIAL PRIMARY KEY,
    username TEXT NOT NULL,
    submission_id TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    title_slug TEXT NOT NULL,
    submitted_at TIMESTAMPTZ NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS leetcode_submissions_username_submitted_at_idx
    ON leetcode_submissions (username, submitted_at DESC);

CREATE INDEX IF NOT EXISTS leetcode_submissions_title_slug_idx
    ON leetcode_submissions (title_slug);
