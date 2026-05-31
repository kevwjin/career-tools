CREATE TABLE IF NOT EXISTS report_sends (
    id BIGSERIAL PRIMARY KEY,
    report_type TEXT NOT NULL,
    week_start DATE NOT NULL,
    week_end DATE NOT NULL,
    to_addrs TEXT[] NOT NULL,
    cc_addrs TEXT[] NOT NULL DEFAULT '{}',
    content_hash TEXT NOT NULL,
    gmail_message_id TEXT,
    sent_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (report_type, week_start, to_addrs, cc_addrs)
);
