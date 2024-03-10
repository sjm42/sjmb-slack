// slackbot.rs

use ::serde::{Deserialize, Serialize};
use anyhow::anyhow;
use chrono::*;
use log::*;
use regex::Regex;
use slack_morphism::prelude::*;
use std::{collections::HashMap, fs::File, io::BufReader, sync::Arc};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::*;

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct SlackWorkspace {
    name: String,
    api_token: String,
    socket_token: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Bot {
    url_regex: String,
    url_log_db: String,
    workspaces: Vec<SlackWorkspace>,

    #[serde(skip)]
    url_re: Option<Regex>,
    #[serde(skip)]
    channels: HashMap<String, String>,
}

#[derive(Debug)]
struct BotState {
    bot: Arc<Bot>,
    tx: UnboundedSender<(Arc<Bot>, SlackMessageEvent)>,
}

impl Bot {
    pub async fn new(opts: &OptsCommon) -> anyhow::Result<Self> {
        let now1 = Utc::now();

        let file = &opts.bot_config;
        info!("Reading config file {file}");
        let mut bot: Bot = serde_json::from_reader(BufReader::new(File::open(file)?))?;

        // Expand $HOME where relevant
        bot.url_log_db = shellexpand::full(&bot.url_log_db)?.into_owned();

        // pre-compile url detection regex
        bot.url_re = Some(Regex::new(&bot.url_regex)?);

        for ws in bot.workspaces.iter() {
            info!("SlackBot::new(): WS {}", ws.name);
            let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));

            let api_token = SlackApiToken::new(ws.api_token.clone().into());

            info!("Testing API...");
            let sess = client.open_session(&api_token);
            let ret = sess
                .api_test(&SlackApiTestRequest::new().with_foo("Test".into()))
                .await?;
            debug!("Test result: {ret:#?}");

            info!("Getting all channels...");
            let chans = SlackApiConversationsListRequest::new()
                .with_exclude_archived(true)
                .with_limit(100)
                .scroll(&sess)
                .await?
                .channels;
            // debug!("Channels: {chans:#?}");

            chans
                .iter()
                .filter(|c| c.name_normalized.is_some())
                .for_each(|c| {
                    bot.channels.insert(
                        c.id.to_string(),
                        format!("{}-{}", ws.name, c.name_normalized.as_ref().unwrap()),
                    );
                });

            /*
            // get and display last 10 messages
            let msgs = SlackApiConversationsHistoryRequest::new()
                .with_channel("id".into())
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
            */
        }
        debug!("channels: {:#?}", &bot.channels);

        info!(
            "New runtime config successfully created in {} ms.",
            Utc::now().signed_duration_since(now1).num_milliseconds()
        );
        // debug!("New BotConfig:\n{bot:#?}");

        Ok(bot)
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let mut handles = vec![];
        let bot = Arc::new(self.clone());
        let (tx, rx) = mpsc::unbounded_channel::<(Arc<Bot>, SlackMessageEvent)>();

        handles.push(tokio::spawn(async move { handle_messages(rx).await }));

        for ws in &self.workspaces {
            let name = &ws.name;
            info!("SlackBot::run(): WS {name}");

            let sock_token = SlackApiToken::new(ws.socket_token.clone().into());
            let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));

            let socket_mode_callbacks = SlackSocketModeListenerCallbacks::new()
                .with_interaction_events(handler_interaction_events)
                .with_push_events(handler_push_events);

            let listener_environment = Arc::new(
                SlackClientEventsListenerEnvironment::new(client.clone())
                    .with_user_state(BotState {
                        bot: bot.clone(),
                        tx: tx.clone(),
                    })
                    .with_error_handler(handler_error),
            );

            debug!("WS {name} creating listener");
            let socket_mode_listener = SlackClientSocketModeListener::new(
                &SlackClientSocketModeConfig::new(),
                listener_environment.clone(),
                socket_mode_callbacks,
            );

            debug!("WS {name} listening for events");
            // Register an app token to listen for events,
            socket_mode_listener.listen_for(&sock_token).await?;

            let wsname = name.clone();
            handles.push(tokio::spawn(async move {
                debug!("WS {wsname} serve...");
                error!(
                    "Socket listener returned {}",
                    socket_mode_listener.serve().await
                );
            }));
        }
        drop(tx);
        futures::future::join_all(handles).await;
        Ok(())
    }
}

fn handler_error(
    err: Box<dyn std::error::Error + Send + Sync>,
    _client: Arc<SlackHyperClient>,
    _state: SlackClientEventsUserState,
) -> http::status::StatusCode {
    error!("{:#?}", err);

    // This return value should be OK if we want to return successful ack
    // to the Slack server using Web-sockets
    // https://api.slack.com/apis/connections/socket-implement#acknowledge
    // so that Slack knows whether to retry
    http::StatusCode::OK
}

async fn handler_interaction_events(
    event: SlackInteractionEvent,
    _client: Arc<SlackHyperClient>,
    _state: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("{:#?}", event);
    Ok(())
}

async fn handler_push_events(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    state: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // info!("{:#?}", event);
    let statelock = state.write().await;

    let botstate = statelock
        .get_user_state::<BotState>()
        .ok_or_else(|| anyhow::anyhow!("No state"))?;
    // debug!("{:#?}", botstate);

    // Handle message events here
    if let SlackEventCallbackBody::Message(msge) = event.event {
        botstate.tx.send((botstate.bot.clone(), msge))?;
    }
    Ok(())
}

async fn handle_messages(mut rx: UnboundedReceiver<(Arc<Bot>, SlackMessageEvent)>) {
    while let Some((bot, msg)) = rx.recv().await {
        // debug!("Got message: {msg:#?}");
        if let Err(e) = handle_msg(bot, msg).await {
            error!("Slack msg handling failed: {e:?}");
        }
    }
}

async fn handle_msg(bot: Arc<Bot>, msg: SlackMessageEvent) -> anyhow::Result<()> {
    if let Some(channel_id) = msg.origin.channel {
        let channel = channel_name(&bot, &channel_id.0);
        if let Some(cont) = msg.content {
            if let Some(text) = cont.text {
                info!("#{channel}: {text}");

                for url_cap in bot
                    .url_re
                    .as_ref()
                    .ok_or_else(|| anyhow!("No url_regex_re"))?
                    .captures_iter(text.as_ref())
                {
                    let url_s = url_cap[1].to_string();
                    info!("*** on {channel} detected url: {url_s}");
                    let mut dbc = start_db(&bot.url_log_db).await?;
                    info!(
                        "Urllog: inserted {} row(s)",
                        db_add_url(
                            &mut dbc,
                            &UrlCtx {
                                ts: Utc::now().timestamp(),
                                chan: channel.into(),
                                nick: "N/A".into(),
                                url: url_s,
                            },
                        )
                        .await?
                    );
                }
            }
        }
    }
    Ok(())
}

fn channel_name<'a>(bot: &'a Bot, id: &'a str) -> &'a str {
    match bot.channels.get(id) {
        Some(s) => s.as_str(),
        None => "<NONE>",
    }
}

// EOF
