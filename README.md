# career-tools

Local job application tracker that reads Gmail, asks a local Qwen model to identify job application submission confirmations, and stores tracked applications in Postgres.

This is intentionally local-first:

- Gmail access uses the Gmail API with read-only OAuth.
- LLM inference runs locally through vLLM.
- The model is `Qwen/Qwen3-8B-AWQ`.
- Postgres is the source of truth.
- CSV export and a web UI are not part of this stage.

## Current Behavior

The app tracks only emails showing that an application was sent/submitted/received, such as:

- "Thanks for applying"
- "Thank you for your application"
- "Thanks for applying to ..."
- application receipt/confirmation emails from Lever, Ashby, Workday, company career systems, etc.

The app does not track later lifecycle emails as new applications:

- rejection emails
- interview scheduling
- recruiter outreach
- job alerts
- newsletters
- status updates
- Gmail drafts

There is a deterministic phrase guard for obvious rejection/status-update language. That guard runs before trusting the model's final decision, so a rejection email that mentions a prior application is skipped as `skipped_later_status`.

## Requirements

- Rust/Cargo
- Docker with Compose
- `uv`
- NVIDIA GPU with working driver/CUDA visibility
- Google Cloud project with Gmail API enabled
- OAuth Desktop client JSON for Gmail API

The working local GPU setup used:

- GPU: NVIDIA GeForce RTX 3060 Ti, 8 GB VRAM
- Driver: 535.309.01
- CUDA reported by driver: 12.2
- vLLM: 0.10.2
- Transformers: 4.56.1
- Model: `Qwen/Qwen3-8B-AWQ`

Check GPU visibility:

```bash
nvidia-smi
```

## Google Cloud / Gmail API Setup

1. Open Google Cloud Console.
2. Select or create a project.
3. Enable the Gmail API.
4. Configure Google Auth Platform / OAuth consent:
   - App can stay in testing mode.
   - Add your Gmail account as a test user.
   - Use read-only Gmail scope:

```text
https://www.googleapis.com/auth/gmail.readonly
```

5. Create an OAuth client:
   - Type: Desktop app
   - Name: `career-tools local`

6. Download the client JSON and save it outside the repo:

```bash
mkdir -p ~/.config/career-tools
mv ~/Downloads/client_secret_*.json ~/.config/career-tools/google-oauth-client.json
```

If the JSON download fails, create the file manually:

```json
{
  "installed": {
    "client_id": "PASTE_CLIENT_ID_HERE",
    "client_secret": "PASTE_CLIENT_SECRET_HERE",
    "auth_uri": "https://accounts.google.com/o/oauth2/auth",
    "token_uri": "https://oauth2.googleapis.com/token"
  }
}
```

Save it as:

```text
~/.config/career-tools/google-oauth-client.json
```

## Postgres Setup

Start local Postgres:

```bash
docker compose up -d
```

Apply migrations:

```bash
cargo run -- db migrate
```

Default database URL:

```text
postgres://career_tools:career_tools@localhost:5432/career_tools
```

Override with:

```bash
export CAREER_TOOLS_DATABASE_URL='postgres://...'
```

## Gmail OAuth

Normal flow:

```bash
cargo run -- gmail-auth
```

The app opens a Google OAuth page and listens on localhost for the callback.

If your browser cannot reach the callback because the browser and CLI are not on the same localhost, use manual callback mode:

1. Run:

```bash
cargo run -- gmail-auth
```

2. Complete the Google consent screen.
3. Let the browser fail on `127.0.0.1`.
4. Copy the full failed localhost URL from the browser address bar.
5. Stop the waiting CLI with `Ctrl-C`.
6. Run:

```bash
cargo run -- gmail-auth --callback-url 'PASTE_FULL_LOCALHOST_URL_HERE'
```

The token is stored outside the repo:

```text
~/.config/career-tools/google-token.json
```

## Ingest Gmail

Pull the last 48 hours:

```bash
cargo run -- ingest --hours 48
```

Daily default uses a 26-hour overlap:

```bash
cargo run -- daily
```

Ingestion details:

- Gmail query uses `after:<epoch_seconds> -in:drafts`.
- Epoch seconds avoid Gmail date-string timezone surprises.
- Drafts are excluded.
- Messages are deduped by Gmail message ID.
- Thread ID is stored only for context and never dedupes applications.

Inspect ingested messages:

```bash
cargo run -- inspect emails --limit 10
cargo run -- inspect email <gmail_message_id>
```

## vLLM / Qwen Setup

Create and configure the vLLM environment:

```bash
uv venv .venv-vllm --python 3.12
uv pip install --python .venv-vllm/bin/python 'vllm==0.10.2' --torch-backend cu128
uv pip install --python .venv-vllm/bin/python 'transformers==4.56.1'
```

Verify:

```bash
.venv-vllm/bin/python -c 'import torch; print(torch.__version__); print(torch.cuda.is_available()); print(torch.cuda.get_device_name(0))'
.venv-vllm/bin/python -c 'import transformers, tokenizers, vllm; print(vllm.__version__); print(transformers.__version__); print(tokenizers.__version__)'
```

Expected shape:

```text
torch cuda available: True
GPU: NVIDIA GeForce RTX 3060 Ti
vllm: 0.10.2
transformers: 4.56.1
tokenizers: 0.22.2
```

Start Qwen:

```bash
scripts/serve-qwen-vllm.sh
```

The launch script uses a conservative 8 GB VRAM profile:

- disables FlashInfer sampler because this machine does not have `nvcc` available
- uses `PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True`
- uses `--max-model-len 1024`
- uses `--gpu-memory-utilization 0.80`
- uses `--max-num-seqs 1`
- uses `--max-num-batched-tokens 512`
- uses `--enforce-eager`

Confirm server health:

```bash
curl http://127.0.0.1:8000/v1/models
```

Expected model:

```text
Qwen/Qwen3-8B-AWQ
```

## Processing Emails

Dry-run processing:

```bash
cargo run -- process --dry-run --limit 10
```

Force retry/reprocess a batch:

```bash
cargo run -- process --dry-run --limit 10 --force
```

Write application rows:

```bash
cargo run -- process --limit 100 --force
```

Inspect model decisions:

```bash
cargo run -- inspect attempts --limit 20
```

Inspect tracked applications:

```bash
cargo run -- inspect applications --limit 20
```

## Decisions

Extraction attempts use these decisions:

- `tracked_applied`: this email is a submission/receipt confirmation and can create/update an application row.
- `skipped_not_application`: this email is not a submission confirmation.
- `skipped_uncertain`: Qwen output was too uncertain or lacked company/role.
- `skipped_later_status`: deterministic guard saw rejection/status-update language.

Only `tracked_applied` creates or updates `job_applications`.

## Idempotency

Email ingestion:

- `gmail_message_id` is unique.
- Re-ingesting the same Gmail message updates the stored message instead of duplicating it.

Application tracking:

- Gmail thread ID is never used as an application dedupe key.
- Applications dedupe by normalized company + role + optional location.
- Specific job posting URLs can be used as keys.
- Generic host URLs like `https://hire.lever.co` are ignored for dedupe because they are not job-specific.

This allows multiple roles at the same company and multiple applications in the same Gmail thread.

## Useful Commands

Preflight:

```bash
cargo run -- preflight
```

Run the daily pipeline:

```bash
cargo run -- daily
```

Use a different vLLM endpoint:

```bash
CAREER_TOOLS_VLLM_BASE_URL='http://127.0.0.1:8000/v1' cargo run -- process --dry-run
```

Use a different model name:

```bash
CAREER_TOOLS_VLLM_MODEL='Qwen/Qwen3-8B-AWQ' cargo run -- process --dry-run
```

## Cron

Example daily cron entry:

```cron
0 9 * * * cd /home/kevwjin/workspace/01-projects/career-tools && /home/kevwjin/.cargo/bin/cargo run -- daily >> /tmp/career-tools-daily.log 2>&1
```

Run vLLM separately before the cron job, or manage `scripts/serve-qwen-vllm.sh` with your preferred process manager.

## Current Limitations

- Qwen/vLLM must already be running before `process` or `daily`.
- The vLLM profile is tuned for an 8 GB 3060 Ti and short prompts.
- Body text is truncated before sending to the model.
- The rejection/status guard is phrase-based substring matching, not a full classifier.
- There is no CSV export yet.
- There is no UI yet.
- `cargo fmt` was not available in the current Rust toolchain during setup.

## Troubleshooting

### `nvidia-smi` says driver/library version mismatch

The NVIDIA kernel driver and user-space library are out of sync. Reboot first:

```bash
sudo reboot
```

### vLLM latest wheel asks for `libcudart.so.13`

Use the pinned vLLM setup in this README. The latest vLLM wheel may expect a newer CUDA runtime than this machine has.

### vLLM cannot find `nvcc`

Use `scripts/serve-qwen-vllm.sh`, which disables FlashInfer sampling.

### vLLM runs out of memory

Use the included launch script. It lowers context and batching so `Qwen/Qwen3-8B-AWQ` fits on the 3060 Ti.

### OAuth callback opens localhost but Safari cannot connect

Use manual callback mode:

```bash
cargo run -- gmail-auth --callback-url 'PASTE_FULL_LOCALHOST_URL_HERE'
```

### Google says the app is blocked

Add your Gmail address as a test user in Google Auth Platform / Audience.

### Postgres connection refused

Start the container:

```bash
docker compose up -d
```

