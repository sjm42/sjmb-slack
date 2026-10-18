use super::*;
use tokio::sync::Notify;

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const DEDUP_TTL: Duration = Duration::from_secs(900);
type SocketListener =
    SlackClientSocketModeListener<SlackClientHyperConnector<SlackHyperHttpsConnector>>;
type SeenEvents = Arc<Mutex<ExpiringCache<(SlackTeamId, SlackAppId, String), ()>>>;

#[derive(Debug, Default)]
pub(super) struct Health {
    ready: AtomicBool,
    failure: Mutex<Option<String>>,
    changed: Notify,
}

impl Health {
    fn fail(&self, message: String) {
        lock(&self.failure).get_or_insert(message);
        self.changed.notify_one();
    }

    fn check(&self) -> anyhow::Result<()> {
        if let Some(message) = lock(&self.failure).as_ref() {
            bail!("{message}");
        }
        Ok(())
    }

    async fn wait_ready(&self) -> anyhow::Result<()> {
        loop {
            self.check()?;
            if self.ready.load(Ordering::Acquire) {
                return Ok(());
            }
            // notify_one retains a permit if notification races this await.
            self.changed.notified().await;
        }
    }

    async fn wait_failure(&self) -> anyhow::Result<()> {
        loop {
            self.check()?;
            self.changed.notified().await;
        }
    }
}

impl Workspace {
    async fn connect(config: &SlackWorkspace) -> anyhow::Result<Self> {
        crate::config::install_crypto_provider()?;
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
        Self::connect_with_client(config, client).await
    }

    async fn connect_with_client(
        config: &SlackWorkspace,
        client: Arc<SlackHyperClient>,
    ) -> anyhow::Result<Self> {
        let token = SlackApiToken::new(config.api_token.clone().into());
        let session = client.open_session(&token);
        let auth = timeout(API_TIMEOUT, session.auth_test())
            .await
            .context("auth.test timed out")??;
        let bot_id = auth.bot_id.context("api_token must identify a Slack bot")?;
        let bot = timeout(
            API_TIMEOUT,
            session.bots_info(&SlackApiBotsInfoRequest::new().with_bot(bot_id.0)),
        )
        .await
        .context("bots.info timed out")??;
        let mut cache = ExpiringCache::new(CACHE_CAPACITY);
        timeout(Duration::from_secs(30), async {
            let request = SlackApiConversationsListRequest::new()
                .with_types(vec![
                    SlackConversationType::Public,
                    SlackConversationType::Private,
                ])
                .with_exclude_archived(true)
                .with_limit(200);
            // scroll() fetches just one page. Use the stateful scroller and
            // retain only the bounded cache while traversing all cursors.
            let mut scroller = request.scroller();
            while scroller.has_next() {
                for channel in scroller.next_mut(&session).await?.channels {
                    if let Some(name) = channel.name_normalized.as_deref().and_then(non_empty) {
                        cache.insert(
                            channel.id.0,
                            log_text(name, 256),
                            Instant::now(),
                            CHANNEL_TTL,
                        );
                    }
                }
            }
            Ok::<(), SlackClientError>(())
        })
        .await
        .context("channel discovery timed out")??;
        info!(
            "WS {} authenticated as team {} / app {}",
            config.name, auth.team_id.0, bot.bot.app_id
        );
        Ok(Self {
            name: config.name.clone(),
            team_id: auth.team_id,
            app_id: bot.bot.app_id.into(),
            client,
            token,
            users: Mutex::new(ExpiringCache::new(CACHE_CAPACITY)),
            channels: Mutex::new(cache),
            user_retry_at: Mutex::new(None),
            channel_retry_at: Mutex::new(None),
            health: Health::default(),
            #[cfg(feature = "user-session")]
            session_api: None,
        })
    }
}

#[derive(Debug)]
struct Ingress {
    accepting: AtomicBool,
    seen: SeenEvents,
    counters: Arc<Counters>,
}

impl Ingress {
    fn enqueue(
        &self,
        tx: &mpsc::Sender<QueuedMessage>,
        event_id: String,
        message: QueuedMessage,
    ) -> Result<(), RetryEventError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(RetryEventError);
        }
        // Make duplicate detection and queue admission atomic across the SDK's
        // simultaneous Socket Mode connections. No network or await under lock.
        let mut seen = lock(&self.seen);
        let now = Instant::now();
        let key = (
            message.workspace.team_id.clone(),
            message.workspace.app_id.clone(),
            event_id,
        );
        if seen.get(&key, now).is_some() {
            self.counters.duplicates.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        match tx.try_send(message) {
            Ok(()) => seen.insert(key, (), now, DEDUP_TTL),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.counters.full.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(RetryEventError),
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct BotState {
    workspace: Arc<Workspace>,
    ingress: Arc<Ingress>,
    tx: mpsc::Sender<QueuedMessage>,
    dry_run: bool,
}

#[derive(Debug)]
struct RetryEventError;

impl std::fmt::Display for RetryEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Slack message worker unavailable")
    }
}
impl std::error::Error for RetryEventError {}

impl Bot {
    pub(super) async fn run_socket_sessions(
        &self,
        counters: Arc<Counters>,
        shutdown: impl Future<Output = anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        let db = start_db(&self.url_log_db).await?;
        let regex = self.url_re.as_ref().context("url_regex not initialized")?;
        let seen = Arc::new(Mutex::new(ExpiringCache::new(8192)));
        tokio::pin!(shutdown);
        let mut backoff = 2;
        loop {
            let (tx, rx) = mpsc::channel(MESSAGE_QUEUE_BOUND);
            let ingress = Arc::new(Ingress {
                accepting: AtomicBool::new(true),
                seen: seen.clone(),
                counters: counters.clone(),
            });
            let (drain_tx, drain_rx) = watch::channel(false);
            let mut worker = Box::pin(handle_messages(rx, regex, &db, &counters, drain_rx));
            let mut listeners = Vec::new();
            let mut registered_since = None;
            let mut worker_finished = false;
            let mut shutdown_result = Ok(());
            let result = tokio::select! {
                biased;
                result = &mut shutdown => { shutdown_result = result; None },
                result = self.connect_and_watch(&mut listeners, ingress.clone(), tx, &mut registered_since) => Some(result),
                result = &mut worker => { worker_finished = true; Some(result.and_then(|()| Err(anyhow!("message worker stopped unexpectedly")))) },
            };
            ingress.accepting.store(false, Ordering::Release);
            // Keep the worker future alive during cleanup so its in-flight job
            // can finish. No application task survives this session's scope.
            let _ = drain_tx.send(true);
            for listener in &listeners {
                if timeout(SHUTDOWN_TIMEOUT, listener.shutdown())
                    .await
                    .is_err()
                {
                    error!("Slack listener shutdown timed out");
                }
            }
            if !worker_finished {
                match timeout(DRAIN_TIMEOUT, &mut worker).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => error!("Message worker stopped: {error:#}"),
                    Err(_) => error!("Message queue drain timed out; remaining work discarded"),
                }
            }
            drop(worker);
            drop(listeners);
            let Some(result) = result else {
                return shutdown_result;
            };
            if registered_since
                .is_some_and(|since: Instant| since.elapsed() >= Duration::from_secs(60))
            {
                backoff = 2;
            }
            let error = result
                .err()
                .unwrap_or_else(|| anyhow!("Slack session stopped unexpectedly"));
            error!("Slack session ended: {error:#}");
            let delay = reconnect_delay(backoff, &error);
            warn!("Retrying Slack startup in {} seconds", delay.as_secs_f32());
            tokio::select! { _ = sleep(delay) => {}, result = &mut shutdown => { result?; return Ok(()); } }
            backoff = (backoff * 2).min(30);
        }
    }

    async fn connect_and_watch(
        &self,
        listeners: &mut Vec<SocketListener>,
        ingress: Arc<Ingress>,
        tx: mpsc::Sender<QueuedMessage>,
        registered_since: &mut Option<Instant>,
    ) -> anyhow::Result<()> {
        let mut workspaces = Vec::new();
        let mut identities = HashSet::new();
        for config in &self.workspaces {
            let workspace = Arc::new(
                Workspace::connect(config)
                    .await
                    .with_context(|| format!("WS {} startup failed", config.name))?,
            );
            ensure!(
                identities.insert((workspace.team_id.clone(), workspace.app_id.clone())),
                "duplicate Slack installation"
            );
            let environment = Arc::new(
                SlackClientEventsListenerEnvironment::new(workspace.client.clone())
                    .with_user_state(BotState {
                        workspace: workspace.clone(),
                        ingress: ingress.clone(),
                        tx: tx.clone(),
                        dry_run: self.dry_run,
                    })
                    .with_error_handler(handler_error),
            );
            let listener = SlackClientSocketModeListener::new(
                &SlackClientSocketModeConfig::new(),
                environment,
                SlackSocketModeListenerCallbacks::new()
                    .with_hello_events(|event, _client, state| async move {
                        handle_hello(&event.connection_info.app_id, &state);
                    })
                    .with_interaction_events(handler_interaction_events)
                    .with_push_events(handler_push_events),
            );
            // Retain ownership before any await that could fail or be cancelled.
            listeners.push(listener);
            let listener = listeners.last().context("missing Slack listener")?;
            timeout(
                API_TIMEOUT,
                listener.listen_for(&SlackApiToken::new(config.socket_token.clone().into())),
            )
            .await
            .context("Socket Mode token registration timed out")??;
            timeout(API_TIMEOUT, listener.start())
                .await
                .context("Socket Mode start timed out")?;
            timeout(HELLO_TIMEOUT, workspace.health.wait_ready())
                .await
                .with_context(|| {
                    format!(
                        "WS {} did not receive a Socket Mode hello within 30 seconds",
                        workspace.name
                    )
                })??;
            workspaces.push(workspace);
        }
        *registered_since = Some(Instant::now());
        info!("All Slack workspaces are connected");
        // The SDK owns ordinary transport reconnects; identity/auth failures
        // restart the application session after explicit listener cleanup.
        futures::future::try_join_all(
            workspaces
                .iter()
                .map(|workspace| workspace.health.wait_failure()),
        )
        .await?;
        bail!("Slack session monitors stopped unexpectedly")
    }
}

fn reconnect_delay(backoff: u64, error: &anyhow::Error) -> Duration {
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| u64::from(time.subsec_millis()));
    let delay = Duration::from_millis(backoff * 1000 + jitter);
    match error.downcast_ref::<SlackClientError>() {
        Some(SlackClientError::RateLimitError(rate)) => {
            delay.max(rate.retry_after.unwrap_or(delay))
        }
        _ => delay,
    }
}

fn callback_state(state: &SlackClientEventsUserState) -> Result<BotState, RetryEventError> {
    state
        .try_read()
        .ok()
        .and_then(|state| state.get_user_state::<BotState>().cloned())
        .ok_or(RetryEventError)
}

fn handler_workspace_name(state: SlackClientEventsUserState) -> String {
    callback_state(&state)
        .map(|state| state.workspace.name.clone())
        .unwrap_or_else(|_| "unknown".into())
}

fn handler_error(
    error: Box<dyn std::error::Error + Send + Sync>,
    _client: Arc<SlackHyperClient>,
    state: SlackClientEventsUserState,
) -> http::StatusCode {
    let name = handler_workspace_name(state.clone());
    if let Some(SlackClientError::SocketModeProtocolError(protocol)) =
        error.downcast_ref::<SlackClientError>()
    {
        warn!("WS {name} Slack Socket Mode protocol error: {protocol:?}");
    } else {
        error!("WS {name} Slack Socket Mode error: {error:?}");
    }
    if let Some(SlackClientError::ApiError(api)) = error.downcast_ref::<SlackClientError>()
        && matches!(
            api.code.as_str(),
            "invalid_auth"
                | "not_authed"
                | "token_revoked"
                | "token_expired"
                | "account_inactive"
                | "missing_scope"
        )
        && let Ok(state) = callback_state(&state)
    {
        state
            .workspace
            .health
            .fail(format!("WS {name} authentication failed: {}", api.code));
    }
    // Overload is acknowledged as best-effort loss. A closed/unavailable worker
    // requests redelivery rather than claiming successful admission.
    if error.is::<RetryEventError>() {
        http::StatusCode::SERVICE_UNAVAILABLE
    } else {
        http::StatusCode::OK
    }
}

fn handle_hello(app_id: &SlackAppId, state: &SlackClientEventsUserState) {
    if let Ok(state) = callback_state(state) {
        if *app_id != state.workspace.app_id {
            state.workspace.health.fail(format!(
                "WS {} socket token belongs to a different Slack app",
                state.workspace.name
            ));
        } else {
            state.workspace.health.ready.store(true, Ordering::Release);
            state.workspace.health.changed.notify_one();
        }
    }
}

async fn handler_interaction_events(
    event: SlackInteractionEvent,
    _client: Arc<SlackHyperClient>,
    _state: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("Ignoring unused Slack interaction: {event:?}");
    Ok(())
}

async fn handler_push_events(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    state: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = callback_state(&state)?;
    if event.team_id != state.workspace.team_id || event.api_app_id != state.workspace.app_id {
        state.workspace.health.fail(format!(
            "WS {} received an event for a different Slack installation",
            state.workspace.name
        ));
        return Ok(());
    }
    if let SlackEventCallbackBody::Message(message) = event.event
        && should_process_message(&message)
    {
        let Some(mut queued) =
            prepare_message(state.workspace.clone(), message, &state.ingress.counters)
        else {
            return Ok(());
        };
        if event.event_id.0.len() > 256 || event.event_id.0.is_empty() {
            state
                .ingress
                .counters
                .invalid
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        queued.dry_run = state.dry_run;
        state.ingress.enqueue(&state.tx, event.event_id.0, queued)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        task::JoinHandle,
    };

    struct FakeSlack {
        client: Arc<SlackHyperClient>,
        requests: Arc<Mutex<Vec<String>>>,
        task: JoinHandle<()>,
    }

    impl Drop for FakeSlack {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeSlack {
        async fn new(responses: Vec<(u16, &'static str, serde_json::Value)>) -> Self {
            crate::config::install_crypto_provider().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let saved_requests = requests.clone();
            let task = tokio::spawn(async move {
                for (status, headers, body) in responses {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut socket = BufReader::new(socket);
                    let mut request = String::new();
                    socket.read_line(&mut request).await.unwrap();
                    lock(&saved_requests).push(request);
                    loop {
                        let mut line = String::new();
                        if socket.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                        body.len()
                    );
                    socket
                        .get_mut()
                        .write_all(response.as_bytes())
                        .await
                        .unwrap();
                }
            });
            let connector = hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .unwrap()
                .https_or_http()
                .enable_http1()
                .build();
            let connector = SlackClientHyperConnector::with_connector(connector)
                .with_slack_api_url(&format!("http://{address}/api"));
            Self {
                client: Arc::new(SlackClient::new(connector)),
                requests,
                task,
            }
        }
    }

    fn config() -> serde_json::Value {
        serde_json::json!({"url_regex": "<(https?://[^>]+)>", "url_log_db": "postgres:///url?sslmode=disable",
            "workspaces": [{"name": "test", "api_token": "xoxb-test", "socket_token": "xapp-test"}]})
    }

    fn workspace() -> Arc<Workspace> {
        crate::config::install_crypto_provider().unwrap();
        // Tests use the real SDK with a local fake endpoint; no Slack requests.
        let connector = SlackClientHyperConnector::new()
            .unwrap()
            .with_slack_api_url("http://127.0.0.1:1/api");
        Arc::new(Workspace {
            name: "test".into(),
            team_id: "T1".into(),
            app_id: "A1".into(),
            client: Arc::new(SlackClient::new(connector)),
            token: SlackApiToken::new("test".into()),
            users: Mutex::new(ExpiringCache::new(2)),
            channels: Mutex::new(ExpiringCache::new(2)),
            user_retry_at: Mutex::new(None),
            channel_retry_at: Mutex::new(None),
            health: Health::default(),
            #[cfg(feature = "user-session")]
            session_api: None,
        })
    }

    fn message() -> SlackMessageEvent {
        serde_json::from_value(
            serde_json::json!({"type":"message", "ts":"1234.567890", "channel":"C1",
            "user":"U1", "username":"sender", "text":"<https://example.com/>"}),
        )
        .unwrap()
    }

    fn ingress() -> Ingress {
        Ingress {
            accepting: AtomicBool::new(true),
            seen: Arc::new(Mutex::new(ExpiringCache::new(2))),
            counters: Arc::new(Counters::default()),
        }
    }

    fn state(
        workspace: Arc<Workspace>,
        tx: mpsc::Sender<QueuedMessage>,
    ) -> SlackClientEventsUserState {
        SlackClientEventsListenerEnvironment::new(workspace.client.clone())
            .with_user_state(BotState {
                workspace,
                ingress: Arc::new(ingress()),
                tx,
                dry_run: false,
            })
            .user_state
    }

    #[test]
    fn config_validation_runs_without_network() {
        let mut value = config();
        assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_ok());
        for field in ["name", "api_token", "socket_token"] {
            value = config();
            value["workspaces"][0][field] = " ".into();
            assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        value = config();
        value["workspaces"] = serde_json::json!([]);
        assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
        value = config();
        value["url_regex"] = "https?://.+".into();
        assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
        value = config();
        value["url_log_db"] = "invalid".into();
        assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
        value = config();
        let duplicate = value["workspaces"][0].clone();
        value["workspaces"].as_array_mut().unwrap().push(duplicate);
        assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
        value["workspaces"][1]["name"] = "other".into();
        assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(Bot::from_config(&vec![b' '; CONFIG_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn supported_subtypes_and_hidden_messages() {
        let mut msg = message();
        for subtype in [
            None,
            Some(SlackMessageEventType::BotMessage),
            Some(SlackMessageEventType::FileShare),
            Some(SlackMessageEventType::MeMessage),
            Some(SlackMessageEventType::ThreadBroadcast),
        ] {
            msg.subtype = subtype;
            assert!(should_process_message(&msg));
        }
        msg.hidden = Some(true);
        assert!(!should_process_message(&msg));
        msg.hidden = None;
        for subtype in [
            SlackMessageEventType::MessageChanged,
            SlackMessageEventType::MessageDeleted,
        ] {
            msg.subtype = Some(subtype);
            assert!(!should_process_message(&msg));
        }
    }

    #[test]
    fn sender_names_preserve_profile_precedence_and_skip_blank_names() {
        let profile: SlackUserProfile = serde_json::from_value(
            serde_json::json!({"display_name":"display", "real_name":"Real"}),
        )
        .unwrap();
        assert_eq!(
            slack_user_profile_nick(&profile).as_deref(),
            Some("display")
        );
        let user: SlackUser = serde_json::from_value(serde_json::json!({"id":"U1", "name":"legacy", "profile":{"display_name":" ","real_name":"Real"}})).unwrap();
        assert_eq!(slack_user_nick(&user).as_deref(), Some("Real"));
    }

    #[tokio::test]
    async fn admission_deduplicates_after_enqueue_and_never_waits_for_space() {
        let ingress = ingress();
        let workspace = workspace();
        let (tx, mut rx) = mpsc::channel(1);
        let queued = || prepare_message(workspace.clone(), message(), &ingress.counters).unwrap();
        ingress.enqueue(&tx, "E1".into(), queued()).unwrap();
        ingress.enqueue(&tx, "E1".into(), queued()).unwrap();
        ingress.enqueue(&tx, "E2".into(), queued()).unwrap();
        assert_eq!(ingress.counters.duplicates.load(Ordering::Relaxed), 1);
        assert_eq!(ingress.counters.full.load(Ordering::Relaxed), 1);
        assert_eq!(rx.recv().await.unwrap().ts, 1234);
        ingress.enqueue(&tx, "E2".into(), queued()).unwrap();
        assert!(rx.try_recv().is_ok());
        drop(rx);
        assert!(ingress.enqueue(&tx, "E3".into(), queued()).is_err());
        ingress.accepting.store(false, Ordering::Release);
        assert!(ingress.enqueue(&tx, "E1".into(), queued()).is_err());
    }

    #[tokio::test]
    async fn callback_does_no_network_work_and_checks_installation_identity() {
        let workspace = workspace();
        let (tx, mut rx) = mpsc::channel(1);
        let state = state(workspace.clone(), tx);
        let mut event: SlackPushEventCallback = serde_json::from_value(serde_json::json!({
            "team_id":"T1", "api_app_id":"A1", "event_id":"E1", "event_time":1234,
            "event":{"type":"message","ts":"1234.5","channel":"C1","user":"U1","text":"hello"}}))
        .unwrap();
        timeout(
            Duration::from_secs(1),
            handler_push_events(event.clone(), workspace.client.clone(), state.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(rx.try_recv().unwrap().ts, 1234);
        event.team_id = "T2".into();
        handler_push_events(event, workspace.client.clone(), state)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
        assert!(workspace.health.check().is_err());
    }

    #[tokio::test]
    async fn hello_validates_app_and_wakes_startup_and_failure_monitors() {
        let workspace = workspace();
        let (tx, _rx) = mpsc::channel(1);
        let state = state(workspace.clone(), tx);
        handle_hello(&"A1".into(), &state);
        timeout(Duration::from_secs(1), workspace.health.wait_ready())
            .await
            .unwrap()
            .unwrap();
        handle_hello(&"A2".into(), &state);
        assert!(
            timeout(Duration::from_secs(1), workspace.health.wait_failure())
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn error_handler_never_waits_on_contended_state_and_requests_retry_for_closed_worker() {
        let workspace = workspace();
        let (tx, _rx) = mpsc::channel(1);
        let state = state(workspace.clone(), tx);
        let _guard = state.write().await;
        assert_eq!(handler_workspace_name(state.clone()), "unknown");
        assert_eq!(
            handler_error(
                Box::new(RetryEventError),
                workspace.client.clone(),
                state.clone()
            ),
            http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn expired_messages_skip_io_and_drain_closes_retained_senders() {
        let counters = Counters::default();
        let workspace = workspace();
        let mut queued = prepare_message(workspace, message(), &counters).unwrap();
        queued.expires_at = Instant::now();
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(queued).unwrap();
        let (drain_tx, drain_rx) = watch::channel(false);
        drain_tx.send(true).unwrap();
        let db = start_db("postgres://localhost:1/url?sslmode=disable")
            .await
            .unwrap();
        let regex = Regex::new("(https?://.+)").unwrap();
        timeout(
            Duration::from_secs(1),
            handle_messages(rx, &regex, &db, &counters, drain_rx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(tx.is_closed());
        assert_eq!(counters.expired.load(Ordering::Relaxed), 1);
        assert_eq!(db.dbc.size(), 0);
    }

    #[tokio::test]
    async fn negative_caches_expire_and_unknown_channels_keep_workspace_and_id() {
        let workspace = workspace();
        *lock(&workspace.channel_retry_at) = Some(Instant::now() + Duration::from_secs(60));
        assert_eq!(workspace.channel_name("C123").await, "test-C123");
        let old = Instant::now() - NEGATIVE_TTL;
        lock(&workspace.users).insert("U1".into(), "fallback".into(), old, NEGATIVE_TTL);
        assert!(
            lock(&workspace.users)
                .get(&"U1".into(), Instant::now())
                .is_none()
        );
        let sender = SenderInfo {
            user: Some("U1".into()),
            hint: Some("fresh".into()),
            bot: None,
        };
        assert_eq!(workspace.sender_nick(&sender).await, "fresh");
        assert_eq!(
            lock(&workspace.users)
                .get(&"U1".into(), Instant::now())
                .as_deref(),
            Some("fresh")
        );
    }

    #[tokio::test]
    async fn oversize_messages_and_bad_timestamps_are_dropped_and_missing_content_is_ignored() {
        let counters = Counters::default();
        let workspace = workspace();
        let mut msg = message();
        msg.content.as_mut().unwrap().text = Some("x".repeat(MESSAGE_BYTES + 1));
        assert!(prepare_message(workspace.clone(), msg, &counters).is_none());
        assert_eq!(counters.oversized_messages.load(Ordering::Relaxed), 1);
        let mut msg = message();
        msg.origin.ts = "bad".into();
        assert!(prepare_message(workspace.clone(), msg, &counters).is_none());
        let mut msg = message();
        msg.content = None;
        assert!(prepare_message(workspace, msg, &counters).is_none());
    }

    #[tokio::test]
    async fn shutdown_cancels_startup_without_waiting_for_slack() {
        let bot = Bot::from_config(&serde_json::to_vec(&config()).unwrap()).unwrap();
        timeout(
            Duration::from_secs(1),
            bot.run_sessions(Arc::new(Counters::default()), async { Ok(()) }),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn discovery_authenticates_and_paginates_public_and_private_channels() {
        let server = FakeSlack::new(vec![
            (200, "", serde_json::json!({"ok":true, "team_id":"T1", "user_id":"U1", "bot_id":"B1", "team":"test", "url":"https://test.slack.com/"})),
            (200, "", serde_json::json!({"ok":true, "bot":{"id":"B1", "name":"bot", "app_id":"A1"}})),
            (200, "", serde_json::json!({"ok":true, "channels":[{"id":"C1","created":1234,"name_normalized":"general"}], "response_metadata":{"next_cursor":"next"}})),
            (200, "", serde_json::json!({"ok":true, "channels":[{"id":"G1","created":1234,"name_normalized":"private"}], "response_metadata":{"next_cursor":""}})),
        ]).await;
        let config = Bot::from_config(&serde_json::to_vec(&config()).unwrap()).unwrap();
        let workspace = timeout(
            Duration::from_secs(2),
            Workspace::connect_with_client(&config.workspaces[0], server.client.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(workspace.team_id.0, "T1");
        assert_eq!(workspace.app_id.0, "A1");
        assert_eq!(workspace.channel_name("G1").await, "test-private");
        let requests = lock(&server.requests);
        assert_eq!(requests.len(), 4);
        assert!(requests[0].contains("/auth.test"));
        assert!(requests[2].contains("types=public_channel%2Cprivate_channel"));
        assert!(requests[3].contains("cursor=next"));
    }

    #[tokio::test]
    async fn user_rate_limits_cool_down_all_uncached_lookups_and_recover() {
        let server = FakeSlack::new(vec![
            (
                429,
                "Retry-After: 60\r\n",
                serde_json::json!({"ok":false,"error":"ratelimited"}),
            ),
            (
                200,
                "",
                serde_json::json!({"ok":true,"user":{"id":"U2","name":"resolved"}}),
            ),
        ])
        .await;
        let mut workspace = Arc::try_unwrap(workspace()).unwrap();
        workspace.client = server.client.clone();
        let sender = |id: &str| SenderInfo {
            user: Some(id.into()),
            hint: None,
            bot: None,
        };
        assert_eq!(workspace.sender_nick(&sender("U1")).await, "<@U1>");
        assert_eq!(workspace.sender_nick(&sender("U2")).await, "<@U2>");
        assert_eq!(lock(&server.requests).len(), 1);
        // Simulate expiry without wall-clock sleeps.
        *lock(&workspace.user_retry_at) = None;
        lock(&workspace.users).insert(
            "U2".into(),
            "<@U2>".into(),
            Instant::now() - NEGATIVE_TTL,
            NEGATIVE_TTL,
        );
        assert_eq!(workspace.sender_nick(&sender("U2")).await, "resolved");
        assert_eq!(lock(&server.requests).len(), 2);
    }
}
