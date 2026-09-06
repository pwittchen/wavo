# wavo

AI agent for music processing.

Send a Telegram message in plain language — *"oddziel wokal od Queen - Bohemian
Rhapsody"*, *"slow that song down to 80%"* — and wavo works out which
[demix](https://github.com/pwittchen/demix) operations that means, runs them,
publishes the result to [plainsong](https://github.com/pwittchen/plainsong), and
replies with a link to the new track and a link to the whole collection.

wavo understands Polish and English, and answers in the language you wrote in.

The design is written down in [SPEC.md](SPEC.md); this file is how to run it.

```
Telegram ──long poll──▶ wavo ──stdio──▶ demix-mcp ──▶ demix (spleeter, ffmpeg, yt-dlp)
                         │
                         ├──HTTP──▶ an OpenAI-compatible chat completions API
                         └──HTTP──▶ plainsong  ──▶  your music collection
```

One Rust binary, no database, no queue, no web UI of its own. The only
authentication is an allow-list of Telegram chat IDs.

## quick start

You need a Telegram bot token from [@BotFather](https://t.me/BotFather), an API
key for OpenAI (or any OpenAI-compatible provider), and Docker.

```sh
git clone git@github.com:pwittchen/wavo.git
git clone git@github.com:pwittchen/plainsong.git   # sibling checkout, see below
cd wavo
cp .env.example .env
$EDITOR .env                 # tokens, and the chat IDs allowed to use the bot
docker compose up -d --build
docker compose logs -f wavo
```

Then message the bot. If you do not know your chat ID yet, send it anything: the
bot stays silent for strangers, but writes the rejected chat ID to the log —

```
INFO wavo::telegram: ignored a message from a chat that is not allowed chat_id=123456789
```

— which is the number to put in `WAVO_ALLOWED_CHAT_IDS`.

> plainsong does not publish a container image yet, so `docker-compose.yml`
> builds it from a sibling checkout (`../plainsong`). If you have an image
> instead, replace the `build:` line with `image: …` or put the change in a
> git-ignored `docker-compose.override.yml`.

> The image carries TensorFlow (via spleeter) and is 2–3 GB. Those wheels exist
> for `linux/amd64` only, so on Apple Silicon build with
> `docker compose build --build-arg BUILDPLATFORM=linux/amd64` or set
> `DOCKER_DEFAULT_PLATFORM=linux/amd64`. The first separation downloads ~300 MB
> of spleeter models into the `wavo-work` volume, where they stay.

## talking to it

Anything that is not a command is a request for the model:

| you say | wavo does |
| --- | --- |
| separate vocals from Queen - Bohemian Rhapsody | 2-stem run, publishes the vocals |
| oddziel wokal od Queen - Bohemian Rhapsody | the same run, and answers in Polish |
| slow that song down to 80% | reuses the song from the conversation, `tempo=0.8` |
| przetransponuj go do a-moll | `target_key=Am` |
| cut 1:00 to 2:30 and give me the instrumental | `start`/`end` plus a 2-stem run |

Commands are answered by wavo itself, without the model:

| command | effect |
| --- | --- |
| `/start`, `/help` | what it can do, with examples |
| `/status` | whether a job is running, the queue, uptime |
| `/tracks` | the ten most recent tracks in plainsong |
| `/reset` | forget this chat's conversation |
| `/cancel` | stop after the current step |

While a job runs, the acknowledgement message is edited in place with the stage
and the elapsed time, and turns into the final answer when the run finishes.

## configuration

Environment variables only, validated at startup; a missing or malformed one is
a startup failure naming the variable.

| variable | default | required | description |
| --- | --- | --- | --- |
| `TELEGRAM_BOT_TOKEN` | — | ✔ | Bot token from BotFather |
| `WAVO_ALLOWED_CHAT_IDS` | — | ✔ | Comma-separated numeric chat IDs allowed to use the bot |
| `OPENAI_API_KEY` | — | ✔ | API key of OpenAI or an OpenAI-compatible provider |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | | Swap for OpenRouter, vLLM, Ollama, … |
| `OPENAI_MODEL` | `gpt-5-mini` | | Main orchestration model |
| `OPENAI_MODEL_FALLBACK` | `gpt-5` | | Used only after repeated invalid tool calls |
| `PLAINSONG_URL` | `http://plainsong:8080` | | Internal API base |
| `PLAINSONG_PUBLIC_URL` | = `PLAINSONG_URL` | | Base used in the links sent to users |
| `PLAINSONG_TOKEN` | — | ✔ | Bearer token for plainsong's mutating endpoints |
| `PLAINSONG_MAX_UPLOAD_MB` | `100` | | Client-side guard; keep in sync with plainsong |
| `WAVO_WORK_DIR` | `/work` | | Job outputs and the spleeter model cache |
| `WAVO_MAX_CONCURRENT_JOBS` | `1` | | Parallel demix runs |
| `WAVO_MAX_TOOL_ITERATIONS` | `8` | | Tool-call iterations per turn |
| `WAVO_TURN_TIMEOUT_SEC` | `1800` | | Wall clock per turn |
| `WAVO_JOB_TIMEOUT_SEC` | `1200` | | Wall clock per `process_audio` |
| `WAVO_LLM_TIMEOUT_SEC` | `90` | | LLM request timeout |
| `WAVO_HISTORY_TURNS` | `12` | | Retained user/assistant pairs per chat |
| `WAVO_SESSION_TTL_MIN` | `120` | | Idle session eviction |
| `WAVO_TOOL_OUTPUT_CHARS` | `2000` | | Truncation of tool stdout/stderr |
| `WAVO_PROGRESS_INTERVAL_SEC` | `5` | | Minimum gap between progress edits |
| `WAVO_KEEP_JOB_FILES` | `false` | | Keep job directories after publishing |
| `WAVO_EXPOSE_CLEAN` | `false` | | Expose demix's `clean` tool to the model |
| `WAVO_ALLOW_DELETE` | `false` | | Expose `delete_track` to the model |
| `WAVO_HEALTH_ADDR` | `0.0.0.0:8081` | | Health endpoint bind address |
| `WAVO_SHUTDOWN_GRACE_SEC` | `30` | | Graceful shutdown window |
| `RUST_LOG` | `wavo=info` | | Log filter |
| `DEMIX_YT_DLP_ARGS` | — | | Passed through to demix, e.g. `--cookies-from-browser` |

Two more exist for tests and unusual setups, and are not part of the deployment
surface: `WAVO_MCP_COMMAND` (default `demix-mcp`) and `TELEGRAM_API_BASE`
(default `https://api.telegram.org`).

`GET /healthz` on `WAVO_HEALTH_ADDR` answers `{"ok":true,"mcp":"up","plainsong":"up"}`
when the MCP child is alive and plainsong answered within the last minute, and
`503` otherwise. It is the container health check; no port is published for it.

## development

```sh
cargo test                 # unit and integration tests, no network, no API keys
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

To run the binary outside Docker you need `demix`, `demix-mcp`, `ffmpeg` and
`yt-dlp` on `PATH` — the startup checks refuse to start without them — plus a
plainsong to talk to:

```sh
TELEGRAM_BOT_TOKEN=… WAVO_ALLOWED_CHAT_IDS=… OPENAI_API_KEY=… \
PLAINSONG_URL=http://127.0.0.1:8080 PLAINSONG_TOKEN=… \
WAVO_WORK_DIR=./work RUST_LOG=wavo=debug cargo run
```

The end-to-end test is `#[ignore]`d because it needs a real demix install and a
running plainsong:

```sh
PLAINSONG_TOKEN=… cargo test --test e2e_smoke -- --ignored --nocapture
```

Everything else runs offline: the OpenAI and plainsong clients are tested against
a stub HTTP server, and the MCP layer against `tests/fixtures/stub_mcp_server.py`,
a ~100-line MCP server that also knows how to die mid-call so the restart policy
can be exercised. No test calls the real OpenAI API, the real Telegram API or
YouTube.

## how it is put together

| file | what lives there |
| --- | --- |
| `src/config.rs` | environment parsing; `Secret` redacts tokens in `Debug` |
| `src/telegram/` | long polling, commands, HTML escaping, the bilingual string table |
| `src/llm/` | chat completions, the tool-calling loop, the system prompt |
| `src/mcp/` | the `demix-mcp` child process, tool discovery, schema conversion |
| `src/tools/` | the dispatch table, the plainsong client, the path guard |
| `src/session.rs` | per-chat history and trimming |
| `src/jobs.rs` | job directories, the concurrency permit, progress messages |

Three rules are worth knowing when reading it:

- **The model never picks a path.** `cwd` and `output_dir` are stripped from the
  MCP schemas and injected by wavo; `publish_track` accepts only keys from the
  current turn's path table, and the resolved path is checked against the job
  directory after canonicalization, so a symlink out of it is refused too.
- **Tool failures are results, not errors.** A failed demix run comes back as
  `{"ok": false, "error": "…"}` so the model can correct itself, with the full
  stderr in the log and a classified, human sentence for the user.
- **wavo composes its own links.** The track and listing links are appended by
  wavo after the model's reply, so a hallucinated URL cannot reach a user.

## where this deviates from SPEC.md

- **`src/lib.rs` exists.** The spec's layout lists only `main.rs`; the modules
  are a library so the integration tests can drive them without a bot token.
  `main.rs` is the wiring layer on top.
- **Progress stages are what wavo asked for, plus elapsed time.** `demix-mcp`
  runs demix to completion and returns its output in one go, so there is no
  progress to read. wavo shows `downloading` while a remote source is being
  fetched, switches to `separating stems` / `applying effects` after a minute,
  shows `uploading` when it publishes, and refreshes the elapsed time every
  `WAVO_PROGRESS_INTERVAL_SEC`. The transition is an estimate; the elapsed time
  is not.
- **`temperature` is dropped when a model rejects it.** The spec asks for
  `temperature: 0.2`; the `gpt-5*` family answers an explicit temperature with a
  400. wavo sends it, and on that one error stops sending it for the rest of the
  process rather than failing every turn.
- **plainsong is built, not pulled.** See the note in the quick start.
- **Two extra test files.** `tests/plainsong_client.rs`, `tests/mcp_client.rs`
  and `tests/tool_dispatch.rs` cover the integration level §14 asks for but the
  layout in §11 does not list.

## license

Copyright 2026 Piotr Wittchen, released under the Apache License 2.0. See
[LICENSE](LICENSE).
