// Local serial port
use bytes::{Bytes, BytesMut};
use futures::ready;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tracing::info;

use anyhow::Result;
use futures::prelude::*;
use futures::stream::Stream;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, WriteHalf},
    sync::{broadcast, Mutex as AsyncMutex},
};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

#[derive(Debug)]
struct SerialOpen {
    write: Arc<AsyncMutex<WriteHalf<SerialStream>>>,
    broadcast: broadcast::Sender<Bytes>,
}

#[derive(Debug)]
pub(crate) struct SerialPort {
    name: String,
    rate: Mutex<u32>,
    attributes: HashMap<String, String>,
    open: AsyncMutex<Option<SerialOpen>>,
}
use crate::ConsoleError;

impl SerialPort {
    pub fn new(name: String, attributes: HashMap<String, String>) -> Self {
        let open = AsyncMutex::new(None);
        let rate = Mutex::new(115_200);
        SerialPort {
            name,
            attributes,
            rate,
            open,
        }
    }

    pub async fn open(&self) -> Result<()> {
        let rate = *self.rate.lock().unwrap();
        let mut open = self.open.lock().await;
        let port = tokio_serial::new(&self.name, rate).open_native_async()?;

        let (mut read, write) = tokio::io::split(port);

        let broadcast = broadcast::channel(64).0;
        let b_clone = broadcast.clone();
        let write = Arc::new(AsyncMutex::new(write));
        tokio::spawn(async move {
            loop {
                let mut data = BytesMut::zeroed(1024);
                let r = read.read(&mut data).await.unwrap();
                data.truncate(r);
                let _ = b_clone.send(data.freeze());
            }
        });
        *open = Some(SerialOpen { write, broadcast });
        Ok(())
    }

    async fn get_writer(&self) -> Arc<AsyncMutex<WriteHalf<SerialStream>>> {
        if let Some(open) = &*self.open.lock().await {
            return open.write.clone();
        }
        self.open().await.unwrap();
        if let Some(open) = &*self.open.lock().await {
            return open.write.clone();
        }
        unreachable!();
    }

    async fn get_reader(&self) -> broadcast::Receiver<Bytes> {
        if let Some(open) = &*self.open.lock().await {
            return open.broadcast.subscribe();
        }
        self.open().await.unwrap();
        if let Some(open) = &*self.open.lock().await {
            return open.broadcast.subscribe();
        }
        unreachable!();
    }
}

#[async_trait::async_trait]
impl crate::Console for SerialPort {
    fn name(&self) -> &str {
        &self.name
    }

    fn configure(&self, parameters: serde_yaml::Value) -> Result<(), crate::ConsoleError> {
        #[derive(serde::Deserialize)]
        struct Config {
            rate: u32,
        }
        let config: Config = serde_yaml::from_value(parameters).unwrap();
        let mut r = self.rate.lock().unwrap();
        *r = config.rate;
        Ok(())
    }

    async fn input(
        &self,
    ) -> Result<
        Pin<Box<dyn futures::Sink<Bytes, Error = crate::ConsoleError> + Send>>,
        crate::ConsoleError,
    > {
        let writer = self.get_writer().await;
        Ok(Box::pin(sink::unfold(
            writer,
            |writer, input: Bytes| async move {
                let mut w = writer.lock().await;
                w.write_all(&input).await.unwrap();
                drop(w);
                Ok(writer)
            },
        )))
    }

    async fn output(
        &self,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<Bytes, crate::ConsoleError>>,
        crate::ConsoleError,
    > {
        Ok(Box::pin(SerialPortOutput::new(self.get_reader().await)))
    }

    fn matches(&self, filter: serde_yaml::Value) -> bool {
        let filter: HashMap<String, String> = serde_yaml::from_value(filter).unwrap();
        for (k, v) in filter.iter() {
            match self.attributes.get(k) {
                Some(a_v) if a_v == v => (),
                a_v => {
                    info!("no match {}: {} - {:?}", k, v, a_v);
                    return false;
                }
            }
        }
        true
    }
}

pub struct SerialPortOutput {
    future: tokio_util::sync::ReusableBoxFuture<
        'static,
        (Result<Bytes, ConsoleError>, broadcast::Receiver<Bytes>),
    >,
}

impl SerialPortOutput {
    fn new(receiver: broadcast::Receiver<Bytes>) -> Self {
        let future = tokio_util::sync::ReusableBoxFuture::new(recv_data(receiver));
        Self { future }
    }
}

impl Stream for SerialPortOutput {
    type Item = Result<Bytes, ConsoleError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (result, rx) = ready!(self.future.poll(cx));
        self.future.set(recv_data(rx));
        Poll::Ready(Some(result))
    }
}

async fn recv_data(
    mut rx: broadcast::Receiver<Bytes>,
) -> (Result<Bytes, ConsoleError>, broadcast::Receiver<Bytes>) {
    loop {
        match rx.recv().await {
            Ok(data) => {
                return (Ok(data), rx);
            }
            Err(e) => eprintln!("{:?}", e),
        }
    }
}
