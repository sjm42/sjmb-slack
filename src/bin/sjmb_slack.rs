// bin/sjmb_slack.rs

use anyhow::anyhow;
use log::*;
use slack_morphism::prelude::*;
use std::sync::Arc;
use structopt::StructOpt;

use sjmb_slack::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut opts = OptsCommon::from_args();
    opts.finish()?;
    opts.start_pgm(env!("CARGO_BIN_NAME"));

    info!("Creating client");
    let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()));

    // Need to specify tokens for API and Socket Mode:
    let api_token_value = std::env::var("SLACK_API_TOKEN")?.into();
    let api_token = SlackApiToken::new(api_token_value);

    let sock_token_value = std::env::var("SLACK_SOCKET_TOKEN")?.into();
    let sock_token = SlackApiToken::new(sock_token_value);

    info!("Testing API...");
    let sess = client.open_session(&api_token);
    let ret = sess
        .api_test(&SlackApiTestRequest::new().with_foo("Test".into()))
        .await?;
    info!("Test result: {ret:#?}");

    info!("Getting all channels...");
    let chans = SlackApiConversationsListRequest::new()
        .with_exclude_archived(true)
        .without_limit()
        .scroll(&sess)
        .await?
        .channels;
    // info!("Channels: {chans:#?}");

    let c_id = chans
        .iter()
        .filter(|c| c.name_normalized.is_some())
        .find(|c| c.name_normalized.as_ref().unwrap() == "random")
        .map(|c| c.id.as_ref())
        .ok_or_else(|| anyhow!("Cannot find random"))?;
    info!("Found #random, id: {c_id:#?}");

    // get and display last 10 messages
    let msgs = SlackApiConversationsHistoryRequest::new()
        .with_channel(c_id.into())
        .with_oldest("0".into())
        .with_limit(10)
        .scroll(&sess)
        .await?
        .messages
        .iter()
        .filter(|m| m.content.text.is_some())
        .map(|m| m.content.text.as_ref().unwrap().to_string())
        .collect::<Vec<String>>();
    info!("Got last 10 messages: {msgs:#?}");

    let socket_mode_callbacks = SlackSocketModeListenerCallbacks::new()
        .with_interaction_events(test_interaction_events_function)
        .with_push_events(test_push_events_sm_function);

    let listener_environment = Arc::new(
        SlackClientEventsListenerEnvironment::new(client.clone())
            .with_error_handler(test_error_handler),
    );

    info!("Creating listener");
    let socket_mode_listener = SlackClientSocketModeListener::new(
        &SlackClientSocketModeConfig::new(),
        listener_environment.clone(),
        socket_mode_callbacks,
    );

    info!("Listening for events");
    // Register an app token to listen for events,
    socket_mode_listener.listen_for(&sock_token).await?;

    info!("Serve...");
    // Start WS connections calling Slack API to get WS url for the token,
    // and wait for Ctrl-C to shutdown
    // There are also `.start()`/`.shutdown()` available to manage manually
    socket_mode_listener.serve().await;

    Ok(())
}

fn test_error_handler(
    err: Box<dyn std::error::Error + Send + Sync>,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> http::StatusCode {
    error!("{:#?}", err);

    // This return value should be OK if we want to return successful ack
    // to the Slack server using Web-sockets
    // https://api.slack.com/apis/connections/socket-implement#acknowledge
    // so that Slack knows whether to retry
    http::StatusCode::OK
}

async fn test_interaction_events_function(
    event: SlackInteractionEvent,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("{:#?}", event);
    Ok(())
}

async fn test_push_events_sm_function(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // info!("{:#?}", event);

    if let SlackEventCallbackBody::Message(msg) = event.event {
        if let Some(cont) = msg.content {
            if let Some(text) = cont.text {
                info!("text: {text:#?}");
            }
        }
    }
    Ok(())
}

// EOF
