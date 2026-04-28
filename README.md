# sjmb_slack

`sjmb_slack` is a Slack bot (Socket Mode) written in Rust that listens to message events, extracts URLs with a
configurable regex, and writes them into PostgreSQL.

## What the program does

- Connects to one or more Slack workspaces through Socket Mode.
- Validates each workspace at startup and builds a channel id to `workspace-channel` map.
- Receives Slack push events and filters down to supported message events.
- Extracts URLs from message text using a configured regular expression.
- Resolves the sender name from the Slack event, cached user info, or a `users.info` lookup.
- Inserts detected URLs into a PostgreSQL `url` table together with timestamp, channel, and sender name.
- Updates a `url_changed` marker table after inserts.

## Project layout

- `src/bin/sjmb_slack.rs`: executable entrypoint.
- `src/config.rs`: CLI flags, config path expansion, tracing setup, rustls provider setup.
- `src/slackbot.rs`: bot config model, Slack API/socket setup, sender name lookup/cache, event handlers, URL extraction flow.
- `src/db_util.rs`: PostgreSQL connection helpers and insert retry logic.
- `build.rs`: injects build metadata (`GIT_BRANCH`, `GIT_COMMIT`, `SOURCE_TIMESTAMP`, `RUSTC_VERSION`) and fails the build if metadata emission fails.
- `config/sjmb_slack.json`: example runtime config.
- `config/sjmb_slack.manifest.yaml`: Slack app manifest for scopes, Socket Mode, and event subscriptions.
- `install.sh`: copies release binary to `$HOME/sjmb_slack/bin`.

## Development commands

- `cargo build`
- `cargo build --release`
- `cargo run -- --verbose --bot-config config/sjmb_slack.json`
- `cargo run --release -- --verbose --bot-config config/sjmb_slack.json`
- `cargo check`
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features`
- `cargo test`
- `cargo build --release && ./install.sh`

## Runtime configuration

The bot reads a JSON file (default: `$HOME/sjmb_slack/config/sjmb_slack.json`):

```json
{
  "url_regex": "<(https?://[^>]+)>",
  "url_log_db": "postgres:///url",
  "workspaces": [
    {
      "name": "foo",
      "api_token": "xoxb-...",
      "socket_token": "xapp-..."
    }
  ]
}
```

Fields:

- `url_regex`: regex used to find URLs in message text. The bot stores capture group `1`.
- `url_log_db`: PostgreSQL connection string used by `sqlx`.
- `workspaces`: Slack workspace list.

Workspace entry fields:

- `name`: prefix used when building channel labels (`workspace-channel`).
- `api_token`: bot token for Web API calls (startup validation and channel discovery).
- `socket_token`: app-level token for Socket Mode listener.

## Slack app setup

This bot relies on Socket Mode plus Slack Event Subscriptions. The app must be configured to receive
channel message events for channels where the bot has been invited.

Required bot event subscriptions:

- `message.channels`
- `message.groups`

Required bot token scopes:

- `channels:history`
- `groups:history`
- `channels:read`
- `groups:read`
- `users:read`

After changing scopes or event subscriptions in Slack, reinstall the app before testing.

The repository includes a manifest you can import when creating or updating the Slack app:

- [config/sjmb_slack.manifest.yaml](/home/sjm/git/Rust/sjmb-slack/config/sjmb_slack.manifest.yaml)

What the manifest does not include:

- The app-level Socket Mode token (`xapp-...`). Create it in Slack after import.
- The runtime config file with your actual `api_token`, `socket_token`, and database URL.

Typical setup flow:

1. Create or update the Slack app from the manifest.
2. Enable Socket Mode if Slack prompts for confirmation.
3. Generate an app-level token for Socket Mode with connections enabled.
4. Install or reinstall the app to the workspace.
5. Copy the bot token (`xoxb-...`) and app token (`xapp-...`) into `config/sjmb_slack.json`.
6. Invite the bot to the channels you want monitored.

## CLI flags

- `-v`, `--verbose`: `INFO` logs.
- `-d`, `--debug`: `DEBUG` logs.
- `-t`, `--trace`: `TRACE` logs.
- `-b`, `--bot-config <PATH>`: config JSON path (supports env expansion, for example `$HOME/...`).

If none of `verbose/debug/trace` are set, log level defaults to `ERROR`.

## Program internals

### Startup sequence

1. `main` parses `OptsCommon` with `clap`.
2. `finalize()` expands `bot_config` path with `shellexpand`.
3. `start_pgm()` initializes tracing and logs build metadata from `build.rs` env vars.
4. `Bot::new()` loads JSON config, compiles `url_regex`, validates each workspace with `api_test`, and fetches channels
   to build an internal channel id -> name map.

### Event and message flow

1. `Bot::run()` creates an unbounded Tokio MPSC channel for message processing.
2. One task runs `handle_messages(rx)` and serially processes incoming message events.
3. For each workspace, a Socket Mode listener is started.
4. Callback behavior:

- `handler_push_events`: forwards only relevant `Message` events into the channel. Edited, deleted,
  hidden, and other unsupported message subtypes are ignored.
- `handler_interaction_events`: currently logs and returns `Ok(())`.
- `handler_error`: logs the error and returns HTTP `200 OK` to acknowledge Slack retries.

### URL extraction and DB writes

`handle_msg()`:

1. Resolves channel id to `workspace-channel` (or `"<NONE>"`).
2. Resolves the sender name from message fields, a workspace-local cache, or `users.info`.
3. Reads message text (if present).
4. Runs `url_regex` captures over text.
5. For each URL capture, opens a DB pool with `start_db(url_log_db)`.
6. Calls `db_add_url()` with timestamp, channel, sender name, and URL.

`db_add_url()` behavior:

- Executes `insert into url (seen, channel, nick, url) values (...)`.
- Retries up to `RETRY_CNT = 5` with `RETRY_SLEEP = 1s` on failure.
- Calls `db_mark_change()` (`update url_changed set last = $1`) if `update_change` is true.

## License

This project is licensed under `MIT OR Apache-2.0`.
