# Hermes Agent — Rust Port 🦀

Port Rust dari [Hermes Agent](https://github.com/NousResearch/hermes-agent) (Nous Research, MIT) —
agen AI otonom yang *self-improving*: memiliki learning loop, memori persisten lintas sesi,
dan dapat dijangkau dari CLI maupun messaging gateway.

> Status (2026-10-19): paritas fungsional dengan versi aslinya untuk semua subsistem
> utama — tools (51), provider (7), gateway (8 platform), backend exec (8),
> evals, skills hub, TUI. Tersisa: aplikasi desktop Electron (butuh Tauri) dan
> segelintir tools yang terikat platform/desktop asli (Yuanbao, utilitas desktop).
> Integrasi pihak ketiga diverifikasi via mock server (Telegram, Discord, Slack,
> Matrix, Teams, WhatsApp) dan Chrome headless asli (browser tools); **gateway
> Telegram terverifikasi live end-to-end** melawan Bot API asli
> (`HERMES_TEST_LIVE_TELEGRAM=1 cargo test -p hermes-gateway --test live_telegram`).
>
> Build otomatis: setiap push/PR menjalankan `cargo build`, `clippy -D warnings`,
> dan `cargo test --workspace` via GitHub Actions (`.github/workflows/rust.yml`),
> plus cross-compile release untuk Linux ARM64 (`aarch64-unknown-linux-gnu`)
> dan ARMv7 (`armv7-unknown-linux-gnueabihf`).

## Fitur

| Fitur Hermes asli | Status di port Rust |
|---|---|
| Agent loop (think → tool call → observasi) | ✅ `hermes-core::Agent` |
| Provider model-agnostic (OpenRouter, Nous Portal, OpenAI, Ollama, vLLM, ...) | ✅ OpenAI-compatible streaming + Anthropic + **Gemini native** + **Azure OpenAI** + **AWS Bedrock (SigV4)** |
| Credential pool + rotasi & cooldown API key | ✅ |
| 60+ built-in tools | ✅ **51 tool**: `exec` (+`background`), `execute_code`, file tools, `patch`, `glob`, `grep`, `web_fetch`, `web_search`, `x_search`, `memory_*`, `skill_*`, `session_search`, `cron_*`, `gateway_send`, `react_to_message`, `datetime_now`, `image_generate`, `text_to_speech`, `transcribe_audio`, `video_generate`, `video_analyze`, `vision_analyze`, `todo_list`, `clarify`, `delegate_task`, `process_manage`, 13× `browser_*` (CDP), 5× `feishu_*` |
| Backend eksekusi (local/docker/ssh/daytona) | ✅ 8 backend: local, docker, ssh, **daytona** (REST), **modal** (CLI), **singularity/apptainer** (CLI), **vercel** (node sandbox) |
| Persistent memory + FTS5 lintas sesi | ✅ SQLite (`hermes-memory`) |
| Skills system (SKILL.md, self-improving) | ✅ `hermes-skills` + learning loop otomatis + **skills hub** (`hermes skills hub search/install`) |
| Evals | ✅ `hermes eval run` (suite JSONL + `builtin:tools`, skor kelulusan) |
| Kompresi konteks otomatis | ✅ ringkasan via LLM saat melewati ambang |
| MCP client (stdio + HTTP) | ✅ tool MCP dijembatani sebagai `mcp__<server>__<tool>` |
| Messaging gateway | ✅ 8 platform: Telegram (long-polling), Discord (Gateway WS), Slack (Socket Mode), WhatsApp (Cloud API + webhook), **Matrix** (Client-Server API), **Signal** (via signal-cli), **Teams** (Bot Framework + webhook), **Email** (SMTP + IMAP polling) |
| Bot mode (multi-bot, profil sendiri) | ✅ `[[bots]]` di config + `hermes gateway --bot <nama>` |
| Webhook masuk / keluar | ✅ `hermes webhook serve` (POST /hook); `webhooks.outgoing` untuk event gateway & cron |
| Cron scheduler + pengiriman ke platform | ✅ `hermes cron` |
| Voice mode | ✅ `hermes voice say|listen`; `/voice <teks>` di Telegram |
| ACP server untuk IDE | ✅ `hermes acp` (JSON-RPC via stdio: initialize/session/prompt) |
| Batch processing + trajectory export | ✅ `hermes batch --input prompts.jsonl`; `hermes session export` |
| TUI interaktif | ✅ `hermes tui` (Ratatui — padanan Ink TUI: streaming, input, scroll, status bar) |
| Security: approval bertingkat | ✅ ask/allow/deny + aturan per tool |
| Profil ganda | ✅ `~/.hermes/config.toml` |
| CLI `hermes` | ✅ chat, run, skills, memory, config, cron, gateway, mcp, model, doctor, setup, version, voice, acp, batch, session, webhook, **tui**, **eval** |
| Desktop app (Electron) | ⏳ belum — butuh framework GUI desktop terpisah (kandidat: Tauri) |

## Arsitektur

```
crates/
  hermes-core/       tipe pesan, trait port, config, agent loop, kompresi, budget
  hermes-providers/  klien LLM streaming (OpenAI-compat, Anthropic, Gemini, Azure, Bedrock/SigV4) + credential pool
  hermes-tools/      trait Tool + registry + tool bawaan (shell, fs, web, memori, skill)
  hermes-memory/     SQLite + FTS5: memori & sesi
  hermes-skills/     manajer SKILL.md (procedural memory)
  hermes-mcp/        klien MCP + jembatan tool
  hermes-gateway/    trait Platform + Telegram + registry + bot mode
  hermes-cron/       job store + scheduler + tool cron_*
  hermes-cli/        biner `hermes`
```

## Instalasi

```bash
# Butuh Rust stable (rustup)
cargo build --release -p hermes-cli
sudo install -m755 target/release/hermes /usr/local/bin/hermes
```

## Mulai cepat

```bash
hermes setup          # wizard: pilih provider, masukkan API key, pilih model
hermes doctor         # diagnosa konfigurasi
hermes chat           # obrolan interaktif
hermes run -q "ringkas file README.md" --yes
```

## Konfigurasi

`~/.hermes/config.toml` (dibuat otomatis) dan `~/.hermes/.env` untuk API key:

```toml
[[providers]]
name = "openrouter"
protocol = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[profiles.default]
provider = "openrouter"
model = "nousresearch/hermes-4-70b"
working_dir = "/home/chewy/proyek"

[mcp_servers.docs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-docs"]
```

## Gateway (Telegram, Discord, Slack, WhatsApp)

```bash
export TELEGRAM_BOT_TOKEN=123456:ABC...
hermes gateway --platform telegram

export DISCORD_BOT_TOKEN=...   # aktifkan intent MESSAGE_CONTENT
hermes gateway --platform discord

export SLACK_BOT_TOKEN=xoxb-... SLACK_APP_TOKEN=xapp-...
hermes gateway --platform slack  # socket mode

export WHATSAPP_TOKEN=... WHATSAPP_PHONE_NUMBER_ID=...
hermes gateway --platform whatsapp --webhook-port 8080
```

Kirim pesan ke bot; tiap chat mendapat sesi agen sendiri yang persisten. Perintah `/new` memulai sesi baru,
`/voice <teks>` (Telegram) mengirim voice note. Mode multi-bot: daftarkan `[[bots]]` di `config.toml`
lalu `hermes gateway --bot <nama>`. Event balasan & cron dapat diteruskan ke URL di `webhooks.outgoing`.

Webhook masuk generik: `hermes webhook serve --port 8080`, lalu `POST /hook {"text": "..."}`.

## Cron

```bash
hermes cron add briefing-pagi "0 0 7 * * *" \
  --prompt "Buatkan briefing pagi dari berita teknologi terbaru" \
  --platform telegram --target 123456789
hermes cron serve   # jalankan scheduler
```

## Voice, ACP, Batch

```bash
hermes voice say "Halo, ini Hermes" --out halo.mp3
hermes voice listen rekaman.ogg --language id
hermes acp                       # server ACP via stdio untuk Zed/IDE
hermes batch --input prompts.jsonl --out hasil.jsonl
hermes session export --session <id> --format trajectory
```

## Pengembangan

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
# Test browser terhadap Chrome nyata (headless di :9222):
HERMES_TEST_REAL_BROWSER=1 cargo test -p hermes-tools real_browser
```

### Verifikasi tanpa kredensial (2026-10-19)

Integrasi pihak ketiga diverifikasi tanpa token asli melalui mock server
lokal dan layanan nyata yang bisa dijalankan offline:

- **Gateway** (`hermes-gateway/tests/mock_platforms.rs`, 9 test): Telegram
  (kirim + penolakan `{"ok": false}`), Discord (auth `Bot`), Slack (Bearer),
  Matrix (kirim + alur `/sync` → terima → balas), Teams (token OAuth +
  activities), WhatsApp (kirim + verifikasi webhook benar/salah + terima →
  balas). Base URL tiap API bisa di-override via env
  (`TELEGRAM_API_BASE`, `DISCORD_API_BASE`, `SLACK_API_BASE`,
  `WHATSAPP_API_BASE`, `TEAMS_TOKEN_URL`).
- **Browser CDP**: mock WebSocket server untuk roundtrip protokol, plus test
  terhadap **Google Chrome headless nyata** (navigate, snapshot, exec, click
  dengan verifikasi perubahan DOM, get_images, screenshot PNG).
- **TUI**: dijalankan di bawah PTY semu (`script`), terverifikasi render UI,
  input keyboard, alur kirim → thinking → error provider, dan keluar bersih
  via Esc.
- **Bug yang ditemukan test ini**: Telegram tidak menganggap
  `{"ok": false}` sebagai error (sudah diperbaiki).

## Lisensi

MIT — sama seperti Hermes Agent asli oleh Nous Research.
