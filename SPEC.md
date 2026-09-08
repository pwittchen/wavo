# wavo — Specification

AI agent for music processing.

A user sends a request in natural language over Telegram ("slow down Bohemian Rhapsody
by 20% and give me just the vocals"). wavo turns that into calls to
[demix](https://github.com/pwittchen/demix) via its MCP server, publishes the resulting
audio to [plainsong](https://github.com/pwittchen/plainsong) over its REST API, and
replies with a link to the new track and a link to the whole collection.

The guiding principle, inherited from plainsong, is **simplicity**. No database, no
queue broker, no web UI of its own, no user accounts. A single Rust binary that talks to
three things: the Telegram Bot API, an LLM over HTTP, and one MCP server over stdio.
Every decision below should be re-checked against that rule: if something can be dropped
without losing a listed requirement, it gets dropped.

---

## 1. Scope

### In scope

- A Telegram bot that accepts free-form natural-language music requests
- An LLM-driven tool-calling loop over the demix MCP tools plus wavo's own publish tools
- Running demix (download / search YouTube, stem separation, tempo, pitch, key detection
  and transposition, cutting) through the `demix-mcp` MCP server over stdio
- Uploading the produced audio to plainsong via `POST /api/tracks` with a bearer token
- Replying with a per-track link and a link to the full listing
- Progress feedback while a long job runs
- An allow-list of Telegram chat IDs — the bot answers nobody else
- Bilingual operation: requests are understood in Polish and English, and every reply is
  in the language the user wrote in
- One `docker compose up` that starts plainsong and wavo (agent + demix + demix-mcp)

### Out of scope (explicitly not built)

- A web UI or REST API of wavo's own (beyond a health endpoint)
- User accounts, registration, roles, per-user quotas or billing
- A database, an ORM, migrations, a message broker, a job store
- Persisting conversation history across restarts
- Telegram webhooks (long polling only), inline queries, group-chat threading
- Voice-note or audio-file *input* from Telegram (URLs and search queries only, see §5.2)
- Playlists, tagging, artwork, lyrics, metadata enrichment
- Multi-tenant deployment, horizontal scaling, distributed job scheduling
- Modifying demix or plainsong — wavo consumes them as they are
- Any music acquisition wavo is not entitled to perform: wavo is a personal tool for
  processing material the operator has the right to process; it ships no bulk
  downloading, no library scraping, and no sharing beyond the operator's own plainsong

---

## 2. Technology

| Layer | Choice | Why |
| --- | --- | --- |
| Language | Rust (stable, 2021 edition) | Matches plainsong; one static binary, small runtime |
| Async runtime | `tokio` | Long polling, child process I/O, timers |
| HTTP client | `reqwest` (rustls, multipart, json) | Telegram, OpenAI and plainsong are all plain HTTP |
| Telegram | Bot API over `reqwest`, long polling | Only `getUpdates`, `sendMessage`, `editMessageText` are needed; a bot framework would be more dependency than feature |
| LLM | OpenAI `POST /v1/chat/completions` over `reqwest` | Widest compatibility, incl. OpenAI-compatible servers |
| MCP client | [`rmcp`](https://crates.io/crates/rmcp) (official Rust MCP SDK), stdio child-process transport | Spawns `demix-mcp`, handles the JSON-RPC framing |
| Serialization | `serde` / `serde_json` | |
| IDs | `uuid` (v4) for job directories | |
| Logging | `tracing` + `tracing-subscriber` (JSON or plain) | |
| Errors | `anyhow` at the edges, `thiserror` for typed domain errors | |
| Config | Environment variables only, parsed at startup | Same style as plainsong |

Deliberately **not** used: `async-openai` (thin wrapper over three endpoints we call
directly, and it constrains `base_url` swapping), `teloxide` (a dialogue framework for
three API methods), `axum` (only a health endpoint is served — a bare `tokio::net`
listener answering a fixed response is enough).

> If `rmcp`'s API proves unstable, the fallback is a hand-written MCP client: the
> subset wavo needs is `initialize`, `tools/list` and `tools/call` as newline-delimited
> JSON-RPC 2.0 over the child's stdin/stdout — roughly 200 lines. This is a documented
> escape hatch, not the plan.

---

## 3. Architecture

### 3.1 Components

```
                    Telegram Bot API
                           │  long poll (getUpdates)
                           ▼
┌──────────────────────── wavo container ────────────────────────┐
│                                                                │
│   wavo (Rust binary)                                           │
│     ├── telegram   long polling, sending, progress edits       │
│     ├── session    per-chat message history (in memory)        │
│     ├── llm        OpenAI chat.completions + tool-call loop    │
│     ├── mcp        rmcp client ──stdio──▶ demix-mcp (py3.10)   │
│     │                                        │ subprocess      │
│     │                                        ▼                 │
│     │                                    demix CLI (py3.8)     │
│     │                                    ffmpeg / yt-dlp /     │
│     │                                    spleeter / essentia   │
│     ├── tools      native tools: publish_track, list_tracks…   │
│     └── health     GET /healthz on 0.0.0.0:8081                │
│                                                                │
│   /work  ── job outputs + spleeter model cache (volume)        │
└────────────────────────────────────────────────────────────────┘
      │ downloads, optionally             │ HTTPS/HTTP + Bearer token
      │ through a proxy (§9)              │
      ▼                                   ▼
    YouTube                             plainsong container  ──▶  /data volume
```

### 3.2 Why demix runs in the same container

`demix-mcp` speaks MCP over **stdio only**; it has no HTTP/SSE transport. A stdio MCP
server has to be a child process of its client, so demix, `demix-mcp` and the wavo
binary all live in one image. This also means demix's output files are on a filesystem
wavo can read directly, with no shared-volume choreography between containers.

The cost is a large image (TensorFlow via spleeter) and no independent scaling of
audio processing. Both are acceptable for a single-operator tool. If demix-mcp ever
grows a streamable-HTTP transport, splitting it into its own container becomes a
configuration change (`WAVO_MCP_TRANSPORT=stdio|http`) rather than a redesign.

### 3.3 Request lifecycle

1. `getUpdates` returns a message from an allow-listed chat.
2. If the message is only half a request, it waits for its other half (§5.6); otherwise
   — and once the halves are joined — wavo appends it to that chat's session history and
   acknowledges with a "thinking…" message it will later edit.
3. The LLM is called with the system prompt, the history, and the tool catalogue
   (§6). It either answers directly or requests one or more tool calls.
4. Tool calls are dispatched (§6.3). Long ones (`process_audio`) acquire the job
   semaphore, run in a blocking task, and drive progress edits on the Telegram message.
5. Tool results are truncated (§6.4), appended to the history, and the loop repeats
   until the model answers without tool calls, or the iteration/time budget is hit.
6. The final assistant text is sent to the chat. If tracks were published during the
   turn, wavo appends the links block (§5.4) itself — it is not left to the model.
7. The job directory is removed unless `WAVO_KEEP_JOB_FILES=true`.

### 3.4 Concurrency

- One in-flight turn per chat. A second message from the same chat while a turn is
  running is answered with a short "still working on the previous request" note and
  dropped — not queued. Messages that arrive *before* a turn starts are a different
  case: they are collected into one request (§5.6).
- A global `tokio::sync::Semaphore` with `WAVO_MAX_CONCURRENT_JOBS` permits (default
  `1`) guards `process_audio`. Spleeter is memory- and CPU-hungry; parallel runs on a
  small VPS make everything slower and can OOM. While a turn waits for a permit the
  user is told its queue position.
- The MCP client is shared. `demix-mcp` handles one call at a time; requests are
  serialized by the same semaphore, so no extra locking is required beyond `rmcp`'s own.

---

## 4. Sessions and conversation state

A **session** is per Telegram chat:

| Field | Type | Description |
| --- | --- | --- |
| `chat_id` | i64 | Telegram chat ID, the key |
| `messages` | Vec\<ChatMessage\> | OpenAI-shaped history: system, user, assistant, tool |
| `last_activity` | Instant | For eviction |
| `busy` | bool | A turn is in flight |
| `published` | Vec\<PublishedTrack\> | Tracks published during the current turn |

- Held in a `HashMap<i64, Session>` behind a `tokio::sync::RwLock`.
- History is trimmed to the last `WAVO_HISTORY_TURNS` user/assistant pairs (default
  `12`), always keeping the system prompt first. Tool result blocks older than the
  most recent two turns are replaced by a one-line summary, since demix output is the
  bulkiest thing in the history and the least useful after the fact.
- Sessions idle for `WAVO_SESSION_TTL_MIN` minutes (default `120`) are evicted.
- **Nothing is persisted.** A restart starts every conversation fresh. This is a
  deliberate simplification; if it becomes painful, the smallest fix is a JSON file per
  chat on the `/work` volume, written after each turn.

---

## 5. Telegram interface

### 5.1 Transport

Long polling: `GET /bot<token>/getUpdates?timeout=30&offset=<last+1>&allowed_updates=["message"]`.
No public ingress is needed, which is the point — the container can sit behind NAT with
no reverse proxy. The offset is kept in memory and re-derived from the first poll after
a restart (Telegram redelivers unconfirmed updates).

### 5.2 Accepted input

- Plain text messages. Everything else (photos, documents, stickers, audio uploads,
  voice notes) gets a one-line "I only understand text for now" reply.
- Commands, handled by wavo without involving the LLM:

| Command | Effect |
| --- | --- |
| `/start`, `/help` | Short capability blurb with two or three examples |
| `/status` | Whether a job is running, queue length, uptime |
| `/tracks` | Ten most recent plainsong tracks with links |
| `/reset` | Clears this chat's session history |
| `/cancel` | Requests cancellation of the running turn (§8.3), or drops a request still waiting for its other half (§5.6) |

- Anything else is a natural-language request handed to the LLM.

### 5.3 Authorization

`WAVO_ALLOWED_CHAT_IDS` is a comma-separated allow-list of numeric chat IDs. Messages
from any other chat are logged at `info` (with the chat ID, so the operator can add
themselves) and answered with nothing at all — no error reply, so the bot is silent for
strangers. An empty or unset value is a **startup failure**, not "allow everyone".

### 5.4 Replies

- The bot replies in the language of the user's message — see §5.5.
- Progress: the initial acknowledgement message is edited in place, at most once every
  `WAVO_PROGRESS_INTERVAL_SEC` seconds (default `5`, Telegram rate limits are
  unforgiving), with the current stage — `downloading`, `separating stems`,
  `applying effects`, `uploading`.
- On success the reply ends with a links block wavo composes itself:

```
✅ Bohemian Rhapsody — vocals (0.8×, −2 semitones)
🎵 https://music.example.com/track.html?id=7f1c…
📚 all songs: https://music.example.com/
```

- Messages are truncated to Telegram's 4096-character limit, with demix logs never
  pasted verbatim — errors are summarized into one or two human sentences (§8.2).
- Formatting uses `parse_mode=HTML` with escaping of all interpolated values; MarkdownV2
  is avoided because its escaping rules are a recurring source of send failures.

### 5.5 Language

wavo is bilingual: it understands requests written in **Polish** and in **English**, and
it answers in the language the user wrote in. Polish in → Polish out; English in →
English out. Nothing is translated for the user and no language is ever announced.

- **Detection is per message, by the model.** The system prompt (§6.2) instructs the
  model to mirror the language of the most recent user message. There is no language
  detection library, no `lang` field on the session, and no `/lang` command — this is
  the simplicity rule of the intro applied to a problem an LLM already solves.
- **Language may switch mid-conversation.** A chat that started in Polish and continues
  in English gets English replies from that message onwards, with the history intact —
  the previous song and settings are still in scope.
- **A mixed-language message** (Polish sentence with English song titles or terms like
  "vocals", "bpm") is treated as Polish: the dominant language of the sentence wins, and
  proper nouns, song titles and stem names are never translated.
- **When the language is genuinely undeterminable** — a bare URL, a bare song title,
  an emoji — the model replies in the language of the last message in the chat that had
  one, and in English if the chat has none.
- **Strings wavo composes itself are bilingual too**, since they never pass through the
  model: `/start` and `/help` blurbs, `/status` output, progress stages (§5.4), queue
  position notices, the "still working on the previous request" note, the "I only
  understand text for now" reply, and the classified demix errors of §8.2 all exist in a
  Polish and an English variant. They are picked using the language of the triggering
  user message, falling back to the chat's last known language and then to English.
  These live in `telegram/format.rs` as a small `&str` lookup keyed by
  `(message_id, Lang)` — an enum with two variants, not an i18n framework.
- The links block of §5.4 is language-neutral apart from its one label, which follows
  the same rule (`all songs:` / `wszystkie utwory:`).

---

### 5.6 Coalescing half-requests

People send a link and then say what to do with it, or say it first and paste the link
after. Handled one message at a time, both halves are wrong: the first starts a run with
wavo's defaults before the instruction arrives, and whichever half starts first makes
the other bounce off the one-turn-per-chat rule of §3.4.

So a message that is only **half** a request is held for `WAVO_COALESCE_WINDOW_SEC`
(default `15`, `0` disables the whole mechanism) instead of starting a turn:

- A request is whole when it names **what to work on** — a link, a song named in words,
  or the song already under discussion in this chat — **and says what to do with it**.
- A message that is only a link, only a song title, or only an instruction ("usuń wokal")
  is half a request and starts the window.
- Every further message from that chat joins the buffer and re-opens the window. As soon
  as what has accumulated is whole, the turn starts immediately — no waiting out the rest
  of the window.
- When the window closes on a still-incomplete buffer, it runs **as it stands**: a lone
  link is processed with the defaults of §6.2, a lone instruction gets the clarifying
  question. Nothing is ever held indefinitely.
- The halves reach the model as **one user message**, joined by newlines in the order
  they were sent, so the history holds one request rather than two fragments.
- The reply language is decided by the joined text (§5.5), not by the half that happened
  to arrive first — a bare link followed by a Polish instruction gets a Polish reply.
- `/reset` and `/cancel` drop a buffer that is still waiting; `/cancel` says so, since
  nothing was running to cancel.

The word lists that decide "instruction" from "song name" are a heuristic and are meant
to stay one: they only decide *when* to start, never what to do. A song called "Karaoke"
read as an instruction, or an instruction wavo has no word for, costs one window of
waiting and nothing else.

---

## 6. LLM layer

### 6.1 Provider and models

OpenAI Chat Completions (`POST {OPENAI_BASE_URL}/chat/completions`, default base
`https://api.openai.com/v1`), authenticated with `Authorization: Bearer $OPENAI_API_KEY`.
Chat Completions is chosen over the Responses API because every OpenAI-compatible
server (OpenRouter, vLLM, Ollama, LM Studio) implements it, which keeps a local or
alternative model a config change.

The requirement is "as cheap as possible, but not so cheap that it breaks". This is an
orchestration task: read one sentence, pick the right demix arguments, chain two or
three tool calls, summarize. That is well within the small tier, and the plan is a
**two-model split**, both configurable:

| Role | Env var | Default | Rationale |
| --- | --- | --- | --- |
| Main orchestration loop | `OPENAI_MODEL` | `gpt-5-mini` | Reliable multi-step tool calling at small-tier pricing |
| Escalation on failure | `OPENAI_MODEL_FALLBACK` | `gpt-5` | Used only after the main model produces two consecutive invalid tool calls in one turn (§8.1) |

The nano tier (`gpt-5-nano`) is available by setting `OPENAI_MODEL` and is fine for
single-step requests, but it is not the default: it drops arguments in multi-tool chains
often enough that the retries cost more than the model saves. Escalation is expected to
be rare, so the effective cost stays at the small-tier rate.

> Model names and prices move. Treat the table as defaults, not as facts: check
> OpenAI's current model and pricing pages before assuming what a run costs. Everything
> here is env-configurable precisely so this table can go stale without a code change.

Request parameters: `temperature: 0.2` (deterministic argument construction),
`max_completion_tokens: 1500`, `parallel_tool_calls: true`, `tool_choice: "auto"`,
`user: <chat_id>` for abuse-tracking on the provider side. Requests are not streamed —
Telegram cannot render a token stream, and progress comes from the tool layer instead.

### 6.2 System prompt

Maintained as a single `const &str` in the source (not a template file — there is one).
It states:

- What wavo is and what the tools do, in two sentences.
- The rule that audio is produced with demix tools and published with `publish_track`;
  the model never invents URLs or claims a track was published.
- How to pick the source: a YouTube link in the message goes into `process_audio`'s
  `url` verbatim (never shortened, re-encoded or turned into a search, and never sent
  through `search_youtube`, which is for titles); a song named in words goes into
  `search`; a message that is only a link is a complete request. The link stays the
  source for follow-ups in the same conversation.
- Defaults to apply when the user is vague: `mode=nosplit` unless stems are asked for,
  `2stems` when the user says "karaoke", "instrumental", "backing track" or "vocals",
  no tempo or pitch change unless requested.
- That `target_key` and `transpose` are mutually exclusive.
- That it must ask a clarifying question instead of guessing when the request names no
  identifiable song.
- That it must interpret requests in Polish and in English, and reply in the language of
  the most recent user message — Polish for a Polish message, English for an English one
  (§5.5) — briefly, without markdown tables or emoji spam, and without pasting file paths
  or command lines. Clarifying questions and error explanations follow the same rule.
- That song titles, artist names and stem names are never translated, and that Polish
  music vocabulary maps onto the same tool arguments as its English equivalent
  ("wokal"/"vocals", "podkład"/"instrumental", "zwolnij"/"slow down",
  "przetransponuj"/"transpose").
- That every processed file worth keeping should be published — for multi-stem modes,
  publish the stems the user actually asked for, not all of them.
- That `publish_track` takes the title in three parts — `artist`, `title` and
  `modification` — which wavo joins itself (§7): the artist and the song alone, never
  translated and never pre-joined, and the modification ("bez wokalu", "vocals removed",
  "tonacja a-moll", "slowed down to 80%", "oryginał" / "original" when nothing changed)
  in the language of the user's most recent message, per file rather than per run.

### 6.3 Tool catalogue

Two sources, merged into one OpenAI `tools` array.

**From the demix MCP server** (discovered at startup via `tools/list`, so the catalogue
follows demix without a wavo release):

| Tool | Purpose |
| --- | --- |
| `process_audio` | Download/load, cut, separate stems, tempo, transpose, target key |
| `detect_key` | Detect the musical key of a local file |
| `search_youtube` | Resolve a search query to a YouTube URL and title |
| `clean` | Remove demix output and/or cached spleeter models |

MCP tool schemas are JSON Schema already, so conversion to OpenAI function definitions
is mechanical: `{name, description, input_schema}` → `{type:"function", function:{name,
description, parameters}}`. Two adjustments are applied by wavo before exposing them:

- `cwd` and `output_dir` are **stripped from the schema** and injected by wavo. The
  model must not choose where files land.
- `clean` is exposed only when `WAVO_EXPOSE_CLEAN=true` (default `false`). Cleaning
  models costs a ~300 MB re-download; it is an operator action, not a chat action.

**Native wavo tools:**

| Tool | Arguments | Returns |
| --- | --- | --- |
| `publish_track` | `path` (string, from a `process_audio` result), `artist`, `title`, `modification` (strings, joined by wavo — see §7) | `{id, track_url, all_tracks_url, size_bytes}` |
| `list_tracks` | `q` (optional string) | Up to 10 `{id, title, track_url}` |
| `delete_track` | `id` (string) | `{ok}` — exposed only when `WAVO_ALLOW_DELETE=true` (default `false`) |

### 6.4 Tool result handling

demix results are large and mostly noise to a language model. Before a result goes back
into the history:

- `stdout` / `stderr` are truncated to `WAVO_TOOL_OUTPUT_CHARS` (default `2000`), head
  and tail kept with a `…[N chars omitted]…` marker in the middle.
- The `files` map is reduced to audio files only (`.mp3`, `.wav`, `.flac`), with the
  paths made relative to the job directory. Video files are dropped unless the request
  asked for video.
- The absolute-path table is kept **outside** the model's context, in the turn state,
  keyed by the relative path the model sees. `publish_track` resolves through that
  table, so the model can never name a path wavo did not produce.
- Failures are returned as `{"ok": false, "error": "…"}` tool results — never as an
  exception that aborts the turn — so the model can correct itself.

### 6.5 Budgets

| Budget | Env var | Default |
| --- | --- | --- |
| Tool-call iterations per turn | `WAVO_MAX_TOOL_ITERATIONS` | `8` |
| Wall clock per turn | `WAVO_TURN_TIMEOUT_SEC` | `1800` |
| Wall clock per `process_audio` | `WAVO_JOB_TIMEOUT_SEC` | `1200` |
| LLM request timeout | `WAVO_LLM_TIMEOUT_SEC` | `90` |

Exceeding a budget ends the turn with a plain-language apology and a `warn` log
carrying the chat ID, the iteration count and the last tool called.

---

## 7. plainsong integration

wavo is an ordinary API client of plainsong; it never touches plainsong's data
directory.

| Operation | Call |
| --- | --- |
| Publish | `POST {PLAINSONG_URL}/api/tracks`, `multipart/form-data` with `file` and `title`, `Authorization: Bearer $PLAINSONG_TOKEN` |
| List | `GET {PLAINSONG_URL}/api/tracks?q=…` (public, no auth) |
| Delete | `DELETE {PLAINSONG_URL}/api/tracks/{id}`, bearer token |

- `PLAINSONG_URL` is the internal address used for API calls (`http://plainsong:8080`
  inside the compose network). `PLAINSONG_PUBLIC_URL` is what goes into links sent to
  the user; it defaults to `PLAINSONG_URL` and is set to the externally reachable origin
  in real deployments.
- Track URL: `{PLAINSONG_PUBLIC_URL}/track.html?id={id}`. Listing URL:
  `{PLAINSONG_PUBLIC_URL}/`.
- Before uploading, wavo checks the file is under `PLAINSONG_MAX_UPLOAD_MB` (default
  `100`, kept in sync with plainsong's own limit) and has an accepted extension. A file
  over the limit produces a user-facing message suggesting a shorter cut, not a `413`.
- Status handling: `401`/`403` → an operator-facing log line about the token plus a
  generic user message; `413` → the size message above; `415` → "that file type isn't
  accepted"; `5xx` → one retry after 2 s, then give up.
- Titles are supplied by the model in three parts and **composed by wavo**, the way the
  links block is (§5.4): `Artist — Title (modification)`, where the modification says
  what was done to that file ("bez wokalu", "vocals removed", "zwolnione do 80%") in the
  language of the user's most recent message. A part the model left out is filled with a
  placeholder in that language — "Nieznany wykonawca" / "Unknown artist", "oryginał" /
  "original" — and logged at `warn`, so a finished file is never lost to missing
  metadata, and no stored title is missing a part. A part the model repeated (the artist
  already at the head of the title, the modification already in it) is not said twice.
- The composed title is then sanitized: control characters stripped, trimmed to 200
  characters, falling back to the source filename when empty.
- The **filename** a track is stored under is composed by wavo too, and is unique per
  upload: a fresh UUIDv4, an underscore, then the composed title normalized —
  diacritics folded onto their ASCII letter (`Zażółć` → `Zazolc`), every run of
  anything outside `[A-Za-z0-9]` collapsed into one underscore, capped at 80
  characters — then the source extension:
  `1f0c…_Kult_Arahja_bez_wokalu.mp3`. demix names every run of one stem the same
  (`song_vocals.mp3`), so without this two uploads could collide in the store; the
  slug is only there so the files can be told apart by eye. A title that normalizes
  to nothing leaves the UUID alone.

---

## 8. Failure handling

### 8.1 LLM failures

- Transport errors and `5xx`: two retries with exponential backoff (1 s, 4 s).
- `429`: honour `Retry-After` when present, otherwise back off 5 s, up to three attempts.
- Malformed tool arguments (invalid JSON, unknown tool name, schema violation): the
  error is returned as a tool result so the model can retry. After two consecutive
  invalid tool calls in one turn, wavo switches that turn to `OPENAI_MODEL_FALLBACK` and
  logs the escalation. After two more, the turn is abandoned.
- Missing `OPENAI_API_KEY` is a startup failure.

### 8.2 demix failures

demix errors are the ones a user will actually hit, and they are almost all YouTube
related. `process_audio` returning `ok:false` is classified before being surfaced:

| Signal in stderr/stdout | User-facing message |
| --- | --- |
| `HTTP Error 403`, all download strategies failed | "YouTube blocked the download. Try again later, or send a direct link." |
| No search results | "I couldn't find that song on YouTube — try adding the artist." |
| `ffmpeg` / `ffprobe` not found | Operator-facing: a startup check should have caught this (§10.3) |
| Spleeter model download failure | "The separation model couldn't be downloaded; retrying later usually works." |
| Anything else | "Processing failed." plus the most informative line of the output, truncated to 200 chars |

Classification and that last line both read demix's output with its **constant noise
removed** first: essentia logs `[   INFO   ] MusicExtractorSVM: no classifier models were
configured by default` as `demix` imports it, so it opens the stderr of every run,
successful or not, and bracketed log levels and Python warnings are dropped for the same
reason. Of what is left, a Python traceback is read from its last line — the exception —
and anything else from its first; when stderr says nothing of its own, a `Error: …` line
demix printed on stdout is used; when nothing survives, the bare "Processing failed."
stands alone.

The full stderr always goes to the log at `warn`, regardless of what the user sees.

A 403 that survives every strategy is usually the deployment's IP rather than the
request: YouTube distrusts datacenter ranges, which is what `WAVO_ENABLE_PROXY` (§9) and
operator-supplied cookies (`DEMIX_YT_DLP_ARGS`) are for. Neither is on by default, and
both are operator decisions — wavo never picks a proxy or a cookie jar by itself.

### 8.3 Cancellation and shutdown

- `/cancel` sets a cancellation flag on the turn. The LLM loop checks it between
  iterations; a running `process_audio` is not interrupted mid-way (demix has no
  cancellation protocol) but its result is discarded and no upload happens. The user is
  told the difference: "stopping after the current step".
- `SIGTERM` / `SIGINT`: stop polling, let in-flight turns finish for up to
  `WAVO_SHUTDOWN_GRACE_SEC` (default `30`), close the MCP child process, exit `0`.
- If the `demix-mcp` child dies, wavo restarts it on the next tool call, up to three
  times per hour; beyond that it reports the tool as unavailable and stays up so
  `/status` and `/tracks` keep working.

---

## 9. Configuration

All configuration is via environment variables, validated at startup. Startup fails with
a single clear message naming the missing variable.

| Variable | Default | Required | Description |
| --- | --- | --- | --- |
| `TELEGRAM_BOT_TOKEN` | — | ✔ | Bot token from BotFather |
| `WAVO_ALLOWED_CHAT_IDS` | — | ✔ | Comma-separated numeric chat IDs allowed to use the bot |
| `OPENAI_API_KEY` | — | ✔ | API key (or the key of an OpenAI-compatible provider) |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | | Swap for OpenRouter, vLLM, Ollama, … |
| `OPENAI_MODEL` | `gpt-5-mini` | | Main orchestration model |
| `OPENAI_MODEL_FALLBACK` | `gpt-5` | | Used only on repeated tool-call failures |
| `PLAINSONG_URL` | `http://plainsong:8080` | | Internal API base |
| `PLAINSONG_PUBLIC_URL` | = `PLAINSONG_URL` | | Base used in links sent to users |
| `PLAINSONG_TOKEN` | — | ✔ | Bearer token for plainsong's mutating endpoints |
| `PLAINSONG_MAX_UPLOAD_MB` | `100` | | Client-side guard, keep in sync with plainsong |
| `WAVO_WORK_DIR` | `/work` | | Job outputs and the spleeter model cache |
| `WAVO_MAX_CONCURRENT_JOBS` | `1` | | Permits on the demix semaphore |
| `WAVO_MAX_TOOL_ITERATIONS` | `8` | | Tool-call iterations per turn |
| `WAVO_TURN_TIMEOUT_SEC` | `1800` | | Wall clock per turn |
| `WAVO_JOB_TIMEOUT_SEC` | `1200` | | Wall clock per `process_audio` |
| `WAVO_LLM_TIMEOUT_SEC` | `90` | | LLM request timeout |
| `WAVO_HISTORY_TURNS` | `12` | | Retained user/assistant pairs per chat |
| `WAVO_SESSION_TTL_MIN` | `120` | | Idle session eviction |
| `WAVO_COALESCE_WINDOW_SEC` | `15` | | How long half a request waits for its other half (§5.6); `0` disables |
| `WAVO_TOOL_OUTPUT_CHARS` | `2000` | | Truncation of tool stdout/stderr |
| `WAVO_PROGRESS_INTERVAL_SEC` | `5` | | Minimum gap between progress edits |
| `WAVO_KEEP_JOB_FILES` | `false` | | Keep job directories after publishing |
| `WAVO_EXPOSE_CLEAN` | `false` | | Expose the `clean` MCP tool to the model |
| `WAVO_ALLOW_DELETE` | `false` | | Expose `delete_track` to the model |
| `WAVO_HEALTH_ADDR` | `0.0.0.0:8081` | | Health endpoint bind address |
| `WAVO_SHUTDOWN_GRACE_SEC` | `30` | | Graceful shutdown window |
| `RUST_LOG` | `wavo=info` | | Log filter |
| `DEMIX_YT_DLP_ARGS` | — | | Passed through to demix (e.g. `--cookies-from-browser`) |
| `WAVO_ENABLE_PROXY` | `false` | | Send downloads through an outbound HTTP proxy |
| `WAVO_PROXY_HOST` | — | ✔ when enabled | Proxy hostname (e.g. iproyal.com's `geo.iproyal.com`) |
| `WAVO_PROXY_PORT` | — | ✔ when enabled | Proxy port |
| `WAVO_PROXY_USERNAME` | — | | Proxy username; required together with the password or not at all |
| `WAVO_PROXY_PASSWORD` | — | | Proxy password; redacted like every other secret |

The proxy exists because YouTube distrusts datacenter IP ranges (§8.2), so it covers
**downloads only**: it is set on the `demix-mcp` child's environment
(`HTTP_PROXY`/`HTTPS_PROXY`, both spellings — yt-dlp's urllib and the pytubefix
fallback's requests each read one), and wavo's own calls to Telegram, the LLM provider
and plainsong are unaffected. The credentials travel in the child's environment rather
than in `DEMIX_YT_DLP_ARGS` so they cannot appear on a command line demix echoes into
its stderr, which wavo reads back. With the flag off the other four variables are
ignored entirely and the child inherits whatever proxy environment the host set; with it
on, a missing host or port is a startup failure, and an unreachable proxy is a startup
warning plus one log line naming it with the password removed.

---

## 10. Packaging and deployment

### 10.1 The wavo image

Multi-stage:

1. **Rust builder** — `rust:1-bookworm`, `cargo build --release`, producing `/wavo`.
2. **Runtime** — `ubuntu:22.04`, because it is the only mainstream base where both
   Python versions demix needs are installable side by side:
   - `python3.10` (default in 22.04) → virtualenv `/opt/venv-mcp` with `demix-mcp`
   - `python3.8` (deadsnakes PPA) → virtualenv `/opt/venv-demix` with `demix`
   - `ffmpeg`, `yt-dlp`, `ca-certificates`
   - `PATH=/opt/venv-demix/bin:/opt/venv-mcp/bin:$PATH`, so `demix-mcp` finds `demix`
   - The `wavo` binary at `/usr/local/bin/wavo`
   - Runs as unprivileged user `wavo` (uid `10001`), owning `/work`

The image is large (~2–3 GB; TensorFlow via spleeter dominates). That is the accepted
price of the single-container decision in §3.2. Layer ordering puts the two Python
environments before the Rust binary so a code change rebuilds only the last layer.

### 10.2 Working directory layout

```
/work/
  pretrained_models/       # spleeter model cache (~300 MB, first run downloads it)
  jobs/
    <uuid>/                # one demix output_dir per process_audio call
      music/{wav,mp3}/…
      video/…
```

`demix-mcp` is spawned with `cwd=/work`, so the model cache is shared across jobs and
survives container restarts on the volume. `output_dir` is always
`/work/jobs/<uuid>`, generated by wavo per call. Job directories are removed after the
turn unless `WAVO_KEEP_JOB_FILES=true`; orphans older than 24 h are swept at startup.

### 10.3 Startup checks

Before the first poll, wavo verifies and fails loudly on: required env vars present;
`demix`, `demix-mcp`, `ffmpeg` and `yt-dlp` on `PATH`; `/work` writable; MCP
`initialize` + `tools/list` succeed and contain `process_audio`; plainsong reachable
(`GET /api/tracks`) and the token accepted (a `HEAD`-equivalent probe); Telegram
`getMe` succeeds. Each failure names the variable or binary at fault.

Three more are reported but not fatal, because each one only breaks downloads: the
installed yt-dlp version, whether a cookies file named in `DEMIX_YT_DLP_ARGS` can be
read, and — when `WAVO_ENABLE_PROXY=true` — whether the proxy accepts a TCP connection
within five seconds.

### 10.4 Compose

`docker-compose.yml` in this repo starts both services:

```yaml
services:
  plainsong:
    image: ghcr.io/pwittchen/plainsong:latest   # or build: ../plainsong
    environment:
      PLAINSONG_TOKEN: ${PLAINSONG_TOKEN:?set PLAINSONG_TOKEN in .env}
    volumes: [plainsong-data:/data]
    ports: ["127.0.0.1:8080:8080"]
    restart: unless-stopped

  wavo:
    build: .
    depends_on:
      plainsong: {condition: service_healthy}
    environment:
      TELEGRAM_BOT_TOKEN: ${TELEGRAM_BOT_TOKEN:?}
      WAVO_ALLOWED_CHAT_IDS: ${WAVO_ALLOWED_CHAT_IDS:?}
      OPENAI_API_KEY: ${OPENAI_API_KEY:?}
      OPENAI_MODEL: ${OPENAI_MODEL:-gpt-5-mini}
      PLAINSONG_URL: http://plainsong:8080
      PLAINSONG_PUBLIC_URL: ${PLAINSONG_PUBLIC_URL:-http://127.0.0.1:8080}
      PLAINSONG_TOKEN: ${PLAINSONG_TOKEN:?}
    volumes: [wavo-work:/work]
    restart: unless-stopped

volumes:
  plainsong-data:
  wavo-work:
```

- Secrets live in a git-ignored `.env` next to the compose file.
- No port is published for wavo — long polling needs no ingress. The health endpoint is
  reachable inside the network and used by the container health check.
- `docker-compose.override.yml` (git-ignored, documented in the README) is how a
  developer swaps `image:` for `build: ../plainsong` against a local checkout.

---

## 11. Project layout

```
wavo/
  Cargo.toml
  Dockerfile
  docker-compose.yml
  .env.example
  README.md
  SPEC.md
  src/
    main.rs            # startup checks, wiring, signal handling
    config.rs          # env parsing and validation
    telegram/
      mod.rs           # long-polling loop, dispatch
      api.rs           # getUpdates / sendMessage / editMessageText
      collect.rs       # half-request buffers and the coalescing window
      commands.rs      # /start /help /status /tracks /reset /cancel
      format.rs        # HTML escaping, truncation, links block
    llm/
      mod.rs           # the tool-calling loop and budgets
      openai.rs        # chat.completions request/response types
      prompt.rs        # system prompt
    mcp/
      mod.rs           # rmcp client lifecycle, restart policy
      schema.rs        # MCP → OpenAI tool schema conversion, arg injection
    tools/
      mod.rs           # dispatch table (MCP tools + native tools)
      publish.rs       # publish_track, path guard
      plainsong.rs     # REST client
    session.rs         # per-chat history and trimming
    jobs.rs            # job dirs, semaphore, progress reporting, cleanup
    health.rs          # /healthz
    error.rs
  tests/
    schema_conversion.rs
    path_guard.rs
    llm_loop.rs        # against a stub OpenAI server
    e2e_smoke.rs       # ignored by default; needs compose
```

---

## 12. Security notes

- **The plainsong token, the OpenAI key and the proxy password are never logged**, never
  included in an error message sent to Telegram, and never placed in the LLM context.
  Config `Debug` impls redact them; the proxy is logged as
  `http://user:<redacted>@host:port`.
- **The chat allow-list is the only authentication.** Anyone who can message the bot can
  spend the operator's OpenAI credits and CPU, so an unset allow-list refuses to start.
- **The model cannot choose paths.** `cwd` and `output_dir` are stripped from the tool
  schema and injected by wavo; `publish_track` accepts only keys from the current turn's
  path table, which contains nothing outside `/work/jobs/<uuid>`. Path traversal
  (`..`, absolute paths, symlinks resolving outside the job dir) is rejected before any
  file is opened.
- **No shell.** demix is invoked through MCP, which uses `argv` — user text never
  reaches a shell. `DEMIX_YT_DLP_ARGS` is operator-supplied, not user-supplied.
- **Prompt injection is contained by design**: a hostile YouTube title cannot do more
  than cause another demix run or a badly titled upload, because the tool surface is
  four audio operations plus an upload to the operator's own store. `delete_track` is
  the one destructive tool, and it is off by default.
- **Egress**: the container talks to Telegram, the LLM provider, YouTube and plainsong.
  Nothing listens on a published port. With `WAVO_ENABLE_PROXY=true` the YouTube half of
  that goes through the operator's proxy, which then sees the traffic wavo's downloader
  makes — one more party to trust, and the reason the proxy is off by default.
- TLS termination for plainsong is a reverse proxy's job, as in plainsong's own spec.

---

## 13. Observability

- Structured `tracing` logs. One span per turn carrying `chat_id`, `turn_id` and
  `model`; one child span per tool call carrying the tool name and duration.
- Per turn, an `info` line on completion: iterations used, tools called, tokens in/out
  as reported by the provider, wall clock, tracks published.
- `GET /healthz` returns `200 {"ok":true,"mcp":"up","plainsong":"up"}` when the MCP
  child is alive and plainsong answered within the last minute; `503` otherwise. Used as
  the container health check.
- No metrics endpoint, no tracing exporter — logs are enough at this size.

---

## 14. Testing

| Level | What |
| --- | --- |
| Unit | MCP→OpenAI schema conversion (including the `cwd`/`output_dir` strip); path guard against traversal, symlinks and absolute paths; tool-output truncation; history trimming; Telegram HTML escaping; error classification (§8.2) against captured demix stderr samples; the `Lang` lookup — every wavo-composed string exists in both variants and falls back to English when the language is unknown |
| Integration | The LLM loop against a stub HTTP server replaying scripted tool calls — covers budgets, escalation, malformed arguments and cancellation; the plainsong client against a stub returning `401`/`413`/`415`/`5xx`; the MCP layer against a stub MCP server binary, including a child that exits mid-call |
| End-to-end | `#[ignore]`d test driving a real compose stack with a short local audio file: process → publish → assert the track is listed by `GET /api/tracks` and reachable at its stream URL |
| CI | `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, `docker build` (the e2e test is not run in CI — it needs the models and network) |

No test may call the real OpenAI API, the real Telegram API, or YouTube.

---

## 15. Acceptance criteria

1. `docker compose up -d --build` with a filled `.env` brings up plainsong and wavo, and
   both report healthy.
2. Sending `/help` from an allow-listed chat returns the capability blurb; sending it
   from any other chat returns nothing, and the rejected chat ID appears in the log.
3. "separate vocals from Queen - Bohemian Rhapsody" results in: a progress message that
   updates at least once, a completed 2-stem run, at least the vocals track published to
   plainsong, and a final reply containing a working per-track link and the listing link.
4. The published track appears in `GET /api/tracks` and plays from its plainsong track
   page.
5. "slow down that song to 80%" as a follow-up in the same chat reuses the previous
   song from history without asking again.
6. "transpose it to A minor" produces a run with `target_key=Am` and no `transpose`.
7. A YouTube download failure produces the mapped user-facing message from §8.2, with
   the full stderr present only in the log.
8. A second request sent while a job is running is refused politely; a request from
   another allow-listed chat queues and reports its position.
9. Killing the `demix-mcp` process causes the next tool call to restart it transparently.
10. `docker compose restart wavo` loses conversation history but keeps the spleeter model
    cache — the next job does not re-download models.
11. No log line anywhere contains `PLAINSONG_TOKEN`, `OPENAI_API_KEY` or
    `TELEGRAM_BOT_TOKEN`.
12. A crafted request asking the bot to publish `/etc/passwd` or a path outside the job
    directory is refused by the path guard, and the refusal is visible in the log.
13. "oddziel wokal od Queen - Bohemian Rhapsody" performs the same run as its English
    equivalent in criterion 3 and every reply in that turn — progress stages, the final
    summary and the links block label — is in Polish; switching to English in the next
    message switches the replies to English without losing the song from history.
14. `/help` sent after a Polish message returns the Polish blurb; sent after an English
    message, the English one.

---

## 16. Milestones

| # | Milestone | Done when |
| --- | --- | --- |
| 1 | Skeleton | Config parsing, startup checks, health endpoint, structured logging |
| 2 | Telegram loop | Long polling, allow-list, commands, echo replies, progress edits |
| 3 | MCP client | `demix-mcp` spawned, `tools/list` discovered, `process_audio` callable from a hard-coded request |
| 4 | plainsong client | `publish_track` / `list_tracks` working against a live plainsong |
| 5 | LLM loop | Tool calling end to end, budgets, truncation, error classification |
| 6 | Packaging | Dockerfile with both Python environments, compose file, `.env.example` |
| 7 | Hardening | Path guard, cancellation, MCP restart policy, escalation, full test suite |

Milestones 1–4 are independently demonstrable without an LLM, which keeps the expensive
part of the system out of the critical path until the plumbing is proven.

---

## 17. Open questions

- **Publishing every stem vs. only the requested ones.** The spec says "only what was
  asked for"; a 4-stem run the user asked for in full means four uploads and four links,
  which makes for a noisy reply. A single reply listing four links is assumed acceptable
  until it isn't.
- **Long jobs vs. Telegram.** A 10-minute spleeter run on a small VPS is normal. Progress
  edits cover it, but if runs regularly exceed the turn timeout, the fix is a detached
  job model with a completion notification — a real design change, deliberately deferred.
- **Whether `/work` should be a bind mount** so the operator can grab intermediate files.
  A named volume is specified; a bind mount needs `chown 10001` as in plainsong.
- **A third language.** The model will happily answer in German or Spanish, but wavo's
  own strings (§5.5) only exist in Polish and English, so such a turn would mix
  languages. The spec accepts that: Polish and English are the supported pair, and the
  `Lang` enum stays two-valued until there is a reason for a third.
- **Cost ceiling.** There is no per-chat spend cap. With a one-or-two-user allow-list
  this is fine; it stops being fine the moment the allow-list grows.

---

## 18. Possible follow-ups (not part of this scope)

- Accepting audio files and voice notes sent directly in Telegram as demix input
- A detached job model with `/jobs` and completion notifications
- Persisting sessions to disk across restarts
- An HTTP transport for `demix-mcp`, splitting demix into its own container
- Publishing generated video output (demix `--video`) somewhere other than plainsong
- A second front-end (CLI or Slack) over the same agent core
