use anyhow::{Context, anyhow, bail};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use boardswarm_protocol::{KeyboardRequest, KeyboardState, keyboard_request};
use futures::{SinkExt, StreamExt};
use prost::Message as _;
use tracing::warn;

use crate::{KeyboardEvent, KeyboardId, Server};

pub async fn handler(State(server): State<Server>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(error) = handle_socket(server, socket).await {
            warn!(?error, "keyboard websocket closed with error");
        }
    })
}

async fn handle_socket(server: Server, socket: WebSocket) -> anyhow::Result<()> {
    let (mut sender, mut receiver) = socket.split();

    let first_message = match receiver.next().await {
        Some(message) => message.context("failed to read initial websocket frame")?,
        None => return Ok(()),
    };

    let keyboard_id = match first_message {
        Message::Binary(data) => {
            let req = KeyboardRequest::decode(data)
                .context("failed to decode initial KeyboardRequest")?;
            match req.item_or_signal {
                Some(keyboard_request::ItemOrSignal::Item(id)) => id,
                _ => bail!("first websocket frame must select a keyboard item"),
            }
        }
        Message::Close(_) => return Ok(()),
        _ => bail!("first websocket frame must be a binary KeyboardRequest"),
    };

    let keyboard = server
        .get_keyboard(KeyboardId(keyboard_id))
        .ok_or_else(|| anyhow!("keyboard {keyboard_id} not found"))?;
    let (event_tx, state_stream) = keyboard.open().await?;

    let inbound = async {
        while let Some(message) = receiver.next().await {
            match message.context("failed to read websocket frame")? {
                Message::Binary(data) => {
                    let req = KeyboardRequest::decode(data)
                        .context("failed to decode KeyboardRequest")?;
                    match req.item_or_signal {
                        Some(keyboard_request::ItemOrSignal::Event(event)) => {
                            match KeyboardEvent::try_from(event) {
                                Ok(e) => {
                                    if event_tx.send(e).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    warn!("ignoring invalid keyboard event: {e}");
                                }
                            }
                        }
                        Some(keyboard_request::ItemOrSignal::Item(_)) => {
                            warn!("unexpected keyboard item selection after session start");
                        }
                        None => warn!("ignoring empty keyboard request frame"),
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Text(_) => bail!("keyboard websocket expects binary protobuf frames"),
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let outbound = async {
        let mut state_stream = std::pin::pin!(state_stream);
        while let Some(state) = state_stream.next().await {
            let proto: KeyboardState = state.into();
            sender
                .send(Message::Binary(proto.encode_to_vec().into()))
                .await
                .context("failed to send KeyboardState frame")?;
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        result = inbound => result,
        result = outbound => result,
    }
}
