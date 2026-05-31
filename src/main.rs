use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeZone, Utc};
use clap::{Args, Parser, Subcommand};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";
const PROMPT_VERSION: &str = "applied-v1";

#[derive(Parser)]
#[command(name = "career-tools")]
#[command(about = "Local Gmail job application tracker")]
struct Cli {
    #[arg(long, env = "CAREER_TOOLS_DATABASE_URL", global = true, default_value = "postgres://career_tools:career_tools@localhost:5432/career_tools")]
    database_url: String,

    #[arg(long, env = "CAREER_TOOLS_CONFIG_DIR", global = true)]
    config_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Preflight,
    GmailAuth(GmailAuthArgs),
    Ingest(IngestArgs),
    Process(ProcessArgs),
    Daily(DailyArgs),
    Inspect {
        #[command(subcommand)]
        command: InspectCommand,
    },
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
}

#[derive(Args)]
struct IngestArgs {
    #[arg(long, default_value_t = 48)]
    hours: u64,
}

#[derive(Args)]
struct GmailAuthArgs {
    #[arg(long)]
    callback_url: Option<String>,
}

#[derive(Args)]
struct DailyArgs {
    #[arg(long, default_value_t = 26)]
    hours: u64,
}

#[derive(Args)]
struct ProcessArgs {
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    #[arg(long, env = "CAREER_TOOLS_VLLM_BASE_URL", default_value = "http://127.0.0.1:8000/v1")]
    vllm_base_url: String,

    #[arg(long, env = "CAREER_TOOLS_VLLM_MODEL", default_value = "Qwen/Qwen3-8B-AWQ")]
    model: String,

    #[arg(long, default_value_t = 25)]
    limit: i64,

    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Subcommand)]
enum InspectCommand {
    Emails(InspectListArgs),
    Email(InspectEmailArgs),
    Attempts(InspectListArgs),
    Applications(InspectListArgs),
}

#[derive(Args)]
struct InspectListArgs {
    #[arg(long, default_value_t = 25)]
    limit: i64,
}

#[derive(Args)]
struct InspectEmailArgs {
    gmail_message_id: String,
}

#[derive(Subcommand)]
enum DbCommand {
    Migrate,
}

#[derive(Debug, Deserialize)]
struct OAuthClientFile {
    installed: Option<OAuthClient>,
    web: Option<OAuthClient>,
}

#[derive(Debug, Deserialize)]
struct OAuthClient {
    client_id: String,
    client_secret: String,
    auth_uri: String,
    token_uri: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_epoch: i64,
    scope: Option<String>,
    token_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    scope: Option<String>,
    token_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GmailListResponse {
    messages: Option<Vec<GmailMessageStub>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GmailMessageStub {
    id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct GmailMessage {
    id: String,
    #[serde(rename = "threadId")]
    thread_id: String,
    #[serde(rename = "historyId")]
    history_id: Option<String>,
    #[serde(rename = "internalDate")]
    internal_date: Option<String>,
    payload: Option<GmailPayload>,
    snippet: Option<String>,
    #[serde(rename = "labelIds")]
    label_ids: Option<Vec<String>>,
    #[serde(rename = "sizeEstimate")]
    size_estimate: Option<i32>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GmailPayload {
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    headers: Option<Vec<GmailHeader>>,
    body: Option<GmailBody>,
    parts: Option<Vec<GmailPayload>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GmailHeader {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct GmailBody {
    data: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AppliedExtraction {
    is_applied: bool,
    company: Option<String>,
    role: Option<String>,
    location: Option<String>,
    job_posting_url: Option<String>,
    confidence: f64,
    reason: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = AppConfig::new(cli.config_dir)?;

    match cli.command {
        Command::Preflight => preflight(&cli.database_url, &cfg).await,
        Command::GmailAuth(args) => gmail_auth(&cfg, args).await,
        Command::Ingest(args) => {
            let pool = connect_and_migrate(&cli.database_url).await?;
            ingest(&pool, &cfg, args.hours).await
        }
        Command::Process(args) => {
            let pool = connect_and_migrate(&cli.database_url).await?;
            process(&pool, args).await
        }
        Command::Daily(args) => {
            let pool = connect_and_migrate(&cli.database_url).await?;
            ingest(&pool, &cfg, args.hours).await?;
            process(
                &pool,
                ProcessArgs {
                    dry_run: false,
                    vllm_base_url: "http://127.0.0.1:8000/v1".to_string(),
                    model: "Qwen/Qwen3-8B-AWQ".to_string(),
                    limit: 100,
                    force: false,
                },
            )
            .await
        }
        Command::Inspect { command } => {
            let pool = connect_and_migrate(&cli.database_url).await?;
            inspect(&pool, command).await
        }
        Command::Db {
            command: DbCommand::Migrate,
        } => {
            let pool = PgPool::connect(&cli.database_url).await?;
            sqlx::migrate!("./migrations").run(&pool).await?;
            println!("migrations applied");
            Ok(())
        }
    }
}

struct AppConfig {
    dir: PathBuf,
    client_path: PathBuf,
    token_path: PathBuf,
}

impl AppConfig {
    fn new(config_dir: Option<PathBuf>) -> Result<Self> {
        let dir = match config_dir {
            Some(path) => path,
            None => dirs::config_dir()
                .context("could not determine config directory")?
                .join("career-tools"),
        };
        Ok(Self {
            client_path: dir.join("google-oauth-client.json"),
            token_path: dir.join("google-token.json"),
            dir,
        })
    }
}

async fn connect_and_migrate(database_url: &str) -> Result<PgPool> {
    let pool = PgPool::connect(database_url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

async fn preflight(database_url: &str, cfg: &AppConfig) -> Result<()> {
    println!("config dir: {}", cfg.dir.display());
    check_file("oauth client", &cfg.client_path);
    check_file("gmail token", &cfg.token_path);

    match PgPool::connect(database_url).await {
        Ok(pool) => {
            sqlx::query("SELECT 1").execute(&pool).await?;
            println!("postgres: ok");
        }
        Err(err) => println!("postgres: failed: {err}"),
    }

    match std::process::Command::new("nvidia-smi").arg("-L").output() {
        Ok(output) if output.status.success() => println!("gpu: ok"),
        Ok(output) => println!(
            "gpu: nvidia-smi failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => println!("gpu: nvidia-smi unavailable: {err}"),
    }

    let client = Client::new();
    match client
        .get("http://127.0.0.1:8000/v1/models")
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => println!("vllm: ok"),
        Ok(resp) => println!("vllm: responded with {}", resp.status()),
        Err(err) => println!("vllm: unavailable: {err}"),
    }

    Ok(())
}

fn check_file(label: &str, path: &PathBuf) {
    if path.exists() {
        println!("{label}: {}", path.display());
    } else {
        println!("{label}: missing at {}", path.display());
    }
}

async fn gmail_auth(cfg: &AppConfig, args: GmailAuthArgs) -> Result<()> {
    fs::create_dir_all(&cfg.dir)?;
    let oauth = read_oauth_client(cfg)?;

    if let Some(callback_url) = args.callback_url {
        let (code, redirect_uri) = parse_manual_callback(&callback_url)?;
        let token = exchange_code_for_token(&oauth, &code, &redirect_uri).await?;
        write_token(cfg, &token)?;
        println!("stored Gmail token at {}", cfg.token_path.display());
        return Ok(());
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect_uri = format!("http://{}", listener.local_addr()?);

    let mut auth_url = Url::parse(&oauth.auth_uri)?;
    auth_url.query_pairs_mut()
        .append_pair("client_id", &oauth.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", GMAIL_READONLY_SCOPE)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");

    println!("opening Google OAuth consent in browser");
    if webbrowser::open(auth_url.as_str()).is_err() {
        println!("open this URL manually:\n{auth_url}");
    }

    let code = wait_for_oauth_code(listener).await?;
    let token = exchange_code_for_token(&oauth, &code, &redirect_uri).await?;
    write_token(cfg, &token)?;
    println!("stored Gmail token at {}", cfg.token_path.display());
    Ok(())
}

fn parse_manual_callback(callback_url: &str) -> Result<(String, String)> {
    let callback_url = callback_url
        .replace("\\?", "?")
        .replace("\\=", "=")
        .replace("\\&", "&");
    let url = Url::parse(&callback_url).context("callback URL must be the full localhost URL from the browser")?;
    let code = url
        .query_pairs()
        .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
        .context("callback URL did not include a code query parameter")?;
    if code == "..." {
        bail!("replace code=... with the full callback URL copied from Safari's address bar");
    }
    let host = url
        .host_str()
        .context("callback URL must include a localhost host")?;
    let port = url
        .port()
        .context("callback URL must include the localhost callback port")?;
    let redirect_uri = format!("{}://{}:{}", url.scheme(), host, port);
    Ok((code, redirect_uri))
}

async fn wait_for_oauth_code(listener: TcpListener) -> Result<String> {
    let (mut stream, _) = listener.accept().await?;
    let mut buffer = vec![0; 8192];
    let n = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..n]);
    let first_line = request.lines().next().context("empty OAuth callback")?;
    let path = first_line
        .split_whitespace()
        .nth(1)
        .context("malformed OAuth callback request")?;
    let callback = Url::parse(&format!("http://localhost{path}"))?;
    let params: HashMap<_, _> = callback.query_pairs().into_owned().collect();
    let body = "Gmail auth complete. You can close this tab.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;

    params
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow!("OAuth callback did not include a code"))
}

async fn exchange_code_for_token(oauth: &OAuthClient, code: &str, redirect_uri: &str) -> Result<StoredToken> {
    let client = Client::new();
    let response: TokenResponse = client
        .post(&oauth.token_uri)
        .form(&[
            ("code", code),
            ("client_id", oauth.client_id.as_str()),
            ("client_secret", oauth.client_secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?
        .pipe(error_for_status_with_body)
        .await?
        .json()
        .await?;

    Ok(StoredToken {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at_epoch: now_epoch() + response.expires_in.unwrap_or(3600) - 60,
        scope: response.scope,
        token_type: response.token_type,
    })
}

async fn ingest(pool: &PgPool, cfg: &AppConfig, hours: u64) -> Result<()> {
    let oauth = read_oauth_client(cfg)?;
    let token = access_token(cfg, &oauth).await?;
    let client = Client::new();
    let after_epoch = SystemTime::now()
        .checked_sub(Duration::from_secs(hours * 60 * 60))
        .context("invalid ingest window")?
        .duration_since(UNIX_EPOCH)?
        .as_secs();

    let mut page_token: Option<String> = None;
    let mut seen = 0usize;
    let mut inserted_or_updated = 0usize;

    loop {
        let mut request = client
            .get("https://gmail.googleapis.com/gmail/v1/users/me/messages")
            .bearer_auth(&token)
            .query(&[
                ("q", format!("after:{after_epoch} -in:drafts")),
                ("maxResults", "100".to_string()),
            ]);

        if let Some(page) = &page_token {
            request = request.query(&[("pageToken", page)]);
        }

        let page: GmailListResponse = request.send().await?.error_for_status()?.json().await?;
        for stub in page.messages.unwrap_or_default() {
            seen += 1;
            let message = fetch_message(&client, &token, &stub.id).await?;
            upsert_message(pool, &message).await?;
            inserted_or_updated += 1;
        }

        page_token = page.next_page_token;
        if page_token.is_none() {
            break;
        }
    }

    println!("ingest complete: saw {seen} messages, upserted {inserted_or_updated}");
    Ok(())
}

async fn fetch_message(client: &Client, token: &str, id: &str) -> Result<GmailMessage> {
    client
        .get(format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}"))
        .bearer_auth(token)
        .query(&[("format", "full")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .with_context(|| format!("failed to fetch Gmail message {id}"))
}

async fn upsert_message(pool: &PgPool, message: &GmailMessage) -> Result<()> {
    let headers = headers(message);
    let body_text = normalized_body(message);
    let body_text_hash = body_text.as_ref().map(|body| hash_text(body));
    let internal_date = parse_internal_date(message.internal_date.as_deref())?;
    let raw = serde_json::to_value(message)?;

    sqlx::query(
        r#"
        INSERT INTO gmail_messages (
            gmail_message_id, gmail_thread_id, history_id, internal_date, rfc822_message_id,
            from_addr, to_addrs, cc_addrs, subject, snippet, label_ids, size_estimate,
            body_text, body_text_hash, raw_payload_json, ingested_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, NOW())
        ON CONFLICT (gmail_message_id) DO UPDATE SET
            gmail_thread_id = EXCLUDED.gmail_thread_id,
            history_id = EXCLUDED.history_id,
            internal_date = EXCLUDED.internal_date,
            rfc822_message_id = EXCLUDED.rfc822_message_id,
            from_addr = EXCLUDED.from_addr,
            to_addrs = EXCLUDED.to_addrs,
            cc_addrs = EXCLUDED.cc_addrs,
            subject = EXCLUDED.subject,
            snippet = EXCLUDED.snippet,
            label_ids = EXCLUDED.label_ids,
            size_estimate = EXCLUDED.size_estimate,
            body_text = EXCLUDED.body_text,
            body_text_hash = EXCLUDED.body_text_hash,
            raw_payload_json = EXCLUDED.raw_payload_json,
            ingested_at = NOW()
        "#,
    )
    .bind(&message.id)
    .bind(&message.thread_id)
    .bind(&message.history_id)
    .bind(internal_date)
    .bind(headers.get("message-id").cloned())
    .bind(headers.get("from").cloned())
    .bind(split_addresses(headers.get("to")))
    .bind(split_addresses(headers.get("cc")))
    .bind(headers.get("subject").cloned())
    .bind(&message.snippet)
    .bind(message.label_ids.clone().unwrap_or_default())
    .bind(message.size_estimate)
    .bind(body_text)
    .bind(body_text_hash)
    .bind(raw)
    .execute(pool)
    .await?;

    Ok(())
}

async fn process(pool: &PgPool, args: ProcessArgs) -> Result<()> {
    let query = if args.force {
        r#"
        SELECT m.gmail_message_id, m.gmail_thread_id, m.from_addr, m.subject, m.snippet, m.body_text
        FROM gmail_messages m
        WHERE NOT ('DRAFT' = ANY(m.label_ids))
        ORDER BY m.internal_date DESC NULLS LAST, m.ingested_at DESC
        LIMIT $1
        "#
    } else {
        r#"
        SELECT m.gmail_message_id, m.gmail_thread_id, m.from_addr, m.subject, m.snippet, m.body_text
        FROM gmail_messages m
        LEFT JOIN llm_extraction_attempts a
          ON a.gmail_message_id = m.gmail_message_id
         AND a.model = $1
         AND a.prompt_version = $2
        WHERE a.id IS NULL
          AND NOT ('DRAFT' = ANY(m.label_ids))
        ORDER BY m.internal_date DESC NULLS LAST, m.ingested_at DESC
        LIMIT $3
        "#
    };

    let rows = if args.force {
        sqlx::query(query).bind(args.limit).fetch_all(pool).await?
    } else {
        sqlx::query(query)
            .bind(&args.model)
            .bind(PROMPT_VERSION)
            .bind(args.limit)
            .fetch_all(pool)
            .await?
    };

    let client = Client::new();
    for row in rows {
        let gmail_message_id: String = row.get("gmail_message_id");
        let gmail_thread_id: String = row.get("gmail_thread_id");
        let subject: Option<String> = row.get("subject");
        let from_addr: Option<String> = row.get("from_addr");
        let snippet: Option<String> = row.get("snippet");
        let body_text: Option<String> = row.get("body_text");

        let prompt = extraction_prompt(
            from_addr.as_deref(),
            subject.as_deref(),
            snippet.as_deref(),
            body_text.as_deref(),
        );
        let is_later_status_update = is_later_status_update(
            subject.as_deref(),
            snippet.as_deref(),
            body_text.as_deref(),
        );

        let result = call_vllm(&client, &args.vllm_base_url, &args.model, &prompt).await;
        match result {
            Ok((raw, parsed)) => {
                let decision = extraction_decision(&parsed, is_later_status_update);
                upsert_attempt(pool, &gmail_message_id, &args.model, &raw, &parsed, &decision, None).await?;
        if !args.dry_run && decision == "tracked_applied" {
                    upsert_application(pool, &gmail_message_id, &gmail_thread_id, &parsed).await?;
                }
                println!("{gmail_message_id}: {decision}");
            }
            Err(err) => {
                println!("{gmail_message_id}: failed without recording attempt: {err}");
            }
        }
    }

    Ok(())
}

fn extraction_prompt(from: Option<&str>, subject: Option<&str>, snippet: Option<&str>, body: Option<&str>) -> String {
        let body = body.unwrap_or("").chars().take(500).collect::<String>();
        format!(
        r#"/no_think
You extract job application submission confirmations.

Return ONLY valid JSON with this exact shape:
{{
  "is_applied": boolean,
  "company": string|null,
  "role": string|null,
  "location": string|null,
  "job_posting_url": string|null,
  "confidence": number,
  "reason": string|null
}}

Rules:
- is_applied is true only when this email confirms the application was sent, submitted, received, or thanks me for applying.
- Do not mark recruiter outreach, newsletters, job alerts, interview scheduling, rejection emails, or later status updates as applied.
- If the email's main purpose is rejection or status update, is_applied must be false even if it mentions a previous application.
- If company or role is unclear, set is_applied false or use low confidence.
- Confidence must be between 0 and 1.

Email:
From: {}
Subject: {}
Snippet: {}
Body:
{}
"#,
        from.unwrap_or(""),
        subject.unwrap_or(""),
        snippet.unwrap_or(""),
        body
    )
}

async fn call_vllm(client: &Client, base_url: &str, model: &str, prompt: &str) -> Result<(Value, AppliedExtraction)> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let request = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a precise information extraction system. Output JSON only."},
            {"role": "user", "content": prompt}
        ],
        "temperature": 0,
        "max_tokens": 160
    });
    let raw: Value = error_for_status_with_body(client.post(url).json(&request).send().await?).await?.json().await?;
    let content = raw["choices"][0]["message"]["content"]
        .as_str()
        .context("vLLM response did not include choices[0].message.content")?;
    let json_content = extract_json_object(content)?;
    let parsed: AppliedExtraction = serde_json::from_str(&json_content)
        .with_context(|| format!("model returned non-JSON content: {content}"))?;
    Ok((raw, parsed))
}

fn extract_json_object(content: &str) -> Result<String> {
    let content = content
        .rsplit_once("</think>")
        .map(|(_, after)| after)
        .unwrap_or(content)
        .trim();
    let start = content
        .find('{')
        .context("model response did not contain a JSON object start")?;
    let end = content
        .rfind('}')
        .context("model response did not contain a JSON object end")?;
    if end < start {
        bail!("model response JSON delimiters were malformed");
    }
    Ok(content[start..=end].to_string())
}

fn extraction_decision(parsed: &AppliedExtraction, is_later_status_update: bool) -> &'static str {
    if is_later_status_update {
        return "skipped_later_status";
    }
    if !parsed.is_applied {
        return "skipped_not_application";
    }
    if parsed.confidence < 0.75 || blank(parsed.company.as_deref()) || blank(parsed.role.as_deref()) {
        return "skipped_uncertain";
    }
    "tracked_applied"
}

fn is_later_status_update(subject: Option<&str>, snippet: Option<&str>, body: Option<&str>) -> bool {
    let text = format!(
        "{}\n{}\n{}",
        subject.unwrap_or(""),
        snippet.unwrap_or(""),
        body.unwrap_or("")
    )
    .to_lowercase();

    let rejection_markers = [
        "we would like to inform you",
        "will not be moving forward",
        "not be moving forward",
        "not selected",
        "not proceed",
        "pursue other candidates",
        "after careful consideration",
        "unfortunately",
        "regret to inform",
    ];

    rejection_markers
        .iter()
        .any(|marker| text.contains(marker))
}

fn blank(value: Option<&str>) -> bool {
    value.map(|v| v.trim().is_empty()).unwrap_or(true)
}

async fn upsert_attempt(
    pool: &PgPool,
    gmail_message_id: &str,
    model: &str,
    raw: &Value,
    parsed: &AppliedExtraction,
    decision: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO llm_extraction_attempts (
            gmail_message_id, model, prompt_version, raw_response_json, parsed_company,
            parsed_role, parsed_location, parsed_job_posting_url, confidence, decision, error
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (gmail_message_id, model, prompt_version) DO UPDATE SET
            raw_response_json = EXCLUDED.raw_response_json,
            parsed_company = EXCLUDED.parsed_company,
            parsed_role = EXCLUDED.parsed_role,
            parsed_location = EXCLUDED.parsed_location,
            parsed_job_posting_url = EXCLUDED.parsed_job_posting_url,
            confidence = EXCLUDED.confidence,
            decision = EXCLUDED.decision,
            error = EXCLUDED.error,
            created_at = NOW()
        "#,
    )
    .bind(gmail_message_id)
    .bind(model)
    .bind(PROMPT_VERSION)
    .bind(raw)
    .bind(&parsed.company)
    .bind(&parsed.role)
    .bind(&parsed.location)
    .bind(&parsed.job_posting_url)
    .bind(parsed.confidence)
    .bind(decision)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

async fn upsert_application(pool: &PgPool, gmail_message_id: &str, gmail_thread_id: &str, parsed: &AppliedExtraction) -> Result<()> {
    let company = parsed.company.as_ref().context("tracked extraction missing company")?;
    let role = parsed.role.as_ref().context("tracked extraction missing role")?;
    let application_key = application_key(
        company,
        role,
        parsed.location.as_deref(),
        parsed.job_posting_url.as_deref(),
    );

    sqlx::query(
        r#"
        INSERT INTO job_applications (
            company, role, location, job_posting_url, status, source_gmail_message_id,
            gmail_thread_id, confidence, application_key, first_seen_at, updated_at
        )
        VALUES ($1, $2, $3, $4, 'applied', $5, $6, $7, $8, NOW(), NOW())
        ON CONFLICT (application_key) DO UPDATE SET
            source_gmail_message_id = EXCLUDED.source_gmail_message_id,
            gmail_thread_id = EXCLUDED.gmail_thread_id,
            confidence = GREATEST(job_applications.confidence, EXCLUDED.confidence),
            updated_at = NOW()
        "#,
    )
    .bind(company)
    .bind(role)
    .bind(&parsed.location)
    .bind(&parsed.job_posting_url)
    .bind(gmail_message_id)
    .bind(gmail_thread_id)
    .bind(parsed.confidence)
    .bind(application_key)
    .execute(pool)
    .await?;
    Ok(())
}

async fn inspect(pool: &PgPool, command: InspectCommand) -> Result<()> {
    match command {
        InspectCommand::Emails(args) => inspect_emails(pool, args.limit).await,
        InspectCommand::Email(args) => inspect_email(pool, &args.gmail_message_id).await,
        InspectCommand::Attempts(args) => inspect_attempts(pool, args.limit).await,
        InspectCommand::Applications(args) => inspect_applications(pool, args.limit).await,
    }
}

async fn inspect_emails(pool: &PgPool, limit: i64) -> Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT gmail_message_id, gmail_thread_id, internal_date, from_addr, subject
        FROM gmail_messages
        ORDER BY internal_date DESC NULLS LAST, ingested_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    for row in rows {
        let id: String = row.get("gmail_message_id");
        let thread: String = row.get("gmail_thread_id");
        let date: Option<DateTime<Utc>> = row.get("internal_date");
        let from: Option<String> = row.get("from_addr");
        let subject: Option<String> = row.get("subject");
        println!(
            "{} | thread={} | {} | {} | {}",
            id,
            thread,
            date.map(|d| d.to_rfc3339()).unwrap_or_else(|| "-".to_string()),
            from.unwrap_or_else(|| "-".to_string()),
            subject.unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(())
}

async fn inspect_email(pool: &PgPool, gmail_message_id: &str) -> Result<()> {
    let row = sqlx::query(
        r#"
        SELECT gmail_message_id, gmail_thread_id, internal_date, from_addr, to_addrs, cc_addrs,
               subject, snippet, label_ids, body_text
        FROM gmail_messages
        WHERE gmail_message_id = $1
        "#,
    )
    .bind(gmail_message_id)
    .fetch_optional(pool)
    .await?
    .with_context(|| format!("no Gmail message found for {gmail_message_id}"))?;

    let body: Option<String> = row.get("body_text");
    println!("id: {}", row.get::<String, _>("gmail_message_id"));
    println!("thread: {}", row.get::<String, _>("gmail_thread_id"));
    println!("internal_date: {:?}", row.get::<Option<DateTime<Utc>>, _>("internal_date"));
    println!("from: {:?}", row.get::<Option<String>, _>("from_addr"));
    println!("to: {:?}", row.get::<Vec<String>, _>("to_addrs"));
    println!("cc: {:?}", row.get::<Vec<String>, _>("cc_addrs"));
    println!("subject: {:?}", row.get::<Option<String>, _>("subject"));
    println!("labels: {:?}", row.get::<Vec<String>, _>("label_ids"));
    println!("snippet: {:?}", row.get::<Option<String>, _>("snippet"));
    println!("\nbody preview:\n{}", body.unwrap_or_default().chars().take(4000).collect::<String>());
    Ok(())
}

async fn inspect_attempts(pool: &PgPool, limit: i64) -> Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT gmail_message_id, decision, parsed_company, parsed_role, confidence, error, created_at
        FROM llm_extraction_attempts
        ORDER BY created_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    for row in rows {
        println!(
            "{} | {} | company={:?} | role={:?} | confidence={:?} | error={:?}",
            row.get::<String, _>("gmail_message_id"),
            row.get::<String, _>("decision"),
            row.get::<Option<String>, _>("parsed_company"),
            row.get::<Option<String>, _>("parsed_role"),
            row.get::<Option<f64>, _>("confidence"),
            row.get::<Option<String>, _>("error")
        );
    }
    Ok(())
}

async fn inspect_applications(pool: &PgPool, limit: i64) -> Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT company, role, location, job_posting_url, confidence, source_gmail_message_id, updated_at
        FROM job_applications
        ORDER BY updated_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    for row in rows {
        println!(
            "{} | {} | location={:?} | url={:?} | confidence={} | source={}",
            row.get::<String, _>("company"),
            row.get::<String, _>("role"),
            row.get::<Option<String>, _>("location"),
            row.get::<Option<String>, _>("job_posting_url"),
            row.get::<f64, _>("confidence"),
            row.get::<String, _>("source_gmail_message_id")
        );
    }
    Ok(())
}

async fn access_token(cfg: &AppConfig, oauth: &OAuthClient) -> Result<String> {
    let mut token = read_token(cfg)?;
    if token.expires_at_epoch > now_epoch() {
        return Ok(token.access_token);
    }

    let refresh_token = token
        .refresh_token
        .clone()
        .context("stored token has no refresh_token; rerun gmail-auth")?;
    let response: TokenResponse = Client::new()
        .post(&oauth.token_uri)
        .form(&[
            ("client_id", oauth.client_id.as_str()),
            ("client_secret", oauth.client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?
        .pipe(error_for_status_with_body)
        .await?
        .json()
        .await?;

    token.access_token = response.access_token;
    token.expires_at_epoch = now_epoch() + response.expires_in.unwrap_or(3600) - 60;
    token.scope = response.scope.or(token.scope);
    token.token_type = response.token_type.or(token.token_type);
    write_token(cfg, &token)?;
    Ok(token.access_token)
}

fn read_oauth_client(cfg: &AppConfig) -> Result<OAuthClient> {
    let text = fs::read_to_string(&cfg.client_path)
        .with_context(|| format!("missing OAuth client JSON at {}", cfg.client_path.display()))?;
    let file: OAuthClientFile = serde_json::from_str(&text)?;
    file.installed
        .or(file.web)
        .context("OAuth client JSON must contain installed or web client settings")
}

fn read_token(cfg: &AppConfig) -> Result<StoredToken> {
    let text = fs::read_to_string(&cfg.token_path)
        .with_context(|| format!("missing Gmail token at {}; run gmail-auth", cfg.token_path.display()))?;
    serde_json::from_str(&text).context("invalid stored Gmail token")
}

fn write_token(cfg: &AppConfig, token: &StoredToken) -> Result<()> {
    fs::create_dir_all(&cfg.dir)?;
    fs::write(&cfg.token_path, serde_json::to_vec_pretty(token)?)?;
    Ok(())
}

fn headers(message: &GmailMessage) -> HashMap<String, String> {
    message
        .payload
        .as_ref()
        .and_then(|payload| payload.headers.as_ref())
        .into_iter()
        .flatten()
        .map(|h| (h.name.to_lowercase(), h.value.clone()))
        .collect()
}

fn normalized_body(message: &GmailMessage) -> Option<String> {
    let mut chunks = Vec::new();
    if let Some(payload) = &message.payload {
        collect_text_parts(payload, &mut chunks);
    }
    let joined = chunks.join("\n\n");
    let normalized = normalize_ws(&strip_html(&joined));
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn collect_text_parts(payload: &GmailPayload, chunks: &mut Vec<String>) {
    let mime = payload.mime_type.as_deref().unwrap_or("");
    if (mime == "text/plain" || mime == "text/html") && payload.body.as_ref().and_then(|b| b.data.as_ref()).is_some() {
        if let Some(data) = payload.body.as_ref().and_then(|b| b.data.as_ref()) {
            if let Ok(bytes) = URL_SAFE_NO_PAD.decode(data) {
                if let Ok(text) = String::from_utf8(bytes) {
                    chunks.push(text);
                }
            }
        }
    }
    for part in payload.parts.as_deref().unwrap_or(&[]) {
        collect_text_parts(part, chunks);
    }
}

fn strip_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn normalize_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_addresses(value: Option<&String>) -> Vec<String> {
    value
        .map(|v| {
            v.split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_internal_date(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let millis = value.parse::<i64>()?;
    Ok(Some(Utc.timestamp_millis_opt(millis).single().context("invalid Gmail internalDate")?))
}

fn hash_text(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn application_key(company: &str, role: &str, location: Option<&str>, job_posting_url: Option<&str>) -> String {
    if let Some(url) = job_posting_url.and_then(|u| nonempty(u)).filter(|u| !is_generic_job_host(u)) {
        return format!("url:{}", normalize_key_part(url));
    }
    format!(
        "company:{}|role:{}|location:{}",
        normalize_key_part(company),
        normalize_key_part(role),
        location.map(normalize_key_part).unwrap_or_default()
    )
}

fn is_generic_job_host(url: &str) -> bool {
    let normalized = url.trim().trim_end_matches('/').to_lowercase();
    matches!(
        normalized.as_str(),
        "https://hire.lever.co"
            | "http://hire.lever.co"
            | "https://jobs.ashbyhq.com"
            | "http://jobs.ashbyhq.com"
            | "https://myworkdayjobs.com"
            | "http://myworkdayjobs.com"
    )
}

fn normalize_key_part(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn nonempty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

async fn error_for_status_with_body(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    bail!("HTTP {status}: {body}");
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}

impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_key_distinguishes_roles_at_same_company() {
        let one = application_key("Acme", "Backend Engineer", None, None);
        let two = application_key("Acme", "Data Engineer", None, None);
        assert_ne!(one, two);
    }

    #[test]
    fn application_key_prefers_url() {
        let key = application_key("Acme", "Backend Engineer", None, Some("https://jobs.example.com/123"));
        assert_eq!(key, "url:https jobs example com 123");
    }

    #[test]
    fn application_key_ignores_generic_job_host_url() {
        let key = application_key("Reply", "iOS Developer Intern", None, Some("https://hire.lever.co"));
        assert_eq!(key, "company:reply|role:ios developer intern|location:");
    }

    #[test]
    fn normalizes_whitespace() {
        assert_eq!(normalize_ws("a\n\n b\tc"), "a b c");
    }

    #[test]
    fn extracts_json_after_think_block() {
        let content = "<think>reasoning</think>\n{\"is_applied\":false}";
        assert_eq!(extract_json_object(content).unwrap(), "{\"is_applied\":false}");
    }

    #[test]
    fn detects_later_rejection_status_update() {
        assert!(is_later_status_update(
            Some("Thank you for your interest"),
            Some("We would like to inform you that we will not be moving forward"),
            None,
        ));
    }
}
