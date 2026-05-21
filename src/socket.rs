use std::sync::Arc;

use anyhow::{Context, Result};
use slack_morphism::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::AppEvent;
use crate::slack::Msg;

#[derive(Clone)]
struct EventSink(pub UnboundedSender<AppEvent>);

pub async fn run(app_token: String, tx: UnboundedSender<AppEvent>) -> Result<()> {
    let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));

    let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(on_push);

    let environment = Arc::new(
        SlackClientEventsListenerEnvironment::new(client.clone())
            .with_error_handler(on_error)
            .with_user_state(EventSink(tx)),
    );

    let listener = SlackClientSocketModeListener::new(
        &SlackClientSocketModeConfig::new(),
        environment.clone(),
        callbacks,
    );

    let token = SlackApiToken::new(app_token.into());
    listener
        .listen_for(&token)
        .await
        .context("socket mode: listen_for failed")?;
    listener.serve().await;
    Ok(())
}

async fn on_push(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let SlackEventCallbackBody::Message(msg) = event.event {
        let Some(channel) = msg.origin.channel.clone() else {
            return Ok(());
        };
        if msg.subtype.is_some() {
            // Skip joins/edits/etc. — polling will catch any rewrites we care about.
            return Ok(());
        }
        if let Some(parsed) = Msg::from_event(&msg) {
            let sink = {
                let guard = states.read().await;
                guard.get_user_state::<EventSink>().cloned()
            };
            if let Some(sink) = sink {
                let _ = sink.0.send(AppEvent::NewMessages(channel, vec![parsed]));
            }
        }
    }
    Ok(())
}

fn on_error(
    err: Box<dyn std::error::Error + Send + Sync>,
    _client: Arc<SlackHyperClient>,
    _states: SlackClientEventsUserState,
) -> slack_morphism::prelude::HttpStatusCode {
    eprintln!("socket-mode error: {err}");
    slack_morphism::prelude::HttpStatusCode::OK
}
