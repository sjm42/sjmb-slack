# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
# Build
cargo build
cargo build --release

# Run (debug)
cargo run -- --verbose --bot-config config/sjmb_slack.json

# Run (release)
cargo run --release -- --verbose --bot-config config/sjmb_slack.json

# Install to ~/sjmb_slack/bin/
cargo build --release && ./install.sh

# Check/lint
cargo check
cargo clippy
```

CLI flags: `-v/--verbose` (INFO), `-d/--debug` (DEBUG), `-t/--trace` (TRACE). Default log level is ERROR.

## Architecture

Single binary (`src/bin/sjmb_slack.rs`) backed by a library crate. The three source modules are:

- **`config.rs`** — `OptsCommon` (clap-derived CLI args). Handles shell expansion of paths and initializes `tracing_subscriber` and the `rustls` crypto provider.
- **`slackbot.rs`** — Core bot logic. `Bot` is deserialized from the JSON config and holds workspaces, a compiled URL regex, and a channel-ID→name map. `Bot::run()` spawns one Tokio task per workspace (Socket Mode listener) plus one shared message-handler task. Messages are passed from the per-workspace push-event handler over a `tokio::mpsc::unbounded_channel` to `handle_messages`, where URLs are extracted and logged to Postgres.
- **`db_util.rs`** — Postgres helpers via `sqlx`. Inserts detected URLs into the `url` table and updates a `url_changed` table on each insert. Retries up to 5 times on failure with 1-second sleep.

## Configuration

Config file location defaults to `$HOME/sjmb_slack/config/sjmb_slack.json`. See `config/sjmb_slack.json` for the structure:

```json
{
  "url_regex": "<(https?://[^>]+)>",
  "url_log_db": "postgres:///url",
  "workspaces": [
    { "name": "ws-name", "api_token": "xoxb-...", "socket_token": "xapp-..." }
  ]
}
```

Each workspace requires a Bot OAuth token (`api_token`) and an App-Level token with `connections:write` scope (`socket_token`) for Socket Mode.

## Database

The bot expects a PostgreSQL database with at least:
- `url` table: columns `id`, `seen` (i64 timestamp), `channel`, `nick`, `url`
- `url_changed` table: column `last` (i64 timestamp)

`nick` is always stored as `"N/A"` (Slack messages don't expose IRC-style nicks; this field is a legacy from IRC bot heritage).
