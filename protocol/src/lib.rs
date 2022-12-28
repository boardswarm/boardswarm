use std::pin::Pin;
use std::task::Poll;
use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use futures::ready;
use futures::stream::BoxStream;
use futures::stream::Stream;
use protocol::{serial_server::Serial, Output};
use tonic::Streaming;

#[tonic::async_trait]
pub trait ConsoleStream: Stream<Item = Bytes> + Send {}

#[tonic::async_trait]
pub trait Console: Send + Sync + 'static {
    async fn get_output(&self) -> Pin<Box<dyn ConsoleStream>>;
    async fn send_input(&self, input: Bytes);
}

pub mod protocol {
    tonic::include_proto!("serial");
}

#[derive(Clone)]
pub struct SerialServer {
    consoles: HashMap<String, Arc<dyn Console>>,
}

impl SerialServer {
    pub fn new() -> Self {
        Self {
            consoles: HashMap::new(),
        }
    }

    pub fn register_console(&mut self, name: String, console: Arc<dyn Console>) {
        self.consoles.insert(name, console);
    }
}

#[tonic::async_trait]
impl Serial for SerialServer {
    type StreamOutputStream = BoxStream<'static, Result<Output, tonic::Status>>;

    async fn stream_output(
        &self,
        request: tonic::Request<protocol::OutputRequest>,
    ) -> Result<tonic::Response<Self::StreamOutputStream>, tonic::Status> {
        if let Some(console) = self.consoles.get(&request.into_inner().name) {
            let inner = console.get_output().await;
            let wrapper = Box::pin(OutputWrapper { inner });
            Ok(tonic::Response::new(wrapper))
        } else {
            Err(tonic::Status::invalid_argument("can't find console").into())
        }
    }

    async fn stream_input(
        &self,
        request: tonic::Request<Streaming<protocol::InputRequest>>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let mut rx = request.into_inner();
        let mut console = None;
        while let Ok(Some(request)) = rx.message().await {
            if console.is_none() {
                console = self.consoles.get(&request.name.unwrap());
            }
            if let Some(console) = console {
                console.send_input(request.data).await;
            } else {
                break;
            }
        }
        Ok(tonic::Response::new(()))
    }
}

struct OutputWrapper<T>
where
    T: Stream<Item = Bytes> + Unpin,
{
    inner: T,
}

impl<T> Stream for OutputWrapper<T>
where
    T: Stream<Item = Bytes> + Unpin,
{
    type Item = Result<Output, tonic::Status>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let me = self.get_mut();
        let data = ready!(Pin::new(&mut me.inner).poll_next(cx));
        Poll::Ready(Some(Ok(Output {
            data: data.unwrap(),
        })))
    }
}
