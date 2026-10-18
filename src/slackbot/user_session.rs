//! Minimal read-only Slack user client. It uses its own cursors, never the
//! user's Slack read markers, and does not send presence, typing or read updates.
mod api;
mod state;

use super::*;
use anyhow::{anyhow, bail};
pub(super) use api::SessionApi;
use api::{ApiError, authentication_error};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use state::{MAX_CHANNELS, OVERLAP, Store, valid_id};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};

const FRAME_BYTES: usize = 1024 * 1024;
const PAGE_SIZE: &str = "200";
const PING_INTERVAL: Duration = Duration::from_secs(15);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(30);

fn now() -> i64 {
    Utc::now().timestamp()
}

#[derive(Debug)]
struct Session {
    workspace: Arc<Workspace>,
    api: Arc<SessionApi>,
    store: Arc<Store>,
    config: RecoveryConfig,
    members: Mutex<HashSet<String>>,
    processing: tokio::sync::Mutex<()>,
    reconnect_url: Mutex<Option<String>>,
    tx: mpsc::Sender<QueuedMessage>,
    counters: Arc<Counters>,
    dry_run: bool,
    token: String,
}

enum Event {
    Message(Value),
    Checkpoint(i64),
}

impl Bot {
    pub(super) async fn run_user_sessions(
        &self,
        counters: Arc<Counters>,
        shutdown: impl Future<Output = anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        let db = start_db(&self.url_log_db).await?;
        let (tx, rx) = mpsc::channel(MESSAGE_QUEUE_BOUND);
        let (drain_tx, drain_rx) = watch::channel(false);
        let regex = self.url_re.as_ref().context("url_regex not initialized")?;
        let mut worker = Box::pin(handle_messages(rx, regex, &db, &counters, drain_rx));
        let mut sessions: Vec<Arc<Session>> = Vec::new();
        let mut recovery = self.recovery.clone();
        if self.dry_run {
            recovery.state_dir = std::path::Path::new(&recovery.state_dir)
                .join("probe")
                .to_string_lossy()
                .into_owned();
        }
        let mut connections = Box::pin(async {
            // Initialization is also cancellable by shutdown and worker failure.
            for config in &self.workspaces {
                let session = Session::new(
                    config,
                    &recovery,
                    tx.clone(),
                    counters.clone(),
                    self.dry_run,
                )
                .await?;
                sessions.push(Arc::new(session));
            }
            drop(tx);
            futures::future::try_join_all(sessions.iter().map(|s| s.run())).await?;
            Ok::<(), anyhow::Error>(())
        });
        tokio::pin!(shutdown);
        let mut worker_finished = false;
        let result = tokio::select! {
            result = &mut shutdown => result,
            result = &mut connections => result,
            result = &mut worker => { worker_finished = true; result.and_then(|()| Err(anyhow!("message worker stopped unexpectedly"))) },
        };
        drop(connections);
        let _ = drain_tx.send(true);
        if !worker_finished {
            match timeout(DRAIN_TIMEOUT, &mut worker).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => error!("Message worker failed: {error:#}"),
                Err(_) => error!("Message queue drain timed out; remaining work discarded"),
            }
        }
        for session in &sessions {
            session.store.save().await?;
        }
        result
    }
}

impl Session {
    async fn new(
        config: &SlackWorkspace,
        recovery: &RecoveryConfig,
        tx: mpsc::Sender<QueuedMessage>,
        counters: Arc<Counters>,
        dry_run: bool,
    ) -> anyhow::Result<Self> {
        let api = SessionApi::new(config)?;
        let mut delay = 2;
        let auth = loop {
            match api.call("auth.test", &[]).await {
                Ok(auth) => break auth,
                Err(error) if authentication_error(&error) => return Err(error),
                Err(error) => {
                    let pause = retry_delay(delay, &error);
                    warn!(
                        "WS {} authentication request failed: {error:#}; retry in {}s",
                        config.name,
                        pause.as_secs()
                    );
                    sleep(pause).await;
                    delay = (delay * 2).min(30);
                }
            }
        };
        let team = auth["team_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .context("auth.test returned invalid team_id")?;
        let user = auth["user_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .context("auth.test returned invalid user_id")?;
        let store = Arc::new(Store::open(recovery, team, user)?);
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
        let token = SlackApiToken::new(config.api_token.clone().into())
            .with_cookie(config.cookie.clone().into());
        let workspace = Arc::new(Workspace {
            name: config.name.clone(),
            team_id: team.into(),
            #[cfg(feature = "socket-mode")]
            app_id: format!("user-session-{user}").into(),
            client,
            token,
            users: Mutex::new(ExpiringCache::new(CACHE_CAPACITY)),
            channels: Mutex::new(ExpiringCache::new(CACHE_CAPACITY)),
            user_retry_at: Mutex::new(None),
            channel_retry_at: Mutex::new(None),
            #[cfg(feature = "socket-mode")]
            health: socket_mode::Health::default(),
            session_api: Some(api.clone()),
        });
        info!(
            "WS {} authenticated user session for {team}/{user}",
            config.name
        );
        Ok(Self {
            workspace,
            api,
            store,
            config: recovery.clone(),
            members: Mutex::new(HashSet::new()),
            processing: tokio::sync::Mutex::new(()),
            reconnect_url: Mutex::new(None),
            tx,
            counters,
            dry_run,
            token: config.api_token.clone(),
        })
    }

    async fn run(&self) -> anyhow::Result<()> {
        let mut backoff = 2;
        loop {
            let since = Instant::now();
            let result = self.connection().await;
            self.store.save().await?;
            let error = result
                .err()
                .unwrap_or_else(|| anyhow!("user session ended"));
            if authentication_error(&error) {
                return Err(error);
            }
            if since.elapsed() >= Duration::from_secs(60) {
                backoff = 2;
            }
            let delay = retry_delay(backoff, &error);
            warn!(
                "WS {} disconnected: {error:#}; reconnect in {}s",
                self.workspace.name,
                delay.as_secs_f32()
            );
            sleep(delay).await;
            backoff = (backoff * 2).min(30);
        }
    }

    async fn connection(&self) -> anyhow::Result<()> {
        let began = now();
        // Consume each reconnect ticket once. If it fails, the next attempt
        // falls back to the normal gateway; newly received tickets are retained.
        let url = lock(&self.reconnect_url)
            .take()
            .unwrap_or_else(|| gateway_url(&self.token, &self.workspace.team_id.0));
        let url = validate_socket_url(&url)?;
        let mut request = url.as_str().into_client_request()?;
        request
            .headers_mut()
            .insert(http::header::COOKIE, self.api.cookie.clone());
        let settings = WebSocketConfig::default()
            .max_message_size(Some(FRAME_BYTES))
            .max_frame_size(Some(FRAME_BYTES))
            .write_buffer_size(4096)
            .max_write_buffer_size(FRAME_BYTES);
        let connection = timeout(
            START_TIMEOUT,
            connect_async_with_config(request, Some(settings), true),
        )
        .await
        .context("user websocket connection timed out")?;
        let (mut socket, _) = match connection {
            Ok(connection) => connection,
            Err(tokio_tungstenite::tungstenite::Error::Http(response))
                if matches!(response.status().as_u16(), 401 | 403) =>
            {
                return Err(ApiError::Authentication(format!(
                    "websocket HTTP {}",
                    response.status()
                ))
                .into());
            }
            Err(tokio_tungstenite::tungstenite::Error::Http(response))
                if response.status() == http::StatusCode::TOO_MANY_REQUESTS =>
            {
                return Err(ApiError::RateLimited(api::retry_after(response.headers())).into());
            }
            Err(error) => return Err(error.into()),
        };
        timeout(START_TIMEOUT, async {
            loop {
                let frame = socket
                    .next()
                    .await
                    .context("websocket closed before hello")??;
                match frame {
                    Message::Text(text) => {
                        let event: Value = serde_json::from_str(&text)?;
                        check_error(&event)?;
                        if event["type"] == "hello" {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                    Message::Ping(data) => socket.send(Message::Pong(data)).await?,
                    Message::Close(_) => bail!("websocket closed before hello"),
                    _ => {}
                }
            }
        })
        .await
        .context("user websocket hello timed out")??;
        let (events_tx, events_rx) = mpsc::channel(MESSAGE_QUEUE_BOUND);
        let reader = self.read_socket(socket, events_tx);
        let processing = async {
            self.discover_channels().await?;
            let channels = lock(&self.members).iter().cloned().collect::<Vec<_>>();
            self.store.begin_connection(&channels, began)?;
            self.store.save().await?;
            info!(
                "WS {} connected; {} joined channels",
                self.workspace.name,
                channels.len()
            );
            let consumer = self.consume(events_rx);
            let recovery = self.recover(began);
            tokio::try_join!(consumer, recovery)?;
            Ok::<(), anyhow::Error>(())
        };
        tokio::pin!(reader, processing);
        tokio::select! { result = &mut reader => result, result = &mut processing => result }
    }

    async fn read_socket<S>(
        &self,
        mut socket: tokio_tungstenite::WebSocketStream<S>,
        events: mpsc::Sender<Event>,
    ) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut tick = tokio::time::interval(PING_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut pending: Option<(u64, Instant, i64)> = None;
        let mut sequence = 0u64;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Some((_,sent,_)) = pending { ensure!(sent.elapsed() < PONG_TIMEOUT,"Slack heartbeat timed out"); }
                    else {
                        sequence = sequence.wrapping_add(1);
                        timeout(Duration::from_secs(5),socket.send(Message::Text(json!({"type":"ping","id":sequence}).to_string().into()))).await.context("websocket write timed out")??;
                        pending = Some((sequence,Instant::now(),now()));
                    }
                }
                frame = socket.next() => {
                    match frame.context("Slack websocket closed")?? {
                        Message::Text(text) => {
                            let event: Value = match serde_json::from_str(&text) {
                                Ok(value) => value,
                                Err(error) => { self.counters.invalid.fetch_add(1,Ordering::Relaxed); warn!("Invalid Slack websocket JSON: {error}"); continue; }
                            };
                            check_error(&event)?;
                            match event["type"].as_str() {
                                Some("pong") => {
                                    if let Some((id,_,at)) = pending
                                        && event["reply_to"].as_u64().is_none_or(|reply| reply == id) {
                                        // An ordered barrier: only processed messages before this
                                        // heartbeat can contribute to a saved progress checkpoint.
                                        self.enqueue_event(&events,Event::Checkpoint(at))?;
                                        pending = None;
                                    }
                                },
                                Some("reconnect_url") => {
                                    if let Some(url) = event["url"].as_str() { validate_socket_url(url)?; *lock(&self.reconnect_url) = Some(url.into()); }
                                },
                                Some("goodbye" | "disconnect") => bail!("Slack requested reconnect"),
                                Some("message" | "channel_joined" | "group_joined" | "channel_left" | "group_left") => {
                                    self.enqueue_event(&events,Event::Message(event))?;
                                },
                                _ => {},
                            }
                        },
                        Message::Ping(data) => { timeout(Duration::from_secs(5),socket.send(Message::Pong(data))).await.context("websocket pong timed out")??; },
                        Message::Close(_) => bail!("Slack closed websocket"),
                        _ => {},
                    }
                }
            }
        }
    }

    fn enqueue_event(&self, events: &mpsc::Sender<Event>, event: Event) -> anyhow::Result<()> {
        events.try_send(event).map_err(|error| {
            self.counters.full.fetch_add(1, Ordering::Relaxed);
            anyhow!("live event queue unavailable; reconnecting for recovery: {error}")
        })
    }

    async fn discover_channels(&self) -> anyhow::Result<()> {
        let mut cursor = String::new();
        let mut ids = HashSet::new();
        let mut cursors = HashSet::new();
        loop {
            let response = self
                .api
                .call(
                    "users.conversations",
                    &[
                        ("types", "public_channel,private_channel"),
                        ("exclude_archived", "true"),
                        ("limit", PAGE_SIZE),
                        ("cursor", &cursor),
                    ],
                )
                .await?;
            let channels = response["channels"]
                .as_array()
                .context("missing joined channel list")?;
            for channel in channels {
                if eligible_channel(channel) {
                    let id = channel["id"].as_str().context("missing channel id")?;
                    ensure!(
                        ids.len() < MAX_CHANNELS || ids.contains(id),
                        "joined channel limit exceeded"
                    );
                    ids.insert(id.to_owned());
                    self.cache_channel(channel);
                }
            }
            cursor = next_cursor(&response)?;
            if cursor.is_empty() {
                break;
            }
            ensure!(
                cursors.len() < 100 && cursors.insert(cursor.clone()),
                "channel pagination repeated or exceeded 100 pages"
            );
        }
        *lock(&self.members) = ids;
        Ok(())
    }

    fn cache_channel(&self, channel: &Value) {
        if let (Some(id), Some(name)) = (
            channel["id"].as_str(),
            channel["name_normalized"]
                .as_str()
                .or_else(|| channel["name"].as_str()),
        ) {
            lock(&self.workspace.channels).insert(
                id.into(),
                log_text(name, 256),
                Instant::now(),
                CHANNEL_TTL,
            );
        }
    }

    async fn consume(&self, mut events: mpsc::Receiver<Event>) -> anyhow::Result<()> {
        while let Some(event) = events.recv().await {
            match event {
                Event::Checkpoint(at) => {
                    self.store.checkpoint(at);
                    self.store.save().await?;
                }
                Event::Message(event) if event["type"] != "message" => {
                    self.discover_channels().await?;
                    for id in lock(&self.members).iter() {
                        self.store.add_channel(id, now())?;
                    }
                }
                Event::Message(event) => {
                    let Some(id) = event["channel"]
                        .as_str()
                        .filter(|id| valid_id(id) && (id.starts_with('C') || id.starts_with('G')))
                    else {
                        continue;
                    };
                    let known = lock(&self.members).contains(id);
                    if !known {
                        let response = self
                            .api
                            .call("conversations.info", &[("channel", id)])
                            .await?;
                        let channel = &response["channel"];
                        if !eligible_channel(channel) || channel["is_member"] != true {
                            continue;
                        }
                        ensure!(
                            lock(&self.members).len() < MAX_CHANNELS,
                            "joined channel limit exceeded"
                        );
                        lock(&self.members).insert(id.into());
                        self.store.add_channel(id, now())?;
                        self.cache_channel(channel);
                    }
                    self.process(event).await?;
                }
            }
        }
        bail!("live event receiver closed")
    }

    async fn process(&self, event: Value) -> anyhow::Result<()> {
        let _guard = self.processing.lock().await;
        let Some((message, ts, thread)) = normalise(event, &self.counters) else {
            return Ok(());
        };
        let id = message
            .origin
            .channel
            .as_ref()
            .context("missing channel")?
            .0
            .clone();
        if let Some(thread) = &thread {
            self.store.remember_thread(&id, thread);
        }
        if self.store.seen(&id, &ts) {
            self.counters.duplicates.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let Some(mut queued) = prepare_message(self.workspace.clone(), message, &self.counters)
        else {
            return Ok(());
        };
        let (completed, receipt) = oneshot::channel();
        queued.completion = Some(completed);
        queued.dry_run = self.dry_run;
        let result = async {
            timeout(Duration::from_secs(30), self.tx.send(queued))
                .await
                .context("message queue admission timed out")?
                .context("message worker closed")?;
            ensure!(
                timeout(Duration::from_secs(60), receipt)
                    .await
                    .context("message processing timed out")?
                    .context("message worker dropped receipt")?,
                "message processing failed"
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if result.is_ok() {
            self.store.processed(&id, &ts, thread.as_deref(), now());
        } else {
            self.store.failed(&id);
        }
        result
    }
}

fn eligible_channel(channel: &Value) -> bool {
    channel["id"]
        .as_str()
        .is_some_and(|id| valid_id(id) && (id.starts_with('C') || id.starts_with('G')))
        && channel["is_im"] != true
        && channel["is_mpim"] != true
        && channel["is_archived"] != true
}

fn normalise(
    event: Value,
    counters: &Counters,
) -> Option<(SlackMessageEvent, String, Option<String>)> {
    if event["type"] != "message" || event["hidden"] == true {
        return None;
    }
    if event["subtype"].as_str().is_some_and(|subtype| {
        !matches!(
            subtype,
            "bot_message" | "me_message" | "file_share" | "thread_broadcast"
        )
    }) {
        return None;
    }
    let ts = event["ts"].as_str()?;
    if ts.len() > 32 || message_timestamp(ts).is_err() {
        counters.invalid.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let ts = ts.to_owned();
    let thread = event["thread_ts"].as_str().map(str::to_owned).or_else(|| {
        event["reply_count"]
            .as_u64()
            .filter(|count| *count > 0)
            .map(|_| ts.clone())
    });
    let message: SlackMessageEvent = match serde_json::from_value(event) {
        Ok(value) => value,
        Err(_) => {
            counters.invalid.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    should_process_message(&message).then_some((message, ts, thread))
}

fn gateway_url(token: &str, team: &str) -> String {
    let mut url = url::Url::parse("wss://wss-primary.slack.com/").expect("constant Slack URL");
    url.query_pairs_mut()
        .append_pair("token", token)
        .append_pair("gateway_server", &format!("{team}-1"))
        .append_pair("slack_client", "desktop")
        .append_pair("batch_presence_aware", "1");
    url.into()
}

fn validate_socket_url(value: &str) -> anyhow::Result<url::Url> {
    ensure!(value.len() <= 16384, "websocket URL exceeds 16 KiB");
    let url = url::Url::parse(value)?;
    ensure!(
        url.scheme() == "wss"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".slack.com"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.fragment().is_none(),
        "untrusted Slack websocket URL"
    );
    Ok(url)
}

fn check_error(event: &Value) -> anyhow::Result<()> {
    if event["type"] == "error" {
        let code = event["error"]["code"]
            .as_str()
            .or_else(|| event["error"].as_str())
            .unwrap_or("websocket_error");
        if matches!(
            code,
            "invalid_auth"
                | "token_revoked"
                | "token_expired"
                | "account_inactive"
                | "not_authed"
                | "session_expired"
        ) {
            return Err(ApiError::Authentication(code.into()).into());
        }
        bail!(
            "Slack websocket error: {}",
            log_text(&event.to_string(), 1024)
        );
    }
    Ok(())
}

fn next_cursor(response: &Value) -> anyhow::Result<String> {
    let cursor = response["response_metadata"]["next_cursor"]
        .as_str()
        .unwrap_or("");
    ensure!(cursor.len() <= 4096, "pagination cursor exceeds 4 KiB");
    if cursor.is_empty() {
        ensure!(
            response["has_more"] != true,
            "Slack returned has_more without a cursor"
        );
    }
    Ok(cursor.into())
}

fn retry_delay(backoff: u64, error: &anyhow::Error) -> Duration {
    let jitter = (Utc::now().timestamp_subsec_millis() as u64) % 1000;
    let delay = Duration::from_millis(backoff * 1000 + jitter);
    match error.downcast_ref::<ApiError>() {
        Some(ApiError::RateLimited(pause)) => delay.max(*pause),
        _ => delay,
    }
}

mod recovery;
#[cfg(test)]
mod tests;
