use anyhow::{Context, anyhow, bail};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use boardswarm_protocol::{MouseRequest, mouse_request};
use futures::StreamExt;
use prost::Message as _;
use tracing::warn;

use crate::{MouseId, MouseInput, Server};

pub async fn handler(State(server): State<Server>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(error) = handle_socket(server, socket).await {
            warn!(?error, "mouse websocket closed with error");
        }
    })
}

async fn handle_socket(server: Server, socket: WebSocket) -> anyhow::Result<()> {
    let (_sender, mut receiver) = socket.split();

    let first_message = match receiver.next().await {
        Some(message) => message.context("failed to read initial websocket frame")?,
        None => return Ok(()),
    };

    let mouse_id = match first_message {
        Message::Binary(data) => {
            let req =
                MouseRequest::decode(data).context("failed to decode initial MouseRequest")?;
            match req.item_or_signal {
                Some(mouse_request::ItemOrSignal::Item(id)) => id,
                _ => bail!("first websocket frame must select a mouse item"),
            }
        }
        Message::Close(_) => return Ok(()),
        _ => bail!("first websocket frame must be a binary MouseRequest"),
    };

    let mouse = server
        .get_mouse(MouseId(mouse_id))
        .ok_or_else(|| anyhow!("mouse {mouse_id} not found"))?;
    let input_tx = mouse.open().await?;

    while let Some(message) = receiver.next().await {
        match message.context("failed to read websocket frame")? {
            Message::Binary(data) => {
                let req =
                    MouseRequest::decode(data).context("failed to decode MouseRequest")?;
                match req.item_or_signal {
                    Some(mouse_request::ItemOrSignal::Input(input)) => {
                        match MouseInput::try_from(input) {
                            Ok(i) => {
                                if input_tx.send(i).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("ignoring invalid mouse input: {e}");
                            }
                        }
                    }
                    Some(mouse_request::ItemOrSignal::Item(_)) => {
                        warn!("unexpected mouse item selection after session start");
                    }
                    None => warn!("ignoring empty mouse request frame"),
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Text(_) => bail!("mouse websocket expects binary protobuf frames"),
        }
    }

    Ok(())
}
