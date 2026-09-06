# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

wavo is a Telegram bot that turns one plain-language music request into demix
tool calls over MCP and publishes the result to plainsong. One Rust binary; no
database, no queue, no web UI beyond `/healthz`.

**[SPEC.md](SPEC.md) is the design authority.** Source comments cite its section
numbers (`§6.4`, `§8.2`) — when changing behaviour, read the cited section
first. Where the implementation knowingly departs from the spec, the departure
is listed in README's *"where this deviates from SPEC.md"* section; keep that
list honest rather than silently drifting.

## Commands

```sh
cargo test                                        # everything; no network, no API keys
cargo test --test tool_dispatch                   # one integration file
cargo test --test tool_dispatch a_youtube_link    # one test by name substring
cargo test --lib -- --nocapture                   # unit tests, with output
cargo clippy --all-targets -- -D warnings         # CI gate
cargo fmt --all
PLAINSONG_TOKEN=… cargo test --test e2e_smoke -- --ignored --nocapture
```

CI (`.github/workflows/ci.yml`) runs `fmt --check`, `clippy -D warnings`,
`cargo test --all-targets` and a `docker build`. The e2e test is `#[ignore]`d —
it needs spleeter models, ffmpeg and a live plainsong.

Running the binary needs `demix`, `demix-mcp`, `ffmpeg` and `yt-dlp` on `PATH`
(startup checks refuse to start without them) plus a plainsong to talk to; see
README's *development* section for the env vars. `docker compose up -d --build`
is the real deployment and builds plainsong from a sibling checkout.

## Architecture

One turn walks through every module, in this order:

`telegram/mod.rs` (long poll → allow-list → command or turn) → `session.rs`
(history, one in-flight turn per chat) → `llm/mod.rs` (the tool-calling loop,
budgets, escalation) → `tools/mod.rs` (dispatch) → either `mcp/mod.rs` (the
`demix-mcp` child) or `tools/plainsong.rs` (the REST client) → back up, with the
links block appended by `llm::compose_reply`.

Things that are not obvious from a single file:

- **The tool catalogue is half discovered, half native.** demix's tools come
  from `tools/list` at startup and are converted by `mcp/schema.rs`; `cwd` and
  `output_dir` are stripped from the schema (`INJECTED_ARGS`) and injected by
  wavo per call. `publish_track` / `list_tracks` / `delete_track` are built in
  `Tools::new`. Adding a demix tool needs no wavo change.
- **The model never names a filesystem path.** `TurnCtx` keeps a relative-key →
  absolute-path table filled from each demix result; `publish_track` and a
  `file` argument resolve through it and through `tools/publish.rs`, which
  canonicalizes and re-checks containment in the job directory.
- **Tool failures are results, not errors.** `ToolBox::call` never returns
  `Err`: a failure is `{"ok": false, "error": …}` so the model can correct
  itself. demix stderr is classified by `classify_demix_error` into a
  human sentence; the raw output only goes to the log.
- **wavo composes its own links.** Track and listing URLs are appended after the
  model's text, so a hallucinated URL cannot reach a user.
- **Bilingual strings are a lookup, not an i18n framework.** Every string wavo
  composes itself lives in `telegram/format.rs` keyed by `(Msg, Lang)`, and a
  test asserts every `Msg` in `Msg::ALL` has a Polish *and* an English variant
  that differ. Adding a `Msg` variant means adding both. The model handles the
  language of its own replies, per the system prompt in `llm/prompt.rs`.
- **`llm/prompt.rs` is behaviour.** Defaults for vague requests, source
  selection (`url` / `search` / `file`) and the language rules live in that one
  `const`, mirrored by the bullet list in SPEC §6.2.
- **Concurrency has two layers**: one in-flight turn per chat (a second message
  is refused, not queued) and a global semaphore in `jobs.rs` around
  `process_audio`, because spleeter is memory-hungry.
- **Nothing is persisted.** Sessions are in memory and die with the process;
  only `/work` (job dirs and the spleeter model cache) survives.

## Testing constraints

No test may call the real OpenAI API, the real Telegram API or YouTube. The MCP
layer is tested against `tests/fixtures/stub_mcp_server.py` (which can also die
mid-call, for the restart policy) and HTTP clients against a stub server in
`tests/support`. MCP-backed tests no-op when `python3` is missing, so a green
run on a machine without it proves less than it looks — keep `python3`
available.

## Git

- Commit messages must not contain AI attribution: no `Co-Authored-By: Claude`
  (or any other AI co-author trailer), no "Generated with Claude Code" line, no
  🤖 footer. The same goes for pull request descriptions.
