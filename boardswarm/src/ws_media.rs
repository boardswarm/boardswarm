use anyhow::{Context, anyhow, bail};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use boardswarm_protocol::{MediaRequest, SignalMessage, media_request, signal_message};
use futures::{SinkExt, StreamExt};
use prost::Message as _;
use tracing::warn;

use crate::{MediaId, Server};

pub async fn handler(State(server): State<Server>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(error) = handle_socket(server, socket).await {
            warn!(?error, "media websocket closed with error");
        }
    })
}

async fn handle_socket(server: Server, socket: WebSocket) -> anyhow::Result<()> {
    let (mut sender, mut receiver) = socket.split();

    let first_message = match receiver.next().await {
        Some(message) => message.context("failed to read initial websocket frame")?,
        None => return Ok(()),
    };

    let media_id = match first_message {
        Message::Binary(data) => {
            let req =
                MediaRequest::decode(data).context("failed to decode initial MediaRequest")?;
            match req.item_or_signal {
                Some(media_request::ItemOrSignal::Item(id)) => id,
                _ => bail!("first websocket frame must select a media item"),
            }
        }
        Message::Close(_) => return Ok(()),
        _ => bail!("first websocket frame must be a binary MediaRequest"),
    };

    let media = server
        .get_media(MediaId(media_id))
        .ok_or_else(|| anyhow!("media {media_id} not found"))?;

    let (mut rx, mut tx) = media.open().await?;

    let inbound = async {
        while let Some(message) = receiver.next().await {
            match message.context("failed to read websocket frame")? {
                Message::Binary(data) => {
                    let req =
                        MediaRequest::decode(data).context("failed to decode MediaRequest")?;
                    match req.item_or_signal {
                        Some(media_request::ItemOrSignal::Signal(signal)) => {
                            match signal.sdp_message {
                                Some(signal_message::SdpMessage::Offer(sdp)) => rx.offer(&sdp.sdp),
                                Some(signal_message::SdpMessage::Answer(sdp)) => {
                                    rx.answer(&sdp.sdp)
                                }
                                Some(signal_message::SdpMessage::Ice(ice)) => {
                                    rx.ice(&ice.candidate, ice.mline_index)
                                }
                                None => warn!("ignoring empty signal in media websocket"),
                            }
                        }
                        Some(media_request::ItemOrSignal::Item(_)) => {
                            warn!("unexpected media item selection after session start");
                        }
                        None => warn!("ignoring empty media request frame"),
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Text(_) => bail!("media websocket expects binary protobuf frames"),
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let outbound = async {
        while let Some(msg) = tx.next().await {
            let signal: SignalMessage = msg.into();
            sender
                .send(Message::Binary(signal.encode_to_vec().into()))
                .await
                .context("failed to send SignalMessage frame")?;
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        result = inbound => result,
        result = outbound => result,
    }
}
