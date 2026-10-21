use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    sync::{Arc, Mutex, MutexGuard, atomic::Ordering},
};

use anyhow::{Context, ensure};
#[cfg(feature = "socket-mode")]
use anyhow::{anyhow, bail};
use regex::Regex;

#[cfg(feature = "socket-mode")]
mod socket_mode;
#[cfg(feature = "user-session")]
mod user_session;
use serde::{Deserialize, Serialize};
use slack_morphism::{errors::SlackClientError, prelude::*};
#[cfg(feature = "socket-mode")]
use std::sync::atomic::AtomicBool;
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{Duration, Instant, sleep, timeout},
};

use crate::{
    harvest::{extract_urls, message_timestamp},
    limits::{Counters, ExpiringCache, log_text},
    url_filter::UrlFilter,
    *,
};

const CONFIG_BYTES: u64 = 1024 * 1024;
const MESSAGE_BYTES: usize = 64 * 1024;
const CACHE_CAPACITY: usize = 4096;
const USER_TTL: Duration = Duration::from_secs(3600);
const CHANNEL_TTL: Duration = Duration::from_secs(600);
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
const QUEUE_AGE: Duration = Duration::from_secs(180);
#[cfg(feature = "socket-mode")]
const API_TIMEOUT: Duration = Duration::from_secs(10);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct SlackWorkspace {
    name: String,
    #[serde(default)]
    url_blacklist: Vec<String>,
    #[serde(skip)]
    url_filter: UrlFilter,
    api_token: String,
    #[serde(default)]
    socket_token: String,
    #[serde(default)]
    cookie: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Backend {
    SocketMode,
    UserSession,
}

impl Default for Backend {
    fn default() -> Self {
        if cfg!(feature = "socket-mode") {
            Self::SocketMode
        } else {
            Self::UserSession
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
struct RecoveryConfig {
    state_dir: String,
    window_seconds: u64,
    max_pages_per_channel: usize,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            state_dir: "$HOME/.local/state/sjmb-slack".into(),
            window_seconds: 86400,
            max_pages_per_channel: 100,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Bot {
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    backend: Backend,
    #[serde(default)]
    recovery: RecoveryConfig,
    url_regex: String,
    url_log_db: String,
    #[serde(default)]
    url_blacklist: Vec<String>,
    workspaces: Vec<SlackWorkspace>,
    #[serde(skip)]
    url_re: Option<Regex>,
}

#[derive(Debug)]
struct Workspace {
    name: String,
    url_filter: UrlFilter,
    team_id: SlackTeamId,
    #[cfg(feature = "socket-mode")]
    app_id: SlackAppId,
    client: Arc<SlackHyperClient>,
    token: SlackApiToken,
    users: Mutex<ExpiringCache<String, String>>,
    channels: Mutex<ExpiringCache<String, String>>,
    user_retry_at: Mutex<Option<Instant>>,
    channel_retry_at: Mutex<Option<Instant>>,
    #[cfg(feature = "socket-mode")]
    health: socket_mode::Health,
    #[cfg(feature = "user-session")]
    session_api: Option<Arc<user_session::SessionApi>>,
}

impl Workspace {
    async fn sender_nick(&self, sender: &SenderInfo) -> String {
        let Some(user) = &sender.user else {
            return sender
                .hint
                .clone()
                .or_else(|| sender.bot.clone())
                .unwrap_or_else(|| "N/A".into());
        };
        if let Some(hint) = &sender.hint {
            lock(&self.users).insert(user.0.clone(), hint.clone(), Instant::now(), USER_TTL);
            return hint.clone();
        }
        if let Some(nick) = lock(&self.users).get(&user.0, Instant::now()) {
            return nick;
        }
        let mut nick = None;
        if lookup_allowed(&self.user_retry_at) {
            match timeout(LOOKUP_TIMEOUT, self.fetch_user(user)).await {
                Ok(Ok(user)) => nick = slack_user_nick(&user).map(|nick| log_text(&nick, 256)),
                Ok(Err(error)) => self.lookup_error("users.info", &error, &self.user_retry_at),
                Err(_) => warn!("WS {} users.info timed out", self.name),
            }
        }
        let ttl = if nick.is_some() {
            USER_TTL
        } else {
            NEGATIVE_TTL
        };
        let nick = nick.unwrap_or_else(|| slack_user_fallback(user));
        lock(&self.users).insert(user.0.clone(), nick.clone(), Instant::now(), ttl);
        nick
    }

    async fn channel_name(&self, id: &str) -> String {
        if let Some(name) = lock(&self.channels).get(&id.to_string(), Instant::now()) {
            return format!("{}-{name}", self.name);
        }
        let mut name = None;
        if lookup_allowed(&self.channel_retry_at) {
            match timeout(LOOKUP_TIMEOUT, self.fetch_channel(id)).await {
                Ok(Ok(response)) => {
                    name = response
                        .name_normalized
                        .as_deref()
                        .and_then(non_empty)
                        .map(|name| log_text(name, 256))
                }
                Ok(Err(error)) => {
                    self.lookup_error("conversations.info", &error, &self.channel_retry_at)
                }
                Err(_) => warn!("WS {} conversations.info timed out", self.name),
            }
        }
        let ttl = if name.is_some() {
            CHANNEL_TTL
        } else {
            NEGATIVE_TTL
        };
        let name = name.unwrap_or_else(|| id.to_owned());
        lock(&self.channels).insert(id.to_string(), name.clone(), Instant::now(), ttl);
        format!("{}-{name}", self.name)
    }

    async fn fetch_user(&self, user: &SlackUserId) -> anyhow::Result<SlackUser> {
        #[cfg(feature = "user-session")]
        if let Some(api) = &self.session_api {
            let response = api.call("users.info", &[("user", &user.0)]).await?;
            return Ok(serde_json::from_value(response["user"].clone())?);
        }
        Ok(self
            .client
            .open_session(&self.token)
            .users_info(&SlackApiUsersInfoRequest::new(user.clone()))
            .await?
            .user)
    }

    async fn fetch_channel(&self, id: &str) -> anyhow::Result<SlackChannelInfo> {
        #[cfg(feature = "user-session")]
        if let Some(api) = &self.session_api {
            let response = api.call("conversations.info", &[("channel", id)]).await?;
            return Ok(serde_json::from_value(response["channel"].clone())?);
        }
        Ok(self
            .client
            .open_session(&self.token)
            .conversations_info(&SlackApiConversationsInfoRequest::new(id.into()))
            .await?
            .channel)
    }

    fn lookup_error(&self, method: &str, error: &anyhow::Error, retry_at: &Mutex<Option<Instant>>) {
        warn!("WS {} {method} failed: {error:?}", self.name);
        if let Some(SlackClientError::RateLimitError(rate)) =
            error.downcast_ref::<SlackClientError>()
        {
            let delay = rate.retry_after.unwrap_or(NEGATIVE_TTL);
            *lock(retry_at) = Instant::now().checked_add(delay);
        }
    }
}

fn lookup_allowed(retry_at: &Mutex<Option<Instant>>) -> bool {
    lock(retry_at).is_none_or(|at| at <= Instant::now())
}

#[derive(Debug)]
struct SenderInfo {
    user: Option<SlackUserId>,
    bot: Option<String>,
    hint: Option<String>,
}

impl From<SlackMessageSender> for SenderInfo {
    fn from(sender: SlackMessageSender) -> Self {
        let hint = sender
            .username
            .as_deref()
            .and_then(non_empty)
            .map(str::to_owned)
            .or_else(|| {
                sender
                    .user_profile
                    .as_ref()
                    .and_then(slack_user_profile_nick)
            })
            .map(|nick| log_text(&nick, 256));
        Self {
            user: sender.user,
            bot: sender.bot_id.map(|bot| log_text(&bot.0, 256)),
            hint,
        }
    }
}

#[derive(Debug)]
struct QueuedMessage {
    workspace: Arc<Workspace>,
    expires_at: Instant,
    ts: i64,
    channel: String,
    text: String,
    sender: SenderInfo,
    completion: Option<oneshot::Sender<bool>>,
    dry_run: bool,
}

impl Bot {
    pub async fn new(opts: &OptsCommon) -> anyhow::Result<Self> {
        info!("Reading config file {}", opts.bot_config);
        let mut bytes = Vec::new();
        File::open(&opts.bot_config)
            .context("could not open bot configuration")?
            .take(CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        Self::from_config(&bytes)
    }

    fn from_config(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(
            bytes.len() as u64 <= CONFIG_BYTES,
            "bot configuration exceeds 1 MiB"
        );
        let mut bot: Self =
            serde_json::from_slice(bytes).context("invalid bot configuration JSON")?;
        ensure!(
            !bot.workspaces.is_empty(),
            "configure at least one Slack workspace"
        );
        let mut names = HashSet::new();
        let mut sockets = HashSet::new();
        ensure!(
            match bot.backend {
                Backend::SocketMode => cfg!(feature = "socket-mode"),
                Backend::UserSession => cfg!(feature = "user-session"),
            },
            "requested backend was not compiled into this binary"
        );
        if bot.backend == Backend::UserSession {
            ensure!(
                (1..=604800).contains(&bot.recovery.window_seconds),
                "recovery.window_seconds must be between 1 and 604800"
            );
            ensure!(
                (1..=1000).contains(&bot.recovery.max_pages_per_channel),
                "recovery.max_pages_per_channel must be between 1 and 1000"
            );
            bot.recovery.state_dir = shellexpand::full(&bot.recovery.state_dir)?.into_owned();
            ensure!(
                !bot.recovery.state_dir.trim().is_empty(),
                "recovery.state_dir must not be empty"
            );
        }
        for workspace in &bot.workspaces {
            ensure!(
                !workspace.name.trim().is_empty()
                    && workspace.name.len() <= 256
                    && !workspace.name.chars().any(char::is_control),
                "workspace names must be nonempty and at most 256 bytes without controls"
            );
            ensure!(
                names.insert(&workspace.name),
                "duplicate workspace name: {}",
                workspace.name
            );
            ensure!(
                !workspace.api_token.trim().is_empty(),
                "workspace {} has an empty token",
                workspace.name
            );
            match bot.backend {
                Backend::SocketMode => {
                    ensure!(
                        !workspace.socket_token.trim().is_empty(),
                        "workspace {} has an empty socket token",
                        workspace.name
                    );
                    ensure!(
                        sockets.insert(&workspace.socket_token),
                        "duplicate Socket Mode token"
                    );
                }
                Backend::UserSession => {
                    ensure!(
                        workspace.api_token.starts_with("xoxc-")
                            && workspace.api_token.len() <= 8192,
                        "user-session requires an xoxc session token"
                    );
                    http::HeaderValue::from_str(&format!("Bearer {}", workspace.api_token))
                        .context("invalid session token header")?;
                    ensure!(
                        workspace.cookie.len() <= 16384,
                        "session cookie exceeds 16 KiB"
                    );
                    let cookie = http::HeaderValue::from_str(&workspace.cookie)
                        .context("invalid session Cookie header")?;
                    ensure!(
                        !cookie.is_empty()
                            && workspace.cookie.split(';').any(|part| part
                                .trim()
                                .strip_prefix("d=")
                                .is_some_and(|value| !value.is_empty())),
                        "user-session requires a Cookie header containing d=<value>"
                    );
                    ensure!(
                        workspace.socket_token.is_empty(),
                        "socket_token is unused by user-session; remove it"
                    );
                }
            }
        }
        let global_filter = UrlFilter::compile(&bot.url_blacklist, "global")?;
        for workspace in &mut bot.workspaces {
            workspace.url_filter = UrlFilter::compile(
                &workspace.url_blacklist,
                &format!("workspace {}", workspace.name),
            )?;
            workspace.url_filter.include(&global_filter);
        }
        bot.url_log_db
            .parse::<sqlx::postgres::PgConnectOptions>()
            .context("invalid URL database configuration")?;
        let regex = Regex::new(&bot.url_regex).context("invalid url_regex")?;
        ensure!(
            regex.captures_len() >= 2,
            "url_regex requires capture group 1"
        );
        bot.url_re = Some(regex);
        Ok(bot)
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let counters = Arc::new(Counters::default());
        let result = tokio::select! {
            result = self.run_sessions(counters.clone(), shutdown_signal()) => result,
            _ = report_counters(&counters) => unreachable!("counter reporter runs until cancelled"),
        };
        counters.report();
        result
    }

    async fn run_sessions(
        &self,
        counters: Arc<Counters>,
        shutdown: impl Future<Output = anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        match self.backend {
            #[cfg(feature = "socket-mode")]
            Backend::SocketMode => self.run_socket_sessions(counters, shutdown).await,
            #[cfg(feature = "user-session")]
            Backend::UserSession => self.run_user_sessions(counters, shutdown).await,
            // Single-feature builds still need an error for the missing backend.
            #[allow(unreachable_patterns)]
            _ => {
                let _ = (counters, shutdown);
                anyhow::bail!("requested backend is not compiled in")
            }
        }
    }
}

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn report_counters(counters: &Counters) {
    let mut report = tokio::time::interval(Duration::from_secs(30));
    loop {
        report.tick().await;
        counters.report();
    }
}

fn prepare_message(
    workspace: Arc<Workspace>,
    message: SlackMessageEvent,
    counters: &Counters,
) -> Option<QueuedMessage> {
    let expires_at = Instant::now() + QUEUE_AGE;
    let text = message.content?.text?;
    let channel = message.origin.channel?.0;
    if text.len() > MESSAGE_BYTES {
        counters.oversized_messages.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    if channel.len() > 256
        || channel.is_empty()
        || message
            .sender
            .user
            .as_ref()
            .is_some_and(|user| user.0.len() > 256)
    {
        counters.invalid.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let ts = match message_timestamp(&message.origin.ts.0) {
        Ok(ts) => ts,
        Err(_) => {
            counters.invalid.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    Some(QueuedMessage {
        workspace,
        expires_at,
        ts,
        channel,
        text,
        sender: message.sender.into(),
        completion: None,
        dry_run: false,
    })
}

async fn handle_messages(
    mut rx: mpsc::Receiver<QueuedMessage>,
    regex: &Regex,
    db: &DbCtx,
    counters: &Counters,
    mut drain: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut draining = false;
    loop {
        let message = tokio::select! {
            biased;
            _ = drain.changed(), if !draining => { draining = true; rx.close(); continue; }
            message = rx.recv() => message,
        };
        let Some(mut message) = message else {
            return Ok(());
        };
        let completion = message.completion.take();
        if message.expires_at <= Instant::now() {
            if let Some(completion) = completion {
                let _ = completion.send(false);
            }
            counters.expired.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let result = handle_msg(message, regex, db, counters).await;
        if let Some(completion) = completion {
            let _ = completion.send(result.is_ok());
        }
        if let Err(error) = result {
            error!("Slack message handling failed: {error:#}");
        }
    }
}

async fn handle_msg(
    message: QueuedMessage,
    regex: &Regex,
    db: &DbCtx,
    counters: &Counters,
) -> anyhow::Result<()> {
    let nick = message.workspace.sender_nick(&message.sender).await;
    let channel = message.workspace.channel_name(&message.channel).await;
    info!(
        "#{} ({}): {}",
        log_text(&channel, 1024),
        log_text(&nick, 256),
        log_text(&message.text, 4096)
    );
    let mut failed = false;
    for url in extract_urls(regex, &message.text, counters) {
        let record = UrlCtx {
            ts: message.ts,
            chan: channel.clone(),
            nick: nick.clone(),
            url,
        };
        info!(
            "*** on {channel} detected url: {}",
            log_text(&record.url, 2048)
        );
        if let Err(reason) = message.workspace.url_filter.check(&record.url).await {
            warn!("URL insert skipped on {channel}: {reason:#}");
            continue;
        }
        if message.dry_run {
            info!("Dry run: URL insert skipped");
            continue;
        }
        match db_add_url(db, &record).await {
            Ok(rows) => info!("Urllog: inserted {rows} row(s)"),
            Err(error) => {
                failed = true;
                error!("Urllog insert failed on {channel}: {error:#}");
            }
        }
    }
    ensure!(!failed, "one or more URL inserts failed");
    Ok(())
}

fn should_process_message(message: &SlackMessageEvent) -> bool {
    !message.hidden.unwrap_or(false)
        && matches!(
            message.subtype,
            None | Some(SlackMessageEventType::BotMessage)
                | Some(SlackMessageEventType::MeMessage)
                | Some(SlackMessageEventType::FileShare)
                | Some(SlackMessageEventType::ThreadBroadcast)
        )
}

fn non_empty(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn slack_user_nick(user: &SlackUser) -> Option<String> {
    user.profile
        .as_ref()
        .and_then(slack_user_profile_nick)
        .or_else(|| {
            user.real_name
                .as_deref()
                .and_then(non_empty)
                .map(str::to_owned)
        })
        .or_else(|| user.name.as_deref().and_then(non_empty).map(str::to_owned))
}

fn slack_user_profile_nick(profile: &SlackUserProfile) -> Option<String> {
    [
        profile.display_name.as_deref(),
        profile.display_name_normalized.as_deref(),
        profile.real_name.as_deref(),
        profile.real_name_normalized.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(non_empty)
    .map(str::to_owned)
}

fn slack_user_fallback(user_id: &SlackUserId) -> String {
    log_text(&format!("<@{}>", user_id.0), 256)
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[tokio::test]
    async fn config_combines_global_and_workspace_rules_without_crossing_workspaces() {
        let socket = cfg!(feature = "socket-mode");
        let config = serde_json::json!({
            "url_regex": "<(https?://[^>]+)>", "url_log_db": "postgres:///url",
            "url_blacklist": ["1.1.1.1"],
            "workspaces": [
                {"name": "one", "api_token": "xoxc-one", "cookie": "d=test",
                 "socket_token": if socket { "xapp-one" } else { "" }, "url_blacklist": ["8.8.8.8"]},
                {"name": "two", "api_token": "xoxc-two", "cookie": "d=test",
                 "socket_token": if socket { "xapp-two" } else { "" }}
            ]
        });
        let bot = Bot::from_config(&serde_json::to_vec(&config).unwrap()).unwrap();
        for workspace in &bot.workspaces {
            assert!(
                workspace
                    .url_filter
                    .check("https://1.1.1.1/")
                    .await
                    .is_err()
            );
        }
        assert!(
            bot.workspaces[0]
                .url_filter
                .check("https://8.8.8.8/")
                .await
                .is_err()
        );
        assert!(
            bot.workspaces[1]
                .url_filter
                .check("https://8.8.8.8/")
                .await
                .is_ok()
        );

        let mut bad = config.clone();
        bad["url_blacklist"] = serde_json::json!(["https://example.com"]);
        assert!(
            Bot::from_config(&serde_json::to_vec(&bad).unwrap())
                .unwrap_err()
                .to_string()
                .contains("global url_blacklist entry 1")
        );
        let mut bad = config;
        bad["workspaces"][1]["url_blacklist"] = serde_json::json!(["*.example.com"]);
        assert!(
            Bot::from_config(&serde_json::to_vec(&bad).unwrap())
                .unwrap_err()
                .to_string()
                .contains("workspace two url_blacklist entry 1")
        );
    }
}
