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
    broadcast: broadcast::Sender<Vec<u8>>,
}

impl SerialPort {
    async fn new(path: impl Into<&str>, rate: u32) -> Result<Self> {
        let port = tokio_serial::new(path.into(), rate)
            .open_native_async()
            .unwrap();

        let (mut read, write) = tokio::io::split(port);
        let write = AsyncMutex::new(write);
        let broadcast = broadcast::channel(4096).0;
        let b_clone = broadcast.clone();
        tokio::spawn(async move {
            loop {
                let mut data = Vec::new();
                data.resize(2048, 0);
                let r = read.read(data.as_mut_slice()).await.unwrap();
                data.truncate(r);
                let _ = b_clone.send(data);
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

    async fn send_input(&self, input: &[u8]) {
        let mut write = self.write.lock().await;
        write.write_all(input).await.unwrap();
    }
}

struct SerialPortOutput {
    future: tokio_util::sync::ReusableBoxFuture<'static, (Vec<u8>, broadcast::Receiver<Vec<u8>>)>,
}

impl SerialPortOutput {
    fn new(receiver: broadcast::Receiver<Vec<u8>>) -> Self {
        let future = tokio_util::sync::ReusableBoxFuture::new(recv_data(receiver));
        Self { future }
    }
}

impl ConsoleStream for SerialPortOutput {}

async fn recv_data(
    mut rx: broadcast::Receiver<Vec<u8>>,
) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
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
    type Item = Vec<u8>;
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
