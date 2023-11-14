use std::{pin::Pin, sync::Mutex};

use boardswarm_client::client::Boardswarm;
use boardswarm_protocol::Parameters;
use bytes::Bytes;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc::Receiver;

#[derive(Debug)]
pub struct BoardswarmConsole {
    id: u64,
    remote: Boardswarm,
    parameters: Mutex<Option<Parameters>>,
}

impl BoardswarmConsole {
    pub fn new(id: u64, remote: Boardswarm) -> Self {
        Self {
            id,
            remote,
            parameters: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl crate::Console for BoardswarmConsole {
    fn configure(
        &self,
        parameters: Box<dyn erased_serde::Deserializer>,
    ) -> Result<(), crate::ConsoleError> {
        let parameters = Parameters::deserialize(parameters).unwrap();
        let mut p = self.parameters.lock().unwrap();
        *p = Some(parameters);
        Ok(())
    }

    async fn input(
        &self,
    ) -> Result<
        Pin<Box<dyn futures::Sink<Bytes, Error = crate::ConsoleError> + Send>>,
        crate::ConsoleError,
    > {
        let mut remote = self.remote.clone();
        let p = self.parameters.lock().unwrap().clone();
        if let Some(p) = p {
            remote.console_configure(self.id, p).await.unwrap();
        }

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let id = self.id;
        tokio::spawn(async move {
            // Odd hoop to higher order trait error; Seemingly:
            // https://github.com/rust-lang/rust/issues/102211
            fn make_input(rx: Receiver<Bytes>) -> impl futures::Stream<Item = Bytes> {
                futures::stream::unfold(rx, |mut rx| async move {
                    let b = rx.recv().await;
                    b.map(|b| (b, rx))
                })
            }
            let input = make_input(rx);
            let _ = remote.console_stream_input(id, input).await;
        });

        Ok(Box::pin(futures::sink::unfold(
            tx,
            |tx, b: Bytes| async move {
                let _ = tx.send(b).await;
                Ok(tx)
            },
        )))
    }

    async fn output(
        &self,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<Bytes, crate::ConsoleError>>,
        crate::ConsoleError,
    > {
        let mut remote = self.remote.clone();
        let p = self.parameters.lock().unwrap().clone();
        if let Some(p) = p {
            remote.console_configure(self.id, p).await.unwrap();
        }

        Ok(Box::pin(
            remote.console_stream_output(self.id).await.unwrap().map(Ok),
        ))
    }
}
