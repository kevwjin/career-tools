use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use chrono_tz::America::Los_Angeles;
use chrono_tz::Tz;
use clap::{Args, Parser, Subcommand};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

const GMAIL_SCOPES: &str =
    "https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.send";
const PROMPT_VERSION: &str = "applied-v1";

#[derive(Parser)]
#[command(name = "career-tools")]
#[command(about = "Local Gmail job application tracker")]
struct Cli {
    #[arg(
        long,
        env = "CAREER_TOOLS_DATABASE_URL",
        global = true,
        default_value = "postgres://career_tools:career_tools@localhost:5432/career_tools"
    )]
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
    Weekly(WeeklyArgs),
    Inspect {
        #[command(subcommand)]
        command: InspectCommand,
    },
    Report {
        #[command(subcommand)]
        command: ReportCommand,
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

    #[arg(long, default_value_t = 100)]
    limit: i64,
}

#[derive(Args)]
struct WeeklyArgs {
    #[arg(long, default_value_t = 192)]
    hours: u64,

    #[arg(long, default_value_t = 500)]
    limit: i64,
}

#[derive(Args)]
struct ProcessArgs {
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    #[arg(
        long,
        env = "CAREER_TOOLS_VLLM_BASE_URL",
        default_value = "http://127.0.0.1:8000/v1"
    )]
    vllm_base_url: String,

    #[arg(
        long,
        env = "CAREER_TOOLS_VLLM_MODEL",
        default_value = "Qwen/Qwen3-8B-AWQ"
    )]
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

#[derive(Subcommand)]
enum ReportCommand {
    Weekly(WeeklyReportArgs),
}

#[derive(Args)]
struct WeeklyReportArgs {
    #[arg(long, env = "CAREER_TOOLS_REPORT_TO")]
    to: Option<String>,

    #[arg(long, env = "CAREER_TOOLS_REPORT_CC")]
    cc: Option<String>,

    #[arg(long, default_value_t = false)]
    dry_run: bool,

    #[arg(long, default_value_t = false)]
    send: bool,

    #[arg(long, default_value_t = false)]
    force: bool,

    #[arg(long, default_value_t = false)]
    rolling: bool,
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

#[derive(Debug, Deserialize)]
struct GmailSendResponse {
    id: Option<String>,
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

#[derive(Debug, Clone)]
struct WeeklyApplication {
    submitted_at: DateTime<Utc>,
    company: String,
    role: String,
}

#[derive(Debug)]
struct WeeklyReport {
    week_start: NaiveDate,
    week_end: NaiveDate,
    daily_counts: Vec<i64>,
    applications: Vec<WeeklyApplication>,
}

#[derive(Debug)]
struct RenderedReport {
    subject: String,
    text: String,
    html: String,
    content_hash: String,
}

struct ReportSendRecord<'a> {
    report_type: &'a str,
    week_start: NaiveDate,
    week_end: NaiveDate,
    to_addrs: &'a [String],
    cc_addrs: &'a [String],
    content_hash: &'a str,
    gmail_message_id: Option<&'a str>,
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
                    limit: args.limit,
                    force: false,
                },
            )
            .await
        }
        Command::Weekly(args) => {
            let pool = connect_and_migrate(&cli.database_url).await?;
            ingest(&pool, &cfg, args.hours).await?;
            process(
                &pool,
                ProcessArgs {
                    dry_run: false,
                    vllm_base_url: "http://127.0.0.1:8000/v1".to_string(),
                    model: "Qwen/Qwen3-8B-AWQ".to_string(),
                    limit: args.limit,
                    force: false,
                },
            )
            .await
        }
        Command::Inspect { command } => {
            let pool = connect_and_migrate(&cli.database_url).await?;
            inspect(&pool, command).await
        }
        Command::Report { command } => {
            let pool = connect_and_migrate(&cli.database_url).await?;
            report(&pool, &cfg, command).await
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
        .timeout(StdDuration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => println!("vllm: ok"),
        Ok(resp) => println!("vllm: responded with {}", resp.status()),
        Err(err) => println!("vllm: unavailable: {err}"),
    }

    Ok(())
}

fn check_file(label: &str, path: &Path) {
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
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &oauth.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", GMAIL_SCOPES)
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
    let url = Url::parse(&callback_url)
        .context("callback URL must be the full localhost URL from the browser")?;
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

async fn exchange_code_for_token(
    oauth: &OAuthClient,
    code: &str,
    redirect_uri: &str,
) -> Result<StoredToken> {
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
        .checked_sub(StdDuration::from_secs(hours * 60 * 60))
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
        .get(format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{id}"
        ))
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
        let is_later_status_update =
            is_later_status_update(subject.as_deref(), snippet.as_deref(), body_text.as_deref());

        let result = call_vllm(&client, &args.vllm_base_url, &args.model, &prompt).await;
        match result {
            Ok((raw, parsed)) => {
                let decision = extraction_decision(&parsed, is_later_status_update);
                upsert_attempt(
                    pool,
                    &gmail_message_id,
                    &args.model,
                    &raw,
                    &parsed,
                    decision,
                    None,
                )
                .await?;
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

fn extraction_prompt(
    from: Option<&str>,
    subject: Option<&str>,
    snippet: Option<&str>,
    body: Option<&str>,
) -> String {
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

async fn call_vllm(
    client: &Client,
    base_url: &str,
    model: &str,
    prompt: &str,
) -> Result<(Value, AppliedExtraction)> {
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
    let raw: Value = error_for_status_with_body(client.post(url).json(&request).send().await?)
        .await?
        .json()
        .await?;
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
    if parsed.confidence < 0.75 || blank(parsed.company.as_deref()) || blank(parsed.role.as_deref())
    {
        return "skipped_uncertain";
    }
    "tracked_applied"
}

fn is_later_status_update(
    subject: Option<&str>,
    snippet: Option<&str>,
    body: Option<&str>,
) -> bool {
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

    rejection_markers.iter().any(|marker| text.contains(marker))
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

async fn upsert_application(
    pool: &PgPool,
    gmail_message_id: &str,
    gmail_thread_id: &str,
    parsed: &AppliedExtraction,
) -> Result<()> {
    let company = parsed
        .company
        .as_ref()
        .context("tracked extraction missing company")?;
    let role = parsed
        .role
        .as_ref()
        .context("tracked extraction missing role")?;
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

async fn report(pool: &PgPool, cfg: &AppConfig, command: ReportCommand) -> Result<()> {
    match command {
        ReportCommand::Weekly(args) => weekly_report(pool, cfg, args).await,
    }
}

async fn weekly_report(pool: &PgPool, cfg: &AppConfig, args: WeeklyReportArgs) -> Result<()> {
    if args.dry_run && args.send {
        bail!("use either --dry-run or --send, not both");
    }

    let to_addrs = parse_recipient_list(args.to.as_deref());
    let cc_addrs = parse_recipient_list(args.cc.as_deref());
    let send = args.send;
    if send && to_addrs.is_empty() {
        bail!("--send requires at least one --to recipient or CAREER_TOOLS_REPORT_TO");
    }

    let use_rolling_window = args.rolling || !send;
    let report_type = if use_rolling_window {
        "weekly_rolling"
    } else {
        "weekly"
    };
    let (week_start, week_end_exclusive) = if use_rolling_window {
        rolling_dry_run_window(Utc::now(), Los_Angeles)
    } else {
        previous_week_window(Utc::now(), Los_Angeles)?
    };
    let report = load_weekly_report(pool, week_start, week_end_exclusive, Los_Angeles).await?;
    let rendered = render_weekly_report(&report, Los_Angeles);

    if !send {
        println!("{}", rendered.text);
        return Ok(());
    }

    let week_end = week_end_exclusive - ChronoDuration::days(1);
    if !args.force
        && report_send_exists(pool, report_type, week_start, &to_addrs, &cc_addrs).await?
    {
        bail!(
            "weekly report for {} was already sent to {}; rerun with --force to resend",
            week_start,
            to_addrs.join(", ")
        );
    }

    let oauth = read_oauth_client(cfg)?;
    let token = access_token(cfg, &oauth).await?;
    let mime = build_mime_message(
        &to_addrs,
        &cc_addrs,
        &rendered.subject,
        &rendered.text,
        &rendered.html,
    );
    let gmail_id = send_gmail_message(&token, &mime).await?;
    record_report_send(
        pool,
        ReportSendRecord {
            report_type,
            week_start,
            week_end,
            to_addrs: &to_addrs,
            cc_addrs: &cc_addrs,
            content_hash: &rendered.content_hash,
            gmail_message_id: gmail_id.as_deref(),
        },
    )
    .await?;
    println!(
        "sent weekly report for {} to {}",
        week_start,
        to_addrs.join(", ")
    );
    Ok(())
}

async fn load_weekly_report(
    pool: &PgPool,
    week_start: NaiveDate,
    week_end_exclusive: NaiveDate,
    tz: Tz,
) -> Result<WeeklyReport> {
    let start_utc = local_midnight(tz, week_start)?.with_timezone(&Utc);
    let end_utc = local_midnight(tz, week_end_exclusive)?.with_timezone(&Utc);
    let rows = sqlx::query(
        r#"
        SELECT ja.company, ja.role, gm.internal_date
        FROM job_applications ja
        JOIN gmail_messages gm ON gm.gmail_message_id = ja.source_gmail_message_id
        WHERE gm.internal_date >= $1
          AND gm.internal_date < $2
        ORDER BY gm.internal_date ASC, ja.company ASC, ja.role ASC
        "#,
    )
    .bind(start_utc)
    .bind(end_utc)
    .fetch_all(pool)
    .await?;

    let mut applications = Vec::new();
    for row in rows {
        let submitted_at: Option<DateTime<Utc>> = row.get("internal_date");
        if let Some(submitted_at) = submitted_at {
            applications.push(WeeklyApplication {
                submitted_at,
                company: row.get("company"),
                role: row.get("role"),
            });
        }
    }

    let daily_counts = daily_counts(week_start, &applications, tz);
    Ok(WeeklyReport {
        week_start,
        week_end: week_end_exclusive - ChronoDuration::days(1),
        daily_counts,
        applications,
    })
}

fn render_weekly_report(report: &WeeklyReport, _tz: Tz) -> RenderedReport {
    let total: i64 = report.daily_counts.iter().sum();
    let mean = mean(&report.daily_counts);
    let active_days = report
        .daily_counts
        .iter()
        .filter(|count| **count > 0)
        .count();
    let subject = format!(
        "Career tools weekly report: {}-{}",
        report.week_start.format("%b %-d"),
        report.week_end.format("%b %-d, %Y")
    );

    let mut text = String::new();
    text.push_str("Hi Kevin,\n\n");
    text.push_str("It's time for your weekly career report! Let's see how well you did.\n\n");
    text.push_str(&format!(
        "You applied to a total of {total} roles, averaging {:.2} per day. You applied to roles during {active_days} of the 7 days last week.\n",
        mean
    ));
    if !report.applications.is_empty() {
        text.push_str("\nHere's what you applied to:\n\n");
        for (idx, app) in report.applications.iter().enumerate() {
            text.push_str(&format!("{}. {} - {}\n", idx + 1, app.company, app.role));
        }
    }
    text.push_str("\nBest,\nCareer-Bot\n");

    let mut html = String::new();
    html.push_str("<!doctype html><html><body>");
    html.push_str("<p>Hi Kevin,</p>");
    html.push_str("<p>It's time for your weekly career report! Let's see how well you did.</p>");
    html.push_str(&format!(
        "<p>You applied to a total of {total} roles, averaging {:.2} per day. You applied to roles during {active_days} of the 7 days last week.</p>",
        mean
    ));
    if !report.applications.is_empty() {
        html.push_str("<p>Here's what you applied to:</p>");
        html.push_str("<table><thead><tr><th>Company</th><th>Role</th></tr></thead><tbody>");
        for app in &report.applications {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                html_escape(&app.company),
                html_escape(&app.role)
            ));
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("<p>Best,<br>Career-Bot</p>");
    html.push_str("</body></html>");

    let content_hash = hash_text(&format!("{subject}\n{text}\n{html}"));
    RenderedReport {
        subject,
        text,
        html,
        content_hash,
    }
}

fn previous_week_window(now_utc: DateTime<Utc>, tz: Tz) -> Result<(NaiveDate, NaiveDate)> {
    let local_today = now_utc.with_timezone(&tz).date_naive();
    let days_since_monday = local_today.weekday().num_days_from_monday() as i64;
    let current_week_start = local_today - ChronoDuration::days(days_since_monday);
    let previous_week_start = current_week_start - ChronoDuration::days(7);
    Ok((previous_week_start, current_week_start))
}

fn rolling_dry_run_window(now_utc: DateTime<Utc>, tz: Tz) -> (NaiveDate, NaiveDate) {
    let local_today = now_utc.with_timezone(&tz).date_naive();
    let end_exclusive = local_today;
    let start = end_exclusive - ChronoDuration::days(7);
    (start, end_exclusive)
}

fn local_midnight(tz: Tz, date: NaiveDate) -> Result<DateTime<Tz>> {
    tz.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .with_context(|| format!("could not resolve local midnight for {date}"))
}

fn daily_counts(week_start: NaiveDate, applications: &[WeeklyApplication], tz: Tz) -> Vec<i64> {
    let mut counts = vec![0; 7];
    for app in applications {
        let local_date = app.submitted_at.with_timezone(&tz).date_naive();
        let offset = (local_date - week_start).num_days();
        if (0..7).contains(&offset) {
            counts[offset as usize] += 1;
        }
    }
    counts
}

fn mean(values: &[i64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<i64>() as f64 / values.len() as f64
}

fn parse_recipient_list(value: Option<&str>) -> Vec<String> {
    let mut recipients = value
        .unwrap_or("")
        .split(',')
        .map(|recipient| recipient.trim().to_lowercase())
        .filter(|recipient| !recipient.is_empty())
        .collect::<Vec<_>>();
    recipients.sort();
    recipients.dedup();
    recipients
}

fn build_mime_message(
    to_addrs: &[String],
    cc_addrs: &[String],
    subject: &str,
    text: &str,
    html: &str,
) -> String {
    let boundary = format!(
        "career-tools-{}",
        hash_text(subject).chars().take(16).collect::<String>()
    );
    let mut message = String::new();
    message.push_str(&format!("To: {}\r\n", to_addrs.join(", ")));
    if !cc_addrs.is_empty() {
        message.push_str(&format!("Cc: {}\r\n", cc_addrs.join(", ")));
    }
    message.push_str(&format!("Subject: {}\r\n", subject));
    message.push_str("MIME-Version: 1.0\r\n");
    message.push_str(&format!(
        "Content-Type: multipart/alternative; boundary=\"{}\"\r\n",
        boundary
    ));
    message.push_str("\r\n");
    message.push_str(&format!("--{}\r\n", boundary));
    message.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    message.push_str("Content-Transfer-Encoding: 8bit\r\n\r\n");
    message.push_str(text);
    message.push_str("\r\n");
    message.push_str(&format!("--{}\r\n", boundary));
    message.push_str("Content-Type: text/html; charset=\"UTF-8\"\r\n");
    message.push_str("Content-Transfer-Encoding: 8bit\r\n\r\n");
    message.push_str(html);
    message.push_str("\r\n");
    message.push_str(&format!("--{}--\r\n", boundary));
    message
}

async fn send_gmail_message(token: &str, mime: &str) -> Result<Option<String>> {
    let raw = URL_SAFE_NO_PAD.encode(mime.as_bytes());
    let response: GmailSendResponse = Client::new()
        .post("https://gmail.googleapis.com/gmail/v1/users/me/messages/send")
        .bearer_auth(token)
        .json(&json!({ "raw": raw }))
        .send()
        .await?
        .pipe(error_for_status_with_body)
        .await?
        .json()
        .await?;
    Ok(response.id)
}

async fn report_send_exists(
    pool: &PgPool,
    report_type: &str,
    week_start: NaiveDate,
    to_addrs: &[String],
    cc_addrs: &[String],
) -> Result<bool> {
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM report_sends
            WHERE report_type = $1
              AND week_start = $2
              AND to_addrs = $3
              AND cc_addrs = $4
        )
        "#,
    )
    .bind(report_type)
    .bind(week_start)
    .bind(to_addrs)
    .bind(cc_addrs)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

async fn record_report_send(pool: &PgPool, record: ReportSendRecord<'_>) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO report_sends (
            report_type, week_start, week_end, to_addrs, cc_addrs,
            content_hash, gmail_message_id, sent_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
        ON CONFLICT (report_type, week_start, to_addrs, cc_addrs) DO UPDATE SET
            week_end = EXCLUDED.week_end,
            content_hash = EXCLUDED.content_hash,
            gmail_message_id = EXCLUDED.gmail_message_id,
            sent_at = NOW()
        "#,
    )
    .bind(record.report_type)
    .bind(record.week_start)
    .bind(record.week_end)
    .bind(record.to_addrs)
    .bind(record.cc_addrs)
    .bind(record.content_hash)
    .bind(record.gmail_message_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
            date.map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "-".to_string()),
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
    println!(
        "internal_date: {:?}",
        row.get::<Option<DateTime<Utc>>, _>("internal_date")
    );
    println!("from: {:?}", row.get::<Option<String>, _>("from_addr"));
    println!("to: {:?}", row.get::<Vec<String>, _>("to_addrs"));
    println!("cc: {:?}", row.get::<Vec<String>, _>("cc_addrs"));
    println!("subject: {:?}", row.get::<Option<String>, _>("subject"));
    println!("labels: {:?}", row.get::<Vec<String>, _>("label_ids"));
    println!("snippet: {:?}", row.get::<Option<String>, _>("snippet"));
    println!(
        "\nbody preview:\n{}",
        body.unwrap_or_default()
            .chars()
            .take(4000)
            .collect::<String>()
    );
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
    let text = fs::read_to_string(&cfg.token_path).with_context(|| {
        format!(
            "missing Gmail token at {}; run gmail-auth",
            cfg.token_path.display()
        )
    })?;
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
    if (mime == "text/plain" || mime == "text/html")
        && let Some(data) = payload.body.as_ref().and_then(|b| b.data.as_ref())
        && let Ok(bytes) = URL_SAFE_NO_PAD.decode(data)
        && let Ok(text) = String::from_utf8(bytes)
    {
        chunks.push(text);
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
    Ok(Some(
        Utc.timestamp_millis_opt(millis)
            .single()
            .context("invalid Gmail internalDate")?,
    ))
}

fn hash_text(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn application_key(
    company: &str,
    role: &str,
    location: Option<&str>,
    job_posting_url: Option<&str>,
) -> String {
    if let Some(url) = job_posting_url
        .and_then(|u| nonempty(u))
        .filter(|u| !is_generic_job_host(u))
    {
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
        let key = application_key(
            "Acme",
            "Backend Engineer",
            None,
            Some("https://jobs.example.com/123"),
        );
        assert_eq!(key, "url:https jobs example com 123");
    }

    #[test]
    fn application_key_ignores_generic_job_host_url() {
        let key = application_key(
            "Reply",
            "iOS Developer Intern",
            None,
            Some("https://hire.lever.co"),
        );
        assert_eq!(key, "company:reply|role:ios developer intern|location:");
    }

    #[test]
    fn normalizes_whitespace() {
        assert_eq!(normalize_ws("a\n\n b\tc"), "a b c");
    }

    #[test]
    fn extracts_json_after_think_block() {
        let content = "<think>reasoning</think>\n{\"is_applied\":false}";
        assert_eq!(
            extract_json_object(content).unwrap(),
            "{\"is_applied\":false}"
        );
    }

    #[test]
    fn detects_later_rejection_status_update() {
        assert!(is_later_status_update(
            Some("Thank you for your interest"),
            Some("We would like to inform you that we will not be moving forward"),
            None,
        ));
    }

    #[test]
    fn previous_week_window_uses_pacific_calendar_week() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 16, 0, 0).single().unwrap();
        let (start, end_exclusive) = previous_week_window(now, Los_Angeles).unwrap();
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 5, 25).unwrap());
        assert_eq!(end_exclusive, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
    }

    #[test]
    fn rolling_dry_run_window_ends_yesterday() {
        let now = Utc.with_ymd_and_hms(2026, 6, 3, 16, 0, 0).single().unwrap();
        let (start, end_exclusive) = rolling_dry_run_window(now, Los_Angeles);
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 5, 27).unwrap());
        assert_eq!(end_exclusive, NaiveDate::from_ymd_opt(2026, 6, 3).unwrap());
    }

    #[test]
    fn computes_daily_counts_and_summary_stats() {
        let week_start = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        let applications = vec![
            WeeklyApplication {
                submitted_at: Los_Angeles
                    .with_ymd_and_hms(2026, 5, 25, 9, 0, 0)
                    .single()
                    .unwrap()
                    .with_timezone(&Utc),
                company: "A".to_string(),
                role: "One".to_string(),
            },
            WeeklyApplication {
                submitted_at: Los_Angeles
                    .with_ymd_and_hms(2026, 5, 25, 10, 0, 0)
                    .single()
                    .unwrap()
                    .with_timezone(&Utc),
                company: "B".to_string(),
                role: "Two".to_string(),
            },
            WeeklyApplication {
                submitted_at: Los_Angeles
                    .with_ymd_and_hms(2026, 5, 27, 10, 0, 0)
                    .single()
                    .unwrap()
                    .with_timezone(&Utc),
                company: "C".to_string(),
                role: "Three".to_string(),
            },
        ];

        let counts = daily_counts(week_start, &applications, Los_Angeles);
        assert_eq!(counts, vec![2, 0, 1, 0, 0, 0, 0]);
        assert_eq!(mean(&counts), 3.0 / 7.0);
        assert_eq!(counts.iter().copied().max().unwrap(), 2);
        assert_eq!(counts.iter().filter(|count| **count > 0).count(), 2);
    }

    #[test]
    fn render_report_preserves_application_order() {
        let report = WeeklyReport {
            week_start: NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            week_end: NaiveDate::from_ymd_opt(2026, 5, 31).unwrap(),
            daily_counts: vec![0, 1, 0, 1, 0, 0, 0],
            applications: vec![
                WeeklyApplication {
                    submitted_at: Los_Angeles
                        .with_ymd_and_hms(2026, 5, 26, 9, 0, 0)
                        .single()
                        .unwrap()
                        .with_timezone(&Utc),
                    company: "Base Power Company".to_string(),
                    role: "Software Engineering Intern".to_string(),
                },
                WeeklyApplication {
                    submitted_at: Los_Angeles
                        .with_ymd_and_hms(2026, 5, 28, 9, 0, 0)
                        .single()
                        .unwrap()
                        .with_timezone(&Utc),
                    company: "Apple".to_string(),
                    role: "Software Engineering Masters Internships".to_string(),
                },
            ],
        };

        let rendered = render_weekly_report(&report, Los_Angeles);
        let base_idx = rendered.text.find("Base Power Company").unwrap();
        let apple_idx = rendered.text.find("Apple").unwrap();
        assert!(base_idx < apple_idx);
        assert!(rendered.text.contains("Hi Kevin,"));
        assert!(rendered.text.contains("You applied to a total of 2 roles"));
        assert!(rendered.text.contains("during 2 of the 7 days"));
        assert!(
            rendered
                .text
                .contains("1. Base Power Company - Software Engineering Intern")
        );
        assert!(
            rendered
                .text
                .contains("2. Apple - Software Engineering Masters Internships")
        );
        assert!(!rendered.html.contains("<strong>"));
        assert!(rendered.html.contains("<table>"));
        assert!(rendered.html.contains("<th>Company</th><th>Role</th>"));
    }

    #[test]
    fn render_report_skips_application_list_when_empty() {
        let report = WeeklyReport {
            week_start: NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
            week_end: NaiveDate::from_ymd_opt(2026, 5, 31).unwrap(),
            daily_counts: vec![0, 0, 0, 0, 0, 0, 0],
            applications: vec![],
        };

        let rendered = render_weekly_report(&report, Los_Angeles);
        assert!(rendered.text.contains("You applied to a total of 0 roles"));
        assert!(!rendered.text.contains("Here's what you applied to"));
        assert!(!rendered.html.contains("<table>"));
    }

    #[test]
    fn normalizes_recipients_for_duplicate_detection() {
        let recipients = parse_recipient_list(Some(
            "Friend@Example.com, me@example.com, friend@example.com",
        ));
        assert_eq!(recipients, vec!["friend@example.com", "me@example.com"]);
    }

    #[test]
    fn mime_message_contains_recipients_subject_and_parts() {
        let mime = build_mime_message(
            &["me@example.com".to_string()],
            &["friend@example.com".to_string()],
            "Weekly Report",
            "plain text",
            "<p>html</p>",
        );
        assert!(mime.contains("To: me@example.com\r\n"));
        assert!(mime.contains("Cc: friend@example.com\r\n"));
        assert!(mime.contains("Subject: Weekly Report\r\n"));
        assert!(mime.contains("Content-Type: text/plain; charset=\"UTF-8\""));
        assert!(mime.contains("Content-Type: text/html; charset=\"UTF-8\""));
    }
}
