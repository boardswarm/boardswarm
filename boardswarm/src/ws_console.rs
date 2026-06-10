use anyhow::{Context, anyhow, bail};
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use boardswarm_protocol::{ConsoleInputRequest, ConsoleOutput, console_input_request};
use futures::{SinkExt, StreamExt};
use prost::Message as _;
use tracing::warn;

use crate::{ConsoleId, Server};

pub async fn handler(State(server): State<Server>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if let Err(error) = handle_socket(server, socket).await {
            warn!(?error, "console websocket closed with error");
        }
    })
}

async fn handle_socket(server: Server, socket: WebSocket) -> anyhow::Result<()> {
    let (mut sender, mut receiver) = socket.split();

    let first_message = match receiver.next().await {
        Some(message) => message.context("failed to read initial websocket frame")?,
        None => return Ok(()),
    };

    let request = match first_message {
        Message::Binary(data) => ConsoleInputRequest::decode(data)
            .context("failed to decode initial ConsoleInputRequest")?,
        Message::Close(_) => return Ok(()),
        _ => bail!("first websocket frame must be a binary ConsoleInputRequest"),
    };

    let console_id = match request.target_or_data {
        Some(console_input_request::TargetOrData::Console(console)) => console,
        _ => bail!("first websocket frame must select a console target"),
    };

    let console = server
        .get_console(ConsoleId(console_id))
        .ok_or_else(|| anyhow!("can't find console {console_id}"))?;
    let mut input = console.input().await?;
    let mut output = console.output().await?;

    let inbound = async {
        while let Some(message) = receiver.next().await {
            match message.context("failed to read websocket frame")? {
                Message::Binary(data) => {
                    let request = ConsoleInputRequest::decode(data)
                        .context("failed to decode ConsoleInputRequest")?;
                    match request.target_or_data {
                        Some(console_input_request::TargetOrData::Data(data)) => {
                            input.send(data).await?
                        }
                        Some(console_input_request::TargetOrData::Console(_)) => {
                            bail!("target cannot be changed after session start")
                        }
                        None => bail!("console input frame was empty"),
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Text(_) => bail!("console websocket expects binary protobuf frames"),
            }
        }

        Ok::<(), anyhow::Error>(())
    };

    let outbound = async {
        while let Some(frame) = output.next().await {
            let output = ConsoleOutput { data: frame? };
            sender
                .send(Message::Binary(output.encode_to_vec().into()))
                .await
                .context("failed to send ConsoleOutput frame")?;
        }

        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        result = inbound => result,
        result = outbound => result,
    }
}
