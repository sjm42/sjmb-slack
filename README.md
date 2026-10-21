# sjmb_slack

`sjmb_slack` is a Slack URL harvester written in Rust with Socket Mode and optional user-session backends. It listens to message events, extracts URLs with a
configurable regex, and writes them into PostgreSQL.

## What the program does

- Connects to one or more Slack workspaces through Socket Mode or the optional user-session backend.
- Validates each workspace at startup and builds a channel id to `workspace-channel` map.
- Receives Slack push events and filters down to supported message events.
- Queues supported messages through a bounded Tokio channel before serial database writes.
- Logs supported message text to the console, including `file_share` messages such as images posted with text.
- Extracts URLs from message text using a configured regular expression.
- Resolves the sender name from the Slack event, cached user info, or a `users.info` lookup.
- Excludes blacklisted domains/IPs and non-public or unresolvable destinations before database insertion.
- Inserts detected URLs into a PostgreSQL `url` table together with timestamp, channel, and sender name.
- Relies on the database's `url` table trigger to publish change notifications after inserts.

## Project layout

- `src/bin/sjmb_slack.rs`: executable entrypoint.
- `src/lib.rs`: module exports.
- `src/config.rs`: CLI flags, config path expansion, tracing setup, rustls provider setup, and runtime initialization.
- `src/slackbot.rs`: configuration, shared sender/channel lookups, message queue, message logging, and harvesting pipeline.
- `src/slackbot/socket_mode.rs`: existing app/bot transport and supervision.
- `src/slackbot/user_session.rs`: read-only user websocket transport and message admission.
- `src/slackbot/user_session/`: bounded session HTTP client, persistent recovery state, and history recovery.
- `src/db_util.rs`: lazy PostgreSQL pool and bounded URL inserts.
- `src/harvest.rs`: Slack link decoding, URL validation/limits, and message timestamps.
- `src/url_filter.rs`: global/workspace host rules, DNS resolution, and public-address policy.
- `src/limits.rs`: bounded expiring caches, discard counters, and console text bounds.
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
- `cargo update`
- `cargo outdated --root-deps-only`
- `cargo build --release && ./install.sh`

Direct dependency updates should be checked with `cargo outdated --root-deps-only`. Compatible lockfile-only updates can
usually be applied with `cargo update`; direct version bumps belong in `Cargo.toml` and should be followed by
`cargo check`, `cargo test`, and `cargo clippy --all-targets --all-features`.

## Runtime configuration

The bot reads a JSON file (default: `$HOME/sjmb_slack/config/sjmb_slack.json`):

```json
{
  "url_regex": "<(https?://[^>]+)>",
  "url_log_db": "postgres:///url?sslmode=disable",
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

- `url_regex`: regex used to find URLs in message text. Capture group `1` is required; optional captures that did
  not match are skipped. Slack link labels are removed and entities decoded before HTTP/HTTPS URL validation.
- `url_log_db`: PostgreSQL connection string used by `sqlx`.
- `url_blacklist`: optional global list of DNS names or IP addresses to exclude from the database; defaults to `[]`.
- `workspaces`: Slack workspace list.

Workspace entry fields:

- `name`: prefix used when building channel labels (`workspace-channel`).
- `api_token`: bot token for Web API calls (startup validation and channel discovery).
- `socket_token`: app-level token for Socket Mode listener, from the same app as the bot token.
- `url_blacklist`: optional additional exclusions for this workspace; defaults to `[]`. Global exclusions always apply.

For example, put `"url_blacklist": ["example.com", "internal.example.org"]` at the top level to exclude those
domains everywhere, and `"url_blacklist": ["noisy.example.net"]` inside a workspace entry to add a local exclusion.
A DNS rule includes the named host and **all its subdomains**. There are no wildcards, regexes, or allowlist overrides.
Use hostnames only, without schemes, ports, paths, or queries. IP rules match exact addresses, including DNS answers;
IPv6 rules accept bare or bracketed addresses. Invalid entries fail startup with their scope and entry number.
DNS names use lowercase ASCII/IDNA spelling and exactly one trailing dot internally; either trailing-dot spelling is
accepted in configuration. Filtering does not add a trailing dot to stored URLs. Restart to apply configuration changes.

Every candidate URL also passes a public-address check, even when both blacklists are empty. IP literals are checked
directly. Other hosts are resolved using the system resolver with an absolute DNS name and a **three-second** deadline.
There is no application DNS cache. A URL is excluded if any returned address is non-public, if resolution fails or times
out, or if it returns zero or more than 64 addresses. The conservative address policy matches `sjmb`'s HTTP fetcher:
it excludes private, loopback, link-local, shared/CGNAT, documentation, multicast, reserved, and IPv6 translation/tunnelling
ranges. DNS results reflect the bot machine's resolver configuration, including VPN/split DNS.

Exclusion skips only database insertion. Messages and detected URLs still appear in logs, with the exclusion reason at
INFO (`--verbose`). Excluded URLs count as successfully processed for recovery; temporary DNS failures can therefore
lose otherwise acceptable URLs. Live events, recovered history/thread replies, and dry runs use the same policy.
Existing rows are not removed, and removing a rule does not replay previously excluded messages. No HTTP request or
redirect following is performed. Downstream fetchers must still check their own DNS results and redirect destinations.

Configuration is limited to 1 MiB and validated before contacting Slack. At least one workspace is required;
workspace names and socket tokens must be unique, and tokens must be nonempty. Each listener is for one workspace/app
installation. The bot validates the bot token with `auth.test`, resolves the app with `bots.info`, verifies the Socket
Mode hello's app ID, and checks incoming event workspace/app IDs. Duplicate installations are rejected.

SQLx TLS support remains compiled in. For local connections, SSH tunnels, or WireGuard deployments where PostgreSQL
TLS is unnecessary, set `sslmode=disable` in the connection URL as shown above. Append `&sslmode=disable` when other
query parameters already exist. This explicitly skips negotiation; omitting it leaves SQLx's default `prefer` policy.
The program does not override your configured TLS mode.

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

- [config/sjmb_slack.manifest.yaml](config/sjmb_slack.manifest.yaml)

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

## User-session backend

This optional backend connects using your own Slack session. It harvests joined public/private channels, including
your own posts and ordinary thread replies. DMs, group DMs, edits, deletions, hidden messages, and unsupported subtypes
are excluded. It never calls read-marker, presence, typing, or message-writing methods. Reading history does not mark
messages read. Verify unread markers and any incidental presence effects with your normal Slack clients during the live test.

Build two versions into separate directories:

```sh
cargo build --release --target-dir target/socket
cargo build --release --no-default-features --features user-session --target-dir target/user
```

Socket Mode remains the default feature. A binary built with only `user-session` defaults to that backend. When both
features are compiled (`--all-features`), set `"backend": "user-session"` to select it; omitted `backend` retains Socket
Mode for existing configurations. Selecting a backend not compiled into the binary fails during configuration validation.

Start with [config/sjmb_slack.user-session.json](config/sjmb_slack.user-session.json). Fill in the token beginning with
`xoxc-` and the matching Cookie header from the same browser/native-client session, for example
`d=<exact value>; d-s=<value if required>`. Preserve percent encoding. No `socket_token` is used. Session acquisition is
manual; the bot neither reads your browser profile nor automates sign-in. See
[wee-slack's extraction instructions](https://github.com/wee-slack/wee-slack#get-a-session-token).

The sample enables `"dry_run": true`. This logs messages and extracted URLs without database inserts and keeps
checkpoints in a separate `probe/` state directory. For a live acceptance test:

```sh
target/user/release/sjmb_slack --verbose --bot-config /path/to/your/user-session.json
```

Check messages in unopened public/private channels, your own messages, thread replies, unchanged unread markers,
and recovery after a brief disconnection/restart. Set `dry_run` to `false` to enable harvesting. The database pool is
lazy, so a probe does not connect to PostgreSQL. The current implementation has automated local HTTP/websocket tests
but has not been validated against a real user session on your VM.

Socket Mode and user-session processes do not share duplicate detection state. Use the probe for side-by-side
validation with the existing bot; running both as writers can insert the same messages' URLs twice.

### Get the session token and cookies from Firefox

Do this on your desktop or laptop. Firefox must be signed into Slack itself; a login in the native Slack app is a
separate session. A dedicated Firefox profile is convenient for keeping the bot's source session separate from
everyday browsing. If you use Firefox containers, obtain both credentials in the same container.

1. Visit `https://YOUR-WORKSPACE.slack.com/customize` in Firefox and sign in. Check that the page belongs to the
   workspace you want to harvest. You can also start at <https://my.slack.com/customize> and select the workspace.
2. Press **Ctrl+Shift+K** to open the page's Web Console, then run:

   ```javascript
   window.prompt("Session token:", TS.boot_data.api_token)
   ```

   Copy the complete `xoxc-...` value from the prompt. This becomes `api_token` in the workspace configuration.
3. Open Developer Tools' **Storage** tab, using **Shift+F9** or selecting Storage from the tools menu. Expand
   **Cookies**, then select the Slack origin. Look for a row whose **Name** is `d` and **Domain** is `.slack.com`.
4. Copy that row's complete **Value**. If there is also a `d-s` cookie for Slack, copy its value too. Use the actual
   values, not a shortened table preview; selecting the cookie displays its details. Preserve percent escapes such
   as `%2F` exactly. The configuration needs the cookie names as well as their values:

   ```json
   {
     "name": "your-workspace-label",
     "api_token": "xoxc-YOUR-COMPLETE-TOKEN",
     "cookie": "d=YOUR-COMPLETE-D-VALUE; d-s=YOUR-COMPLETE-D-S-VALUE"
   }
   ```

   If there is no `d-s` cookie, use only `"cookie": "d=YOUR-COMPLETE-D-VALUE"`. This object belongs inside the
   `workspaces` array of the sample user-session config. Omit `socket_token` and keep `dry_run` enabled initially.
5. Save your populated configuration outside the repository and copy it to the cloud VM over SSH/SCP. The VM only
   needs the credential values; Firefox can be closed and the desktop/laptop can be powered off.

If the console command reports `TS is not defined`, use the Slack web client in that same Firefox profile/container.
Open **Network** with **Ctrl+Shift+E**, reload the page, filter for `/api/`, and select a Slack API request. Look in its
**Request** parameters for `token`, or in **Headers** for `Authorization: Bearer xoxc-...`. Copy just the `xoxc-...`
token, excluding `Bearer `. Then obtain the matching cookies through Storage as above. The outgoing request's raw
Cookie header is also useful for checking the exact encoded cookie values.

Keep the token and cookies from the same Firefox session. Each workspace needs its own token. Signing out,
session expiration, or changes to the browser login can invalidate the exported credentials; obtain fresh values
and restart the bot when that happens.

References: [wee-slack session extraction](https://github.com/wee-slack/wee-slack#get-a-session-token),
[Firefox Storage Inspector](https://firefox-source-docs.mozilla.org/devtools-user/storage_inspector/index.html), and
[Firefox request details](https://firefox-source-docs.mozilla.org/devtools-user/network_monitor/request_details/index.html).

### Several workspaces in one Firefox profile

Each workspace has its own `xoxc-...` token. The `.slack.com` session cookie is normally shared by workspaces signed
into the same Firefox profile/container. Adding another login can change that cookie, making an earlier exported
token/cookie pair stale. Separate profiles are not normally required for each workspace.

1. Sign into all desired workspaces first, without signing out between them.
2. Open each workspace and extract its own current `xoxc-...` token.
3. After completing the logins, copy the current `d` cookie and `d-s` if present. Use that current Cookie header in
   each workspace entry from this browser session. If an older token still fails, re-extract it from its workspace.
4. Restart the bot with `dry_run` enabled and check the authenticated team/user IDs logged for each entry.

For example, inside your user-session config:

```json
{
  "workspaces": [
    {
      "name": "workspace-a",
      "api_token": "xoxc-TOKEN-FOR-A",
      "cookie": "d=CURRENT-SHARED-D-VALUE; d-s=CURRENT-SHARED-D-S-VALUE"
    },
    {
      "name": "workspace-b",
      "api_token": "xoxc-TOKEN-FOR-B",
      "cookie": "d=CURRENT-SHARED-D-VALUE; d-s=CURRENT-SHARED-D-S-VALUE"
    }
  ]
}
```

Omit `d-s` if absent. The bot calls `auth.test` to learn the actual workspace/account for each token; `name` only
controls the stored channel label. Reusing the identical token/cookie pair does not select a different workspace,
and configuring the same authenticated workspace/account twice fails on its recovery-state lock.

For independent browser login lifetimes, use a Firefox profile or container per workspace instead. In that case,
export each workspace's token and cookies from its own profile/container; do not share cookies between them.
The shared-cookie arrangement is also demonstrated by
[wee-slack's extractor](https://github.com/wee-slack/wee-slack/blob/master/extract_token_from_browser.py), which pairs
each discovered workspace token with the same browser cookie.

### Disconnect recovery

- `recovery.state_dir` defaults to `$HOME/.local/state/sjmb-slack` and supports environment/tilde expansion.
  One locked state file per workspace/account prevents concurrent instances from sharing progress. Keep this directory
  across restarts. State stores Slack IDs, bot checkpoints, recent message IDs and tracked threads, not credentials.
- `recovery.window_seconds` defaults to **86400** (24 hours), configurable from 1 second to 7 days. Existing checkpoints
  older than the window are clipped with a warning. With no checkpoint, the bot starts from its connection time.
- After reconnecting, the live socket continues receiving while channel history and recently tracked threads are
  recovered through the fixed connection boundary. A two-minute overlap and full Slack timestamps suppress duplicates.
  The bot uses its own progress, never your account's `last_read` position.
- Checkpoints advance after worker acknowledgement and successful inserts, never merely after queue admission.
  Incomplete recovery stays pending and retries every minute. Ordinary database/queue failures reconnect and trigger
  recovery; invalid session credentials stop with an actionable error. Replace expired tokens and matching cookies manually.
- `recovery.max_pages_per_channel` defaults to **100** pages per history/thread request sequence (200 messages requested
  per page), configurable from 1 to 1000. Hitting the cap leaves progress pending and logs an error; increase the cap
  if needed. Recovery observes `Retry-After`; no fixed OAuth rate allowance is assumed for browser sessions.
- Progress is snapshotted on ordered heartbeat barriers, completed recovery, disconnect and shutdown using atomic file
  replacement. Memory/disk state is capped at 4096 channels, 4096 recent threads and 32768 recent message IDs; oldest
  thread/duplicate entries can be evicted. HTTP responses are limited to 4 MiB and websocket messages/frames to 1 MiB,
  before the common 64 KiB message-text limit. Requests/connection startup have deadlines, and heartbeat loss reconnects.

Recovery is best effort: replies to old untracked threads, deleted/inaccessible messages, outages exceeding the recovery
window, and limit exhaustion can leave gaps. A crash between a successful database insert and a persisted checkpoint
can duplicate an insert. There is no PostgreSQL schema change or transaction spanning the database and local state.
The websocket protocol is private and can change. Authenticated HTTP requests never follow redirects, websocket
destinations must use TLS on a `slack.com` subdomain, and only six explicitly allowed read-only API methods are available.

Validate both individual feature builds and their combination when developing:

```sh
cargo test
cargo test --no-default-features --features user-session
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

## CLI flags

- `-v`, `--verbose`: `INFO` logs.
- `-d`, `--debug`: `DEBUG` logs.
- `-t`, `--trace`: `TRACE` logs.
- `-b`, `--bot-config <PATH>`: config JSON path (supports env expansion, for example `$HOME/...`).

If none of `verbose/debug/trace` are set, log level defaults to `ERROR`. Dependencies follow the selected level up to
INFO; DEBUG and TRACE apply to this application's code. Message console output is normalized to one line and bounded
to 4 KiB. Debug output may include credentials and complete SDK payloads; credential redaction is not provided.

## Program internals

### Startup sequence

1. `main` parses `OptsCommon` with `clap`.
2. `finalize()` expands `bot_config` path with `shellexpand`.
3. `start_pgm()` initializes tracing and logs build metadata from `build.rs` env vars.
4. `Bot::new()` loads and validates JSON configuration and compiles `url_regex` without network activity.
5. `Bot::run()` creates a lazy database pool, authenticates each workspace, discovers public and private channels,
   and starts Socket Mode listeners. Each listener must receive its first valid hello within 30 seconds.

Authentication and app lookup each have a 10-second deadline; paginated channel discovery has a 30-second deadline.
Unrecovered session/startup failures trigger cleanup and retries with delays growing from 2 to 30 seconds plus up to
one second of jitter. Startup rate limits honor `Retry-After`; a session healthy for a minute resets the backoff.
The SDK continues to handle ordinary socket reconnects. Local configuration errors fail before retries begin.

### Event and message flow

1. `Bot::run()` creates a bounded Tokio MPSC channel for message processing.
2. One scoped worker runs `handle_messages(rx)` and serially processes queued message events.
3. For each workspace, a Socket Mode listener is started.
4. Callback behavior:

- `handler_push_events`: filters and immediately submits relevant `Message` events without network lookups or waiting
  for queue space. Plain, bot, me,
  thread-broadcast, and file-share messages are supported; edited, deleted, hidden, and other unsupported message
  subtypes are ignored.
- `handler_interaction_events`: logs at DEBUG and returns `Ok(())`.
- `handler_error`: obtains diagnostic state nonblockingly. An unavailable worker requests redelivery; ordinary SDK
  errors are logged and acknowledged. Identity/authentication failures trigger supervised session cleanup.

The queue holds 256 messages, retaining only required text, sender fields, attribution, and timing. Messages over
64 KiB are discarded whole. Queued work expires three minutes after receipt; already-started processing may finish.
A full queue discards new messages and acknowledges them. Event IDs are remembered only after successful admission,
in an installation-scoped cache of at most 8,192 entries for 15 minutes. This suppresses redelivery duplicates, not
legitimate repeated posts of the same URL. The cache survives session retries but resets when the process restarts.

Discard counts for overflow, expiry, oversized input, and invalid input are reported every 30 seconds at ERROR and
again on exit. Duplicate counts are reported at DEBUG. There are no per-workspace fairness budgets.

SIGINT/SIGTERM stops admission, explicitly shuts down each SDK listener (up to five seconds each), and allows the
worker up to ten seconds to drain. The worker is cancelled when that budget expires and cannot survive a reconnect.

### URL extraction and DB writes

`handle_msg()`:

1. Resolves channel id to `workspace-channel`, or `workspace-CHANNEL_ID` if lookup is unavailable.
2. Resolves the sender name from message fields, a workspace-local cache, or `users.info`.
3. Logs message text if present.
4. Examines at most 16 `url_regex` matches over text, removes Slack link labels, decodes `&amp;`, `&lt;`, and `&gt;`
   exactly once, and validates/normalizes HTTP/HTTPS URLs. URLs exceeding **2,048 UTF-8 bytes** before or after
   normalization are skipped whole. Stored URLs are never truncated.
5. Applies the combined global/workspace blacklist and public-address/DNS policy. Invalid and excluded URLs still
   count toward the 16-match limit. Allowed URLs reuse the runtime DB pool created with `start_db(url_log_db)`.
6. Calls `db_add_url()` with the original Slack message timestamp (whole seconds), channel, sender name, and URL.

Sender and channel caches each retain at most 4,096 entries per workspace. Sender names expire after an hour;
channel names after ten minutes, allowing renamed/new channels to be discovered. Failed lookups expire after 30
seconds. Each lookup has a three-second deadline, and rate limits defer that API's further lookups until `Retry-After`.
Lookup failure falls back to a user ID or workspace/channel ID and does not prevent URL harvesting.

`db_add_url()` behavior:

- Executes `insert into url (seen, channel, nick, url) values (...)`.
- Uses a lazy pool of at most four connections, with a three-second acquisition timeout. A database outage does not
  prevent Slack startup. Each connection sets a five-second statement timeout and one-second lock timeout.
- Bounds each insert to eight seconds overall, logs failures, and continues without application-level retries.
- Does not update a separate marker table. Database triggers installed by `urlharvest-rs` publish change notifications
  when `url` is modified.

Harvesting is best effort: crashes, full/expired queues, failed inserts, or a shutdown drain timeout can lose URLs.
There is no durable spool, exactly-once delivery, or database schema change. URLs are stored without being fetched;
downstream fetchers remain responsible for their own destination, response-size, and rendering policies.

## License

This project is licensed under `MIT OR Apache-2.0`.
