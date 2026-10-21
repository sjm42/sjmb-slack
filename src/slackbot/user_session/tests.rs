use super::*;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

struct Fixture {
    session: Arc<Session>,
    rx: mpsc::Receiver<QueuedMessage>,
    requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
    dir: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn fixture(responses: Vec<Value>) -> Fixture {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let saved = requests.clone();
    let server = tokio::spawn(async move {
        for response in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line.to_ascii_lowercase().starts_with("content-length:") {
                    length = line.split(':').nth(1).unwrap().trim().parse().unwrap();
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            request.push_str(&String::from_utf8(body).unwrap());
            lock(&saved).push(request);
            let body = response.to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
        }
    });
    let config = RecoveryConfig {
        state_dir: format!(
            "/tmp/sjmb-session-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ),
        ..Default::default()
    };
    let api = SessionApi::test_endpoint(address);
    let store = Arc::new(Store::open(&config, "T1", "U1").unwrap());
    store.begin_connection(&["C1".into()], now() - 60).unwrap();
    let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new().unwrap()));
    let workspace = Arc::new(Workspace {
        name: "test".into(),
        url_filter: UrlFilter::default(),
        team_id: "T1".into(),
        #[cfg(feature = "socket-mode")]
        app_id: "A1".into(),
        client,
        token: SlackApiToken::new("xoxc-test".into()),
        users: Mutex::new(ExpiringCache::new(2)),
        channels: Mutex::new(ExpiringCache::new(2)),
        user_retry_at: Mutex::new(None),
        channel_retry_at: Mutex::new(None),
        #[cfg(feature = "socket-mode")]
        health: socket_mode::Health::default(),
        session_api: Some(api.clone()),
    });
    let (tx, rx) = mpsc::channel(8);
    let dir = config.state_dir.clone();
    let session = Arc::new(Session {
        workspace,
        api,
        store,
        config,
        members: Mutex::new(HashSet::from(["C1".into()])),
        processing: tokio::sync::Mutex::new(()),
        reconnect_url: Mutex::new(None),
        tx,
        counters: Arc::new(Counters::default()),
        dry_run: false,
        token: "xoxc-test".into(),
    });
    Fixture {
        session,
        rx,
        requests,
        server,
        dir,
    }
}

fn message(ts: &str) -> Value {
    json!({"type":"message","channel":"C1","ts":ts,"user":"U1","username":"sender","text":"<https://example.com/>"})
}

#[test]
fn config_rejects_missing_credentials_and_unavailable_backends() {
    let value = json!({"backend":"user-session","url_regex":"<(https?://[^>]+)>","url_log_db":"postgres:///url?sslmode=disable","workspaces":[{"name":"test","api_token":"xoxc-test","cookie":"d=xoxd-test%2F; d-s=extra"}]});
    assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_ok());
    for cookie in ["", "d=", "other=value", "d=x\r\nX-Test: y"] {
        let mut value = value.clone();
        value["workspaces"][0]["cookie"] = cookie.into();
        assert!(Bot::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let mut bad = value.clone();
    bad["recovery"] = json!({"window_seconds":0});
    assert!(Bot::from_config(&serde_json::to_vec(&bad).unwrap()).is_err());
    if !cfg!(feature = "socket-mode") {
        let mut bad = value;
        bad["backend"] = "socket-mode".into();
        assert!(Bot::from_config(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
}

#[test]
fn gateway_urls_encode_tokens_and_reject_cookie_exfiltration_destinations() {
    let url = validate_socket_url(&gateway_url("xoxc-a&evil=1", "T1")).unwrap();
    assert_eq!(
        url.query_pairs().find(|(key, _)| key == "token").unwrap().1,
        "xoxc-a&evil=1"
    );
    for url in [
        "ws://wss-primary.slack.com/",
        "wss://slack.com.evil.test/",
        "wss://evilslack.com/",
        "wss://user@wss-primary.slack.com/",
        "wss://wss-primary.slack.com:444/",
        "wss://wss-primary.slack.com/#fragment",
    ] {
        assert!(validate_socket_url(url).is_err(), "{url}");
    }
}

#[test]
fn filters_dms_mpims_edits_and_preserves_full_timestamp_and_thread() {
    let counters = Counters::default();
    let mut event = message("1234.000001");
    event["thread_ts"] = "1230.000001".into();
    let (_, ts, thread) = normalise(event.clone(), &counters).unwrap();
    assert_eq!(ts, "1234.000001");
    assert_eq!(thread.as_deref(), Some("1230.000001"));
    for subtype in [
        "message_changed",
        "message_deleted",
        "unknown_future_subtype",
    ] {
        event["subtype"] = subtype.into();
        assert!(normalise(event.clone(), &counters).is_none());
    }
    assert!(!eligible_channel(&json!({"id":"D1","is_im":true})));
    assert!(!eligible_channel(&json!({"id":"G1","is_mpim":true})));
    assert!(eligible_channel(&json!({"id":"G1","is_private":true})));
}

#[tokio::test]
async fn only_successful_worker_receipts_are_deduplicated() {
    let mut f = fixture(vec![]).await;
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.process(message("1234.000001")).await });
    let mut job = f.rx.recv().await.unwrap();
    assert!(!f.session.store.seen("C1", "1234.000001"));
    job.completion.take().unwrap().send(false).unwrap();
    assert!(task.await.unwrap().is_err());
    assert!(!f.session.store.seen("C1", "1234.000001"));
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.process(message("1234.000001")).await });
    f.rx.recv()
        .await
        .unwrap()
        .completion
        .take()
        .unwrap()
        .send(true)
        .unwrap();
    task.await.unwrap().unwrap();
    f.session.process(message("1234.000001")).await.unwrap();
    assert!(f.rx.try_recv().is_err());
    assert_eq!(f.session.counters.duplicates.load(Ordering::Relaxed), 1);
    // Fractional timestamps distinguish different posts in the same second.
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.process(message("1234.000002")).await });
    f.rx.recv()
        .await
        .unwrap()
        .completion
        .take()
        .unwrap()
        .send(true)
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn recovery_paginates_history_and_fetches_replies_without_marking_read() {
    let at = now();
    let parent = format!("{}.000001", at - 10);
    let reply = format!("{}.000002", at - 5);
    let mut root = message(&parent);
    root["reply_count"] = 1.into();
    let mut response_reply = message(&reply);
    response_reply["thread_ts"] = parent.clone().into();
    let mut f = fixture(vec![
        json!({"ok":true,"messages":[root],"has_more":true,"response_metadata":{"next_cursor":"page2"}}),
        json!({"ok":true,"messages":[],"response_metadata":{"next_cursor":""}}),
        json!({"ok":true,"messages":[message(&parent),response_reply]}),
    ]).await;
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.recover_channel("C1", at - 60, at).await });
    for _ in 0..2 {
        f.rx.recv()
            .await
            .unwrap()
            .completion
            .take()
            .unwrap()
            .send(true)
            .unwrap();
    }
    task.await.unwrap().unwrap();
    let requests = lock(&f.requests);
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("cursor=page2"));
    assert!(requests[2].contains("conversations.replies"));
    assert!(requests[2].contains(&format!("ts={parent}")));
    assert!(!requests.iter().any(|request| request.contains(".mark")));
}

#[tokio::test]
async fn websocket_receives_messages_and_heartbeat_barriers_without_read_updates() {
    let f = fixture(vec![]).await;
    let (client, server) = tokio::io::duplex(8192);
    let client = tokio_tungstenite::WebSocketStream::from_raw_socket(
        client,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    let mut server = tokio_tungstenite::WebSocketStream::from_raw_socket(
        server,
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;
    let (tx, mut rx) = mpsc::channel(8);
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.read_socket(client, tx).await });
    let ping = timeout(Duration::from_secs(1), server.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ping: Value = serde_json::from_str(ping.to_text().unwrap()).unwrap();
    assert_eq!(ping["type"], "ping");
    server
        .send(Message::Text(message("1234.000001").to_string().into()))
        .await
        .unwrap();
    server
        .send(Message::Text(
            json!({"type":"pong","reply_to":ping["id"]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert!(matches!(rx.recv().await, Some(Event::Message(_))));
    assert!(matches!(rx.recv().await, Some(Event::Checkpoint(_))));
    server.close(None).await.unwrap();
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn failed_recovery_keeps_checkpoint_frozen() {
    let at = now();
    let mut f = fixture(vec![
        json!({"ok":true,"messages":[message(&format!("{}.000001",at-5))]}),
    ])
    .await;
    f.session
        .store
        .begin_connection(&["C1".into()], at)
        .unwrap();
    let before = f.session.store.recovery_channels()[0].1.covered_until;
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.recover_channel("C1", at - 60, at).await });
    f.rx.recv()
        .await
        .unwrap()
        .completion
        .take()
        .unwrap()
        .send(false)
        .unwrap();
    assert!(task.await.unwrap().is_err());
    f.session.store.finish_recovery("C1", at);
    f.session.store.checkpoint(at + 10);
    assert_eq!(
        f.session.store.recovery_channels()[0].1.covered_until,
        before
    );
}

#[tokio::test]
async fn dry_run_logs_urls_without_connecting_to_database() {
    let f = fixture(vec![]).await;
    lock(&f.session.workspace.channels).insert(
        "C1".into(),
        "general".into(),
        Instant::now(),
        CHANNEL_TTL,
    );
    let mut event = message("1234.000001");
    // Exercise the allowed path without depending on external DNS.
    event["text"] = json!("<https://8.8.8.8/>");
    let (message, _, _) = normalise(event, &f.session.counters).unwrap();
    let mut queued =
        prepare_message(f.session.workspace.clone(), message, &f.session.counters).unwrap();
    queued.dry_run = true;
    let db = start_db("postgres://localhost:1/url?sslmode=disable")
        .await
        .unwrap();
    handle_msg(
        queued,
        &Regex::new("<(https?://[^>]+)>").unwrap(),
        &db,
        &f.session.counters,
    )
    .await
    .unwrap();
    assert_eq!(db.dbc.size(), 0);
}

#[tokio::test]
async fn queue_overflow_requests_reconnect_and_counts_loss() {
    let f = fixture(vec![]).await;
    let (tx, _rx) = mpsc::channel(1);
    f.session
        .enqueue_event(&tx, Event::Message(message("1234.000001")))
        .unwrap();
    assert!(
        f.session
            .enqueue_event(&tx, Event::Checkpoint(now()))
            .is_err()
    );
    assert_eq!(f.session.counters.full.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn excluded_history_urls_complete_recovery_without_database_access() {
    let at = now();
    let ts = format!("{}.000001", at - 10);
    let mut f = fixture(vec![json!({"ok":true, "messages":[message(&ts)]})]).await;
    let session = Arc::get_mut(&mut f.session).unwrap();
    Arc::get_mut(&mut session.workspace).unwrap().url_filter =
        UrlFilter::compile(&["example.com".into()], "test").unwrap();
    lock(&session.workspace.channels).insert(
        "C1".into(),
        "general".into(),
        Instant::now(),
        CHANNEL_TTL,
    );
    let rx = std::mem::replace(&mut f.rx, mpsc::channel(1).1);
    let (drain_tx, drain_rx) = watch::channel(false);
    let db = start_db("postgres://localhost:1/url?sslmode=disable")
        .await
        .unwrap();
    // A closed pool makes any accidental database call fail immediately.
    db.dbc.close().await;
    let regex = Regex::new("<(https?://[^>]+)>").unwrap();
    let worker = handle_messages(rx, &regex, &db, &f.session.counters, drain_rx);
    let recovery = async {
        let result = f.session.recover_channel("C1", at - 60, at).await;
        drain_tx.send(true).unwrap();
        result
    };
    let (worker, recovery) = tokio::join!(worker, recovery);
    worker.unwrap();
    recovery.unwrap();
    assert!(f.session.store.seen("C1", &ts));
    assert_eq!(db.dbc.size(), 0);
}

#[tokio::test]
async fn excluding_one_url_does_not_skip_allowed_urls_in_the_same_message() {
    let mut f = fixture(vec![]).await;
    let session = Arc::get_mut(&mut f.session).unwrap();
    Arc::get_mut(&mut session.workspace).unwrap().url_filter =
        UrlFilter::compile(&["example.com".into()], "test").unwrap();
    lock(&session.workspace.channels).insert(
        "C1".into(),
        "general".into(),
        Instant::now(),
        CHANNEL_TTL,
    );
    let db = start_db("postgres://localhost:1/url?sslmode=disable")
        .await
        .unwrap();
    db.dbc.close().await;
    let regex = Regex::new("<(https?://[^>]+)>").unwrap();
    for (text, succeeds) in [
        ("<https://example.com/> <http://127.0.0.1/>", true),
        (
            "<https://example.com/> <http://127.0.0.1/> <https://8.8.8.8/>",
            false,
        ),
    ] {
        let mut event = message("1234.000001");
        event["text"] = json!(text);
        let (message, _, _) = normalise(event, &f.session.counters).unwrap();
        let queued =
            prepare_message(f.session.workspace.clone(), message, &f.session.counters).unwrap();
        // Only the allowed URL should attempt an insert into the closed pool.
        assert_eq!(
            handle_msg(queued, &regex, &db, &f.session.counters)
                .await
                .is_ok(),
            succeeds
        );
    }
}

#[tokio::test(start_paused = true)]
async fn unresponsive_websocket_is_disconnected() {
    let f = fixture(vec![]).await;
    let (client, server) = tokio::io::duplex(8192);
    let client = tokio_tungstenite::WebSocketStream::from_raw_socket(
        client,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    let mut server = tokio_tungstenite::WebSocketStream::from_raw_socket(
        server,
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;
    let (tx, _rx) = mpsc::channel(8);
    let session = f.session.clone();
    let task = tokio::spawn(async move { session.read_socket(client, tx).await });
    assert!(server.next().await.unwrap().unwrap().is_text());
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = task.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("heartbeat timed out"));
}

#[tokio::test]
async fn channel_discovery_paginates_and_excludes_group_direct_messages() {
    let f = fixture(vec![
        json!({"ok":true,"channels":[{"id":"C1","name":"public"},{"id":"GDM","is_mpim":true}],"response_metadata":{"next_cursor":"second"}}),
        json!({"ok":true,"channels":[{"id":"G1","name":"private","is_private":true}],"response_metadata":{"next_cursor":""}}),
    ]).await;
    f.session.discover_channels().await.unwrap();
    assert_eq!(
        *lock(&f.session.members),
        HashSet::from(["C1".into(), "G1".into()])
    );
    assert_eq!(f.session.workspace.channel_name("G1").await, "test-private");
    assert!(lock(&f.requests)[1].contains("cursor=second"));
}

#[tokio::test]
async fn page_limit_is_reported_without_claiming_recovery_complete() {
    let mut f = fixture(vec![
        json!({"ok":true,"messages":[],"has_more":true,"response_metadata":{"next_cursor":"more"}}),
    ])
    .await;
    Arc::get_mut(&mut f.session)
        .unwrap()
        .config
        .max_pages_per_channel = 1;
    f.session
        .store
        .begin_connection(&["C1".into()], now())
        .unwrap();
    assert!(
        f.session
            .recover_channel("C1", now() - 60, now())
            .await
            .unwrap_err()
            .to_string()
            .contains("page limit")
    );
    assert_eq!(f.session.store.recovery_channels().len(), 1);
}

#[tokio::test]
async fn deleted_tracked_threads_do_not_block_channel_recovery_forever() {
    let at = now();
    let f = fixture(vec![
        json!({"ok":true,"messages":[]}),
        json!({"ok":false,"error":"thread_not_found"}),
    ])
    .await;
    f.session.store.remember_thread("C1", "1234.000001");
    f.session.recover_channel("C1", at - 60, at).await.unwrap();
    assert!(f.session.store.threads("C1").is_empty());
}

#[tokio::test]
async fn simultaneous_live_and_recovery_messages_share_duplicate_detection() {
    let mut f = fixture(vec![]).await;
    let first = f.session.clone();
    let second = f.session.clone();
    let task = tokio::spawn(async move {
        tokio::try_join!(
            first.process(message("1234.000001")),
            second.process(message("1234.000001"))
        )
    });
    timeout(Duration::from_secs(1), f.rx.recv())
        .await
        .unwrap()
        .unwrap()
        .completion
        .take()
        .unwrap()
        .send(true)
        .unwrap();
    task.await.unwrap().unwrap();
    assert!(f.rx.try_recv().is_err());
    assert_eq!(f.session.counters.duplicates.load(Ordering::Relaxed), 1);
}
