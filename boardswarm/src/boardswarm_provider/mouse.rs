use boardswarm_client::client::Boardswarm;
use tokio::sync::mpsc;
use tracing::warn;

use crate::{Mouse, MouseError, MouseInput};

/// Proxies a remote boardswarm mouse item through the gRPC `mouse_io` call.
#[derive(Debug)]
pub struct BoardswarmMouse {
    id: u64,
    remote: Boardswarm,
}

impl BoardswarmMouse {
    pub fn new(id: u64, remote: Boardswarm) -> Self {
        Self { id, remote }
    }
}

#[async_trait::async_trait]
impl Mouse for BoardswarmMouse {
    async fn open(&self) -> Result<mpsc::Sender<MouseInput>, MouseError> {
        let mut remote = self.remote.clone();
        let id = self.id;

        let (input_tx, mut input_rx) = mpsc::channel::<MouseInput>(16);

        tokio::spawn(async move {
            let mut session = match remote.mouse_io(id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to start upstream mouse session for item {id}: {e}");
                    return;
                }
            };

            while let Some(input) = input_rx.recv().await {
                if session
                    .send_input(input.buttons, input.x, input.y, input.wheel, input.hwheel)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(input_tx)
    }
}
