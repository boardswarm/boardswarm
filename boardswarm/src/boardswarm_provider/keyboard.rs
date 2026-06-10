use boardswarm_client::client::Boardswarm;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::warn;

use crate::{Keyboard, KeyboardError, KeyboardEvent, KeyboardState};

/// Proxies a remote boardswarm keyboard item through the gRPC `keyboard_io` call.
#[derive(Debug)]
pub struct BoardswarmKeyboard {
    id: u64,
    remote: Boardswarm,
}

impl BoardswarmKeyboard {
    pub fn new(id: u64, remote: Boardswarm) -> Self {
        Self { id, remote }
    }
}

#[async_trait::async_trait]
impl Keyboard for BoardswarmKeyboard {
    async fn open(
        &self,
    ) -> Result<
        (
            mpsc::Sender<KeyboardEvent>,
            futures::stream::BoxStream<'static, KeyboardState>,
        ),
        KeyboardError,
    > {
        let mut remote = self.remote.clone();
        let id = self.id;

        let (event_tx, mut event_rx) = mpsc::channel::<KeyboardEvent>(16);
        let (state_tx, state_rx) = mpsc::unbounded_channel::<KeyboardState>();

        tokio::spawn(async move {
            let mut session = match remote.keyboard_io(id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to start upstream keyboard session for item {id}: {e}");
                    return;
                }
            };

            loop {
                tokio::select! {
                    // Forward key events from local caller to the upstream server.
                    event = event_rx.recv() => {
                        match event {
                            Some(KeyboardEvent::Down(key)) => {
                                if session.send_key_down(key).await.is_err() {
                                    break;
                                }
                            }
                            Some(KeyboardEvent::Up(key)) => {
                                if session.send_key_up(key).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }

                    // Forward LED state updates from the upstream server to the local caller.
                    state = session.next_state() => {
                        match state {
                            Some(Ok(proto_state)) => {
                                let leds = proto_state
                                    .led
                                    .into_iter()
                                    .filter_map(|v| {
                                        boardswarm_protocol::KeyboardLed::try_from(v).ok()
                                    })
                                    .collect();
                                let _ = state_tx.send(KeyboardState { leds });
                            }
                            Some(Err(e)) => {
                                warn!("Upstream keyboard session error: {e}");
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        let stream = UnboundedReceiverStream::new(state_rx).boxed();
        Ok((event_tx, stream))
    }
}
