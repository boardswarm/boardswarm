use boardswarm_client::client::{Boardswarm, SignalMsg};
use boardswarm_protocol::MediaScreenshotReply;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::warn;

use crate::{Media, MediaError, MediaSignalMsg, MediaSignallingRx};

/// Inbound signals from the local gRPC client (CLI) forwarded upstream.
enum InboundSignal {
    Answer(String),
    Ice { candidate: String, mline_index: u32 },
}

/// Implements [`MediaSignallingRx`] by queuing inbound signals into a channel
/// that is drained by the bridging task running in [`BoardswarmMedia::open`].
struct BoardswarmMediaSignalRx {
    tx: mpsc::UnboundedSender<InboundSignal>,
}

impl MediaSignallingRx for BoardswarmMediaSignalRx {
    fn offer(&mut self, _offer: &str) {
        // The upstream server is always the offerer; the CLI sends back an answer.
        // We do not expect an offer from the CLI side.
        warn!("Unexpected offer received from local client — ignoring");
    }

    fn answer(&mut self, answer: &str) {
        let _ = self.tx.send(InboundSignal::Answer(answer.to_string()));
    }

    fn ice(&mut self, candidate: &str, mline_index: u32) {
        let _ = self.tx.send(InboundSignal::Ice {
            candidate: candidate.to_string(),
            mline_index,
        });
    }
}

/// Proxies a remote boardswarm media item through the gRPC [`media_setup`] call.
///
/// When [`open`] is called the proxy:
/// 1. Establishes a [`MediaSession`] with the upstream server.
/// 2. Spawns a background task that bridges inbound signals (answer/ICE from the
///    local CLI) to the upstream `send_answer`/`send_ice` calls, and forwards
///    upstream signals (offer/ICE) to the outbound stream returned to the local
///    gRPC handler.
#[derive(Debug)]
pub struct BoardswarmMedia {
    id: u64,
    remote: Boardswarm,
}

impl BoardswarmMedia {
    pub fn new(id: u64, remote: Boardswarm) -> Self {
        Self { id, remote }
    }
}

#[async_trait::async_trait]
impl Media for BoardswarmMedia {
    async fn open(
        &self,
    ) -> Result<
        (
            Box<dyn MediaSignallingRx>,
            futures::stream::BoxStream<'static, MediaSignalMsg>,
        ),
        MediaError,
    > {
        let mut remote = self.remote.clone();
        let id = self.id;

        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<InboundSignal>();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<MediaSignalMsg>();

        tokio::spawn(async move {
            let mut session = match remote.media_setup(id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to start upstream media session for item {id}: {e}");
                    return;
                }
            };

            loop {
                tokio::select! {
                    // Forward inbound signals (from local CLI) to the upstream server.
                    Some(signal) = inbound_rx.recv() => {
                        let result = match signal {
                            InboundSignal::Answer(sdp) => session.send_answer(sdp).await,
                            InboundSignal::Ice { candidate, mline_index } => {
                                session.send_ice(candidate, mline_index).await
                            }
                        };
                        if let Err(e) = result {
                            warn!("Failed to send signal to upstream: {e}");
                            break;
                        }
                    }

                    // Forward outbound signals (from upstream server) to the local client.
                    msg = session.next_signal() => {
                        match msg {
                            Some(Ok(SignalMsg::Offer(sdp))) => {
                                let _ = outbound_tx.send(MediaSignalMsg::Offer(sdp));
                            }
                            Some(Ok(SignalMsg::Answer(sdp))) => {
                                let _ = outbound_tx.send(MediaSignalMsg::Answer(sdp));
                            }
                            Some(Ok(SignalMsg::Ice { candidate, mline_index })) => {
                                let _ = outbound_tx.send(MediaSignalMsg::Ice(candidate, mline_index));
                            }
                            Some(Err(e)) => {
                                warn!("Upstream media session error: {e}");
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        let rx = Box::new(BoardswarmMediaSignalRx { tx: inbound_tx });
        let stream = UnboundedReceiverStream::new(outbound_rx).boxed();
        Ok((rx, stream))
    }

    async fn screenshot(&self) -> Result<MediaScreenshotReply, MediaError> {
        let mut remote = self.remote.clone();
        let shot = remote
            .media_screenshot(self.id)
            .await
            .map_err(|e| match e.code() {
                tonic::Code::Unimplemented => MediaError::NotSupported,
                tonic::Code::Internal => MediaError::Internal(e.message().to_owned()),
                _ => MediaError::Internal(e.to_string()),
            })?;
        Ok(MediaScreenshotReply {
            mime_type: shot.mime_type,
            data: shot.data,
        })
    }
}
