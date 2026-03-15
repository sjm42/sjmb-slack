// slackbot.rs

use std::{collections::HashMap, fs::File, io::BufReader, sync::Arc};

use regex::Regex;
use ::serde::{Deserialize, Serialize};
use slack_morphism::prelude::*;
use tokio::sync::{
    mpsc::{self, UnboundedReceiver, UnboundedSender},
    RwLock,
};

use crate::*;

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct SlackWorkspace {
    name: String,
    api_token: String,
    socket_token: String,

    #[serde(skip)]
    client: Option<Arc<SlackHyperClient>>,
    #[serde(skip)]
    api_token_runtime: Option<SlackApiToken>,
    #[serde(skip)]
    user_nicks: Arc<RwLock<HashMap<String, String>>>,
}

impl SlackWorkspace {
    async fn sender_nick(&self, sender: &SlackMessageSender) -> String {
        if let Some(nick) = sender_nick_hint(sender) {
            if let Some(user_id) = sender.user.as_ref() {
                self.cache_user_nick(user_id, &nick).await;
            }
            return nick;
        }

        if let Some(user_id) = sender.user.as_ref() {
            if let Some(nick) = self.cached_user_nick(user_id).await {
                return nick;
            }

            if let Some(nick) = self.lookup_user_nick(user_id).await {
                self.cache_user_nick(user_id, &nick).await;
                return nick;
            }

            return user_id.0.clone();
        }

        sender
            .bot_id
            .as_ref()
            .map(|bot_id| bot_id.0.clone())
            .unwrap_or_else(|| "N/A".to_string())
    }

    async fn cached_user_nick(&self, user_id: &SlackUserId) -> Option<String> {
        self.user_nicks.read().await.get(&user_id.0).cloned()
    }

    async fn cache_user_nick(&self, user_id: &SlackUserId, nick: &str) {
        if nick.is_empty() {
            return;
        }

        self.user_nicks
            .write()
            .await
            .insert(user_id.0.clone(), nick.to_string());
    }

    async fn lookup_user_nick(&self, user_id: &SlackUserId) -> Option<String> {
        let client = self.client.as_ref()?;
        let api_token = self.api_token_runtime.as_ref()?;
        let sess = client.open_session(api_token);
        match sess
            .users_info(&SlackApiUsersInfoRequest {
                user: user_id.clone(),
                include_locale: None,
            })
            .await
        {
            Ok(resp) => slack_user_nick(&resp.user),
            Err(e) => {
                warn!(
                    "WS {} users.info lookup failed for {}: {e:?}",
                    self.name, user_id.0
                );
                None
            }
        }
    }
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
    workspace: SlackWorkspace,
    tx: UnboundedSender<(Arc<Bot>, SlackWorkspace, SlackMessageEvent)>,
}

impl Bot {
    pub async fn new(opts: &OptsCommon) -> anyhow::Result<Self> {
        let now1 = Utc::now();

        let file = &opts.bot_config;
        info!("Reading config file {file}");
        let mut bot: Bot = serde_json::from_reader(BufReader::new(File::open(file)?))?;

        // pre-compile url detection regex
        bot.url_re = Some(Regex::new(&bot.url_regex)?);

        for ws in bot.workspaces.iter_mut() {
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
                        format!(
                            "{}-{}",
                            ws.name,
                            c.name_normalized
                                .clone()
                                .unwrap_or_else(|| "N/A".to_string())
                        ),
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
                .filter_map(|m| m.content.text.clone())
                .collect::<Vec<String>>();
            info!("Got last 10 messages: {msgs:#?}");
             */
            ws.client = Some(client);
            ws.api_token_runtime = Some(api_token);
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
        let (tx, rx) = mpsc::unbounded_channel::<(Arc<Bot>, SlackWorkspace, SlackMessageEvent)>();

        handles.push(tokio::spawn(async move { handle_messages(rx).await }));

        for ws in &bot.workspaces {
            let name = &ws.name;
            info!("SlackBot::run(): WS {name}");

            let sock_token = SlackApiToken::new(ws.socket_token.clone().into());
            let client = ws
                .client
                .as_ref()
                .cloned()
                .ok_or_else(|| anyhow!("No Slack client for workspace {name}"))?;
            let workspace = ws.clone();

            let socket_mode_callbacks = SlackSocketModeListenerCallbacks::new()
                .with_interaction_events(handler_interaction_events)
                .with_push_events(handler_push_events);

            let listener_environment = Arc::new(
                SlackClientEventsListenerEnvironment::new(client.clone())
                    .with_user_state(BotState {
                        bot: bot.clone(),
                        workspace: workspace.clone(),
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
        botstate
            .tx
            .send((botstate.bot.clone(), botstate.workspace.clone(), msge))?;
    }
    Ok(())
}

async fn handle_messages(mut rx: UnboundedReceiver<(Arc<Bot>, SlackWorkspace, SlackMessageEvent)>) {
    while let Some((bot, workspace, msg)) = rx.recv().await {
        // debug!("Got message: {msg:#?}");
        if let Err(e) = handle_msg(bot, workspace, msg).await {
            error!("Slack msg handling failed: {e:?}");
        }
    }
}

async fn handle_msg(
    bot: Arc<Bot>,
    workspace: SlackWorkspace,
    msg: SlackMessageEvent,
) -> anyhow::Result<()> {
    let SlackMessageEvent {
        origin,
        content,
        sender,
        ..
    } = msg;

    if let Some(channel_id) = origin.channel {
        let channel = channel_name(&bot, &channel_id.0);
        let nick = sender_nick(&workspace, &sender).await;

        if let Some(cont) = content
            && let Some(text) = cont.text
        {
            info!("#{channel} ({sender:?}): {text}");

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
                            nick: nick.clone(),
                            url: url_s,
                        },
                    )
                    .await?
                );
            }
        }
    }
    Ok(())
}

async fn sender_nick(workspace: &SlackWorkspace, sender: &SlackMessageSender) -> String {
    workspace.sender_nick(sender).await
}

fn sender_nick_hint(sender: &SlackMessageSender) -> Option<String> {
    sender
        .username
        .as_deref()
        .filter(|nick| !nick.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            sender
                .user_profile
                .as_ref()
                .and_then(slack_user_profile_nick)
        })
}

fn slack_user_nick(user: &SlackUser) -> Option<String> {
    user.profile
        .as_ref()
        .and_then(slack_user_profile_nick)
        .or_else(|| {
            user.real_name
                .as_deref()
                .filter(|nick| !nick.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            user.name
                .as_deref()
                .filter(|nick| !nick.is_empty())
                .map(str::to_owned)
        })
}

fn slack_user_profile_nick(profile: &SlackUserProfile) -> Option<String> {
    profile
        .display_name
        .as_deref()
        .filter(|nick| !nick.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            info!("No display_name");
            profile
                .display_name_normalized
                .as_deref()
                .filter(|nick| !nick.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            info!("No display_name_normalized");
            profile
                .real_name
                .as_deref()
                .filter(|nick| !nick.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            info!("No real_name");
            profile
                .real_name_normalized
                .as_deref()
                .filter(|nick| !nick.is_empty())
                .map(str::to_owned)
        })
}

fn channel_name<'a>(bot: &'a Bot, id: &'a str) -> &'a str {
    match bot.channels.get(id) {
        Some(s) => s.as_str(),
        None => "<NONE>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_nick_prefers_display_name() {
        let profile = SlackUserProfile {
            id: None,
            display_name: Some("display".into()),
            real_name: Some("real".into()),
            real_name_normalized: None,
            avatar_hash: None,
            status_text: None,
            status_expiration: None,
            status_emoji: None,
            display_name_normalized: None,
            email: None,
            icon: None,
            team: None,
            start_date: None,
            first_name: None,
            last_name: None,
            phone: None,
            pronouns: None,
            title: None,
            fields: None,
        };

        assert_eq!(
            slack_user_profile_nick(&profile).as_deref(),
            Some("display")
        );
    }

    #[test]
    fn user_nick_falls_back_to_real_name_then_name() {
        let user = SlackUser {
            id: SlackUserId("U123".into()),
            team_id: None,
            name: Some("legacy".into()),
            locale: None,
            profile: Some(SlackUserProfile {
                id: None,
                display_name: Some(String::new()),
                real_name: Some("Real Name".into()),
                real_name_normalized: None,
                avatar_hash: None,
                status_text: None,
                status_expiration: None,
                status_emoji: None,
                display_name_normalized: None,
                email: None,
                icon: None,
                team: None,
                start_date: None,
                first_name: None,
                last_name: None,
                phone: None,
                pronouns: None,
                title: None,
                fields: None,
            }),
            flags: SlackUserFlags {
                is_admin: None,
                is_app_user: None,
                is_bot: None,
                is_invited_user: None,
                is_owner: None,
                is_primary_owner: None,
                is_restricted: None,
                is_stranger: None,
                is_ultra_restricted: None,
                has_2fa: None,
            },
            tz: None,
            tz_label: None,
            tz_offset: None,
            updated: None,
            deleted: None,
            color: None,
            real_name: None,
            enterprise_user: None,
        };

        assert_eq!(slack_user_nick(&user).as_deref(), Some("Real Name"));
    }
}
// EOF
