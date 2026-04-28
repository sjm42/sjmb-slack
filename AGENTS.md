# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust 2024 crate with one executable and a small library core.

- `src/bin/sjmb_slack.rs`: binary entrypoint (`main`) and CLI startup.
- `src/lib.rs`: module exports.
- `src/config.rs`: CLI flags, config path expansion, log level setup, and runtime initialization.
- `src/slackbot.rs`: Slack Socket Mode handling, sender name lookup/cache, and URL extraction flow.
- `src/db_util.rs`: PostgreSQL helpers and retry logic for URL inserts.
- `config/sjmb_slack.json`: example runtime config template.
- `config/sjmb_slack.manifest.yaml`: Slack app manifest for scopes and event subscriptions.
- `build.rs`: injects build metadata (git commit/branch, compiler info) and fails the build if metadata emission fails.
- `install.sh`: copies release binary to `$HOME/sjmb_slack/bin`.

## Build, Test, and Development Commands
- `cargo build`: debug build.
- `cargo build --release`: optimized production build.
- `cargo run -- --verbose --bot-config config/sjmb_slack.json`: run locally with sample config.
- `cargo run --release -- --verbose --bot-config config/sjmb_slack.json`: run optimized build locally.
- `cargo check`: fast compile checks during development.
- `cargo fmt --check`: formatting check.
- `cargo clippy --all-targets --all-features`: linting pass.
- `cargo test`: run unit/integration tests.
- `cargo build --release && ./install.sh`: build and install to local bin directory.

## Coding Style & Naming Conventions
- Use stable Rust toolchain (`rust-toolchain.toml`) and Rust 2024 edition.
- Format with `cargo fmt` before opening a PR.
- Keep `clippy` clean with `cargo clippy --all-targets --all-features`; fix warnings rather than ignoring them unless there is a clear reason.
- Naming: `snake_case` for functions/modules/files, `PascalCase` for structs/enums, `UPPER_SNAKE_CASE` for constants.
- Prefer small, focused modules by concern (`config`, `slackbot`, `db_util`).

## Testing Guidelines
There is currently no large committed test suite; add tests with each behavior change.

- Unit tests: colocate in the same file via `#[cfg(test)] mod tests`.
- Integration tests: place in `tests/` for cross-module behavior.
- Prefer deterministic tests (mock Slack payloads and DB boundaries where possible).
- Run `cargo fmt`, `cargo test`, and `cargo clippy --all-targets --all-features` before submitting.

## Commit & Pull Request Guidelines
- Commit messages in history are short and imperative (example: `cargo update`).
- Keep subjects concise and action-oriented (example: `fix url insert retry logging`).
- Separate dependency-only updates from functional changes when possible.
- PRs should include what changed and why, modules touched, test/lint commands run, and any config or database impact.

## Security & Configuration Tips
- Never commit real Slack tokens or production DB credentials.
- Keep local secrets in your runtime config (for example `$HOME/sjmb_slack/config/sjmb_slack.json`), including both bot and app-level Socket Mode tokens.
- Validate required PostgreSQL tables (`url`, `url_changed`) before deploying.
