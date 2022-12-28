use bytes::{Bytes, BytesMut};
use futures::ready;
use std::{
    net::ToSocketAddrs,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::Result;
use futures::stream::Stream;
use protocol::{protocol::serial_server, Console, ConsoleStream, SerialServer};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    sync::{broadcast, Mutex as AsyncMutex},
};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

struct SerialPort {
    //read: ReadHalf<SerialStream>,
    write: AsyncMutex<WriteHalf<SerialStream>>,
    broadcast: broadcast::Sender<Bytes>,
}

impl SerialPort {
    async fn new(path: impl Into<&str>, rate: u32) -> Result<Self> {
        let port = tokio_serial::new(path.into(), rate)
            .open_native_async()
            .unwrap();

        let (mut read, write) = tokio::io::split(port);
        let write = AsyncMutex::new(write);
        let broadcast = broadcast::channel(64).0;
        let b_clone = broadcast.clone();
        tokio::spawn(async move {
            loop {
                let mut data = BytesMut::zeroed(1024);
                let r = read.read(&mut data).await.unwrap();
                data.truncate(r);
                let _ = b_clone.send(data.freeze());
            }
        });
        Ok(SerialPort {
            //read,
            write,
            broadcast,
        })
    }

    fn start(&self) {}
}

#[tonic::async_trait]
impl Console for SerialPort {
    async fn get_output(&self) -> Pin<Box<dyn protocol::ConsoleStream>> {
        Box::pin(SerialPortOutput::new(self.broadcast.subscribe()))
    }

    async fn send_input(&self, input: Bytes) {
        let mut write = self.write.lock().await;
        write.write_all(&input).await.unwrap();
    }
}

struct SerialPortOutput {
    future: tokio_util::sync::ReusableBoxFuture<'static, (Bytes, broadcast::Receiver<Bytes>)>,
}

impl SerialPortOutput {
    fn new(receiver: broadcast::Receiver<Bytes>) -> Self {
        let future = tokio_util::sync::ReusableBoxFuture::new(recv_data(receiver));
        Self { future }
    }
}

impl ConsoleStream for SerialPortOutput {}

async fn recv_data(mut rx: broadcast::Receiver<Bytes>) -> (Bytes, broadcast::Receiver<Bytes>) {
    loop {
        match rx.recv().await {
            Ok(data) => {
                return (data, rx);
            }
            Err(e) => eprintln!("{:?}", e),
        }
    }
}

impl Stream for SerialPortOutput {
    type Item = Bytes;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (result, rx) = ready!(self.future.poll(cx));
        self.future.set(recv_data(rx));
        Poll::Ready(Some(result))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut server = SerialServer::new();
    server.register_console(
        "rk3588".to_string(),
        Arc::new(SerialPort::new("/dev/ttyUSB3", 1_500_000).await?),
    );
    server.register_console(
        "am62".to_string(),
        Arc::new(SerialPort::new("/dev/ttyUSB4", 115_200).await?),
    );

    tonic::transport::Server::builder()
        .add_service(serial_server::SerialServer::new(server))
        .serve("[::1]:50051".to_socket_addrs().unwrap().next().unwrap())
        .await
        .unwrap();

    Ok(())
}
