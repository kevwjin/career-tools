CREATE TABLE IF NOT EXISTS gmail_messages (
    id BIGSERIAL PRIMARY KEY,
    gmail_message_id TEXT NOT NULL UNIQUE,
    gmail_thread_id TEXT NOT NULL,
    history_id TEXT,
    internal_date TIMESTAMPTZ,
    rfc822_message_id TEXT,
    from_addr TEXT,
    to_addrs TEXT[] NOT NULL DEFAULT '{}',
    cc_addrs TEXT[] NOT NULL DEFAULT '{}',
    subject TEXT,
    snippet TEXT,
    label_ids TEXT[] NOT NULL DEFAULT '{}',
    size_estimate INTEGER,
    body_text TEXT,
    body_text_hash TEXT,
    raw_payload_json JSONB NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS gmail_messages_internal_date_idx ON gmail_messages (internal_date DESC);
CREATE INDEX IF NOT EXISTS gmail_messages_thread_idx ON gmail_messages (gmail_thread_id);

CREATE TABLE IF NOT EXISTS llm_extraction_attempts (
    id BIGSERIAL PRIMARY KEY,
    gmail_message_id TEXT NOT NULL REFERENCES gmail_messages (gmail_message_id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    raw_response_json JSONB,
    parsed_company TEXT,
    parsed_role TEXT,
    parsed_location TEXT,
    parsed_job_posting_url TEXT,
    confidence DOUBLE PRECISION,
    decision TEXT NOT NULL,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (gmail_message_id, model, prompt_version)
);

CREATE TABLE IF NOT EXISTS job_applications (
    id BIGSERIAL PRIMARY KEY,
    company TEXT NOT NULL,
    role TEXT NOT NULL,
    location TEXT,
    job_posting_url TEXT,
    status TEXT NOT NULL DEFAULT 'applied',
    source_gmail_message_id TEXT NOT NULL REFERENCES gmail_messages (gmail_message_id),
    gmail_thread_id TEXT NOT NULL,
    confidence DOUBLE PRECISION NOT NULL,
    application_key TEXT NOT NULL UNIQUE,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS job_applications_status_idx ON job_applications (status);
