use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{ready, Poll},
};

use boardswarm_protocol::{
    boardswarm_client::BoardswarmClient, console_input_request, volume_io_reply, volume_io_request,
    ActuatorModeRequest, ConsoleConfigureRequest, ConsoleInputRequest, ConsoleOutputRequest,
    DeviceModeRequest, DeviceRequest, Item, ItemPropertiesRequest, ItemType, ItemTypeRequest,
    VolumeInfoMsg, VolumeIoFlush, VolumeIoRead, VolumeIoReply, VolumeIoRequest, VolumeIoTarget,
    VolumeIoWrite, VolumeRequest, VolumeTarget,
};
use bytes::Bytes;
use futures::{future::BoxFuture, stream, FutureExt, Stream, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncSeek, AsyncWrite},
    sync::{mpsc, oneshot},
};

use tower::Layer;
use tracing::warn;

use crate::{
    authenticator::{Authenticator, AuthenticatorService},
    config::Auth,
    oidc::{LoginProvider, OidcClientBuilder},
};

#[derive(Clone, Debug)]
pub struct BoardswarmBuilder {
    uri: tonic::transport::Uri,
    auth: Option<Auth>,
    login_provider: Option<Arc<dyn LoginProvider>>,
}

impl BoardswarmBuilder {
    pub fn new(uri: tonic::transport::Uri) -> Self {
        Self {
            uri,
            auth: None,
            login_provider: None,
        }
    }

    pub fn auth_static<S: Into<String>>(&mut self, token: S) {
        self.auth = Some(Auth::Token(token.into()));
    }

    pub fn auth(&mut self, auth: Auth) {
        self.auth = Some(auth)
    }

    pub fn login_provider<LP: Into<Arc<dyn LoginProvider + 'static>>>(
        &mut self,
        login_provider: LP,
    ) {
        self.login_provider = Some(login_provider.into());
    }

    pub async fn connect(self) -> Result<Boardswarm, tonic::transport::Error> {
        let endpoint = tonic::transport::Endpoint::from(self.uri)
            .tls_config(tonic::transport::ClientTlsConfig::new().with_enabled_roots())?;
        let channel = endpoint.connect().await?;
        let authenticator = match self.auth {
            Some(Auth::Token(t)) => Authenticator::from_static(t),
            Some(Auth::Oidc {
                uri,
                client_id,
                token_cache,
            }) => {
                let mut builder = OidcClientBuilder::new(uri, client_id);
                builder.token_cache(token_cache);
                if let Some(lp) = self.login_provider {
                    builder.login_provider(lp);
                }
                let oidc = builder.build();
                Authenticator::from_oidc(oidc)
            }
            None => Authenticator::default(),
        };
        let channel = authenticator.into_layer().layer(channel);
        let client = BoardswarmClient::new(channel);
        Ok(Boardswarm { client })
    }
}

pub enum ItemEvent {
    Added(Vec<Item>),
    Removed(u64),
}

#[derive(Clone, Debug)]
pub enum AuthMethod {
    Oidc { url: String, client_id: String },
}

impl From<boardswarm_protocol::login_info::Method> for AuthMethod {
    fn from(value: boardswarm_protocol::login_info::Method) -> Self {
        match value {
            boardswarm_protocol::login_info::Method::Oidc(o) => AuthMethod::Oidc {
                url: o.url,
                client_id: o.client_id,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct LoginInfo {
    pub description: String,
    pub method: AuthMethod,
}

#[derive(Clone, Debug)]
pub struct Boardswarm {
    client: BoardswarmClient<AuthenticatorService<tonic::transport::Channel>>,
}

impl Boardswarm {
    pub async fn login_info(&mut self) -> Result<Vec<LoginInfo>, tonic::Status> {
        let info = self.client.login_info(()).await?;
        let info = info.into_inner();

        Ok(info
            .info
            .into_iter()
            .filter_map(|i| {
                Some(LoginInfo {
                    description: i.description,
                    method: i.method?.into(),
                })
            })
            .collect())
    }

    pub async fn list(&mut self, type_: ItemType) -> Result<Vec<Item>, tonic::Status> {
        let items = self
            .client
            .list(ItemTypeRequest {
                r#type: type_.into(),
            })
            .await?;

        Ok(items.into_inner().item)
    }

    pub async fn properties(
        &mut self,
        type_: ItemType,
        item: u64,
    ) -> Result<HashMap<String, String>, tonic::Status> {
        let properties = self
            .client
            .item_properties(ItemPropertiesRequest {
                r#type: type_.into(),
                item,
            })
            .await?
            .into_inner();
        Ok(properties
            .property
            .into_iter()
            .map(|v| (v.key, v.value))
            .collect())
    }

    pub async fn monitor(
        &mut self,
        type_: ItemType,
    ) -> Result<impl Stream<Item = Result<ItemEvent, tonic::Status>>, tonic::Status> {
        let items = self
            .client
            .monitor(ItemTypeRequest {
                r#type: type_.into(),
            })
            .await?
            .into_inner();

        Ok(items.filter_map(|event| async {
            event
                .map(|event| {
                    event.event.map(|event| match event {
                        boardswarm_protocol::item_event::Event::Add(added) => {
                            ItemEvent::Added(added.item)
                        }
                        boardswarm_protocol::item_event::Event::Remove(removed) => {
                            ItemEvent::Removed(removed)
                        }
                    })
                })
                .transpose()
        }))
    }

    pub async fn device_info(
        &mut self,
        device: u64,
    ) -> Result<impl Stream<Item = Result<boardswarm_protocol::Device, tonic::Status>>, tonic::Status>
    {
        let r = self.client.device_info(DeviceRequest { device }).await?;
        Ok(r.into_inner())
    }

    pub async fn device_change_mode(
        &mut self,
        device: u64,
        mode: String,
    ) -> Result<(), tonic::Status> {
        self.client
            .device_change_mode(DeviceModeRequest { device, mode })
            .await?;
        Ok(())
    }

    pub async fn console_stream_input<I>(
        &mut self,
        console: u64,
        input: I,
    ) -> Result<(), tonic::Status>
    where
        I: Stream<Item = Bytes> + Send + 'static,
    {
        self.client
            .console_stream_input(
                stream::once(async move {
                    ConsoleInputRequest {
                        target_or_data: Some(console_input_request::TargetOrData::Console(console)),
                    }
                })
                .chain(input.map(|i| ConsoleInputRequest {
                    target_or_data: Some(console_input_request::TargetOrData::Data(i)),
                })),
            )
            .await?;
        Ok(())
    }

    pub async fn console_stream_output(
        &mut self,
        console: u64,
    ) -> Result<impl Stream<Item = Bytes>, tonic::Status> {
        let request = tonic::Request::new(ConsoleOutputRequest { console });
        let response = self.client.console_stream_output(request).await?;
        let stream = response.into_inner();
        Ok(stream.filter_map(|output| async {
            let output = output.ok()?;
            Some(output.data)
        }))
    }

    pub async fn console_configure(
        &mut self,
        console: u64,
        parameters: boardswarm_protocol::Parameters,
    ) -> Result<(), tonic::Status> {
        let configure = ConsoleConfigureRequest {
            console,
            parameters: Some(parameters),
        };
        self.client.console_configure(configure).await?;
        Ok(())
    }

    pub async fn actuator_change_mode(
        &mut self,
        actuator: u64,
        parameters: boardswarm_protocol::Parameters,
    ) -> Result<(), tonic::Status> {
        let mode = ActuatorModeRequest {
            actuator,
            parameters: Some(parameters),
        };
        self.client.actuator_change_mode(mode).await?;
        Ok(())
    }

    pub async fn volume_info(&mut self, volume: u64) -> Result<VolumeInfoMsg, tonic::Status> {
        let request = tonic::Request::new(VolumeRequest { volume });
        let r = self.client.volume_info(request).await?;
        Ok(r.into_inner())
    }

    /// Return an IO object for the volume target that implements AsyncRead, AsyncWrite and
    /// AsyncSeek
    pub async fn volume_io_readwrite<S: Into<String> + AsRef<str>>(
        &mut self,
        volume: u64,
        target: S,
        length: Option<u64>,
    ) -> Result<VolumeIoRW, tonic::Status> {
        let info = self.volume_info(volume).await?;
        let info = info
            .target
            .into_iter()
            .find(|t| t.name == target.as_ref())
            .ok_or_else(|| tonic::Status::unavailable("Target not found in volume"))?;
        let io = self.volume_io(volume, target, length).await?;

        Ok(VolumeIoRW::new(io, info))
    }

    pub async fn volume_io<S: Into<String>>(
        &mut self,
        volume: u64,
        target: S,
        length: Option<u64>,
    ) -> Result<VolumeIo, tonic::Status> {
        let (requests, requests_rx) = mpsc::channel(1);
        let (outstanding_tx, outstanding_rx) = mpsc::unbounded_channel();

        let target = target.into();
        let replies = self
            .client
            .volume_io(
                stream::once(async move {
                    VolumeIoRequest {
                        target_or_request: Some(volume_io_request::TargetOrRequest::Target(
                            VolumeIoTarget {
                                volume,
                                target,
                                length,
                            },
                        )),
                    }
                })
                .chain(stream::unfold(
                    (requests_rx, outstanding_tx),
                    |(mut requests_rx, outstanding_tx)| async move {
                        let (request, outstanding) = requests_rx.recv().await?;
                        outstanding_tx.send(outstanding).ok()?;
                        Some((request, (requests_rx, outstanding_tx)))
                    },
                )),
            )
            .await?;
        let io = VolumeIo::new(requests, replies.into_inner(), outstanding_rx);
        Ok(io)
    }

    pub async fn volume_commit(&mut self, volume: u64) -> Result<(), tonic::Status> {
        let request = tonic::Request::new(VolumeRequest { volume });
        self.client.volume_commit(request).await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("No more requests can be send")]
pub struct VolumeIoNoMoreRequests();

#[derive(Debug)]
pub struct VolumeIoWriteRequest {
    rx: oneshot::Receiver<Result<u64, tonic::Status>>,
}

impl Future for VolumeIoWriteRequest {
    type Output = Result<u64, tonic::Status>;

    fn poll(
        //mut self: std::pin::Pin<&mut Self>,
        mut self: std::pin::Pin<&mut VolumeIoWriteRequest>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut me = self.as_mut();
        Pin::new(&mut me.rx)
            .poll(cx)
            .map(|r| r.unwrap_or_else(|_e| Err(tonic::Status::aborted("Request not received"))))
    }
}

#[derive(Debug)]
pub struct VolumeIoReadRequest {
    rx: oneshot::Receiver<Result<Bytes, tonic::Status>>,
}

impl Future for VolumeIoReadRequest {
    type Output = Result<Bytes, tonic::Status>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut me = self.as_mut();
        Pin::new(&mut me.rx)
            .poll(cx)
            .map(|r| r.unwrap_or_else(|_e| Err(tonic::Status::aborted("Request not received"))))
    }
}

#[derive(Debug)]
pub struct VolumeIoFlushRequest {
    rx: oneshot::Receiver<Result<(), tonic::Status>>,
}

impl Future for VolumeIoFlushRequest {
    type Output = Result<(), tonic::Status>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut me = self.as_mut();
        Pin::new(&mut me.rx)
            .poll(cx)
            .map(|r| r.unwrap_or_else(|_e| Err(tonic::Status::aborted("Request not received"))))
    }
}

#[derive(Debug)]
enum Outstanding {
    Write(oneshot::Sender<Result<u64, tonic::Status>>),
    Read(oneshot::Sender<Result<Bytes, tonic::Status>>),
    Flush(oneshot::Sender<Result<(), tonic::Status>>),
}

impl Outstanding {
    fn fail(self, status: tonic::Status) {
        match self {
            Outstanding::Write(tx) => {
                let _ = tx.send(Err(status));
            }
            Outstanding::Read(tx) => {
                let _ = tx.send(Err(status));
            }
            Outstanding::Flush(tx) => {
                let _ = tx.send(Err(status));
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct VolumeIo {
    requests: mpsc::Sender<(VolumeIoRequest, Outstanding)>,
}

impl VolumeIo {
    fn new(
        requests: mpsc::Sender<(VolumeIoRequest, Outstanding)>,
        replies: tonic::Streaming<VolumeIoReply>,
        outstanding_rx: mpsc::UnboundedReceiver<Outstanding>,
    ) -> Self {
        tokio::spawn(Self::monitor_task(replies, outstanding_rx));

        Self { requests }
    }

    async fn monitor_task(
        mut replies: tonic::Streaming<VolumeIoReply>,
        mut outstanding: mpsc::UnboundedReceiver<Outstanding>,
    ) {
        while let Some(outstanding) = outstanding.recv().await {
            match replies.message().await {
                Ok(Some(reply)) => {
                    let Some(reply) = reply.reply else {
                        outstanding.fail(tonic::Status::internal("Reply didn't have reply data"));
                        continue;
                    };

                    match (outstanding, reply) {
                        (Outstanding::Read(tx), volume_io_reply::Reply::Read(r)) => {
                            let _ = tx.send(Ok(r.data));
                        }
                        (Outstanding::Write(tx), volume_io_reply::Reply::Write(w)) => {
                            let _ = tx.send(Ok(w.written));
                        }
                        (Outstanding::Flush(tx), volume_io_reply::Reply::Flush(_f)) => {
                            let _ = tx.send(Ok(()));
                        }
                        (_o, _r) => {
                            warn!("Unmatched request and reply");
                            break;
                        }
                    }
                }
                Ok(None) => {
                    warn!("Reply stream closed while there were still request outstanding");
                    break;
                }
                Err(s) => outstanding.fail(s),
            }
        }
    }

    async fn send_request(
        &mut self,
        request: (VolumeIoRequest, Outstanding),
    ) -> Result<(), VolumeIoNoMoreRequests> {
        self.requests
            .send(request)
            .await
            .map_err(|_| VolumeIoNoMoreRequests())?;
        Ok(())
    }

    fn prepare_write(
        data: Bytes,
        offset: u64,
    ) -> ((VolumeIoRequest, Outstanding), VolumeIoWriteRequest) {
        let (tx, rx) = oneshot::channel();
        let request = VolumeIoRequest {
            target_or_request: Some(volume_io_request::TargetOrRequest::Write(VolumeIoWrite {
                data,
                offset,
            })),
        };
        let outstanding = Outstanding::Write(tx);

        ((request, outstanding), VolumeIoWriteRequest { rx })
    }

    pub async fn request_write(
        &mut self,
        data: Bytes,
        offset: u64,
    ) -> Result<VolumeIoWriteRequest, VolumeIoNoMoreRequests> {
        let (request, write) = Self::prepare_write(data, offset);
        self.send_request(request).await?;
        Ok(write)
    }

    fn prepare_read(
        length: u64,
        offset: u64,
    ) -> ((VolumeIoRequest, Outstanding), VolumeIoReadRequest) {
        let (tx, rx) = oneshot::channel();
        let request = VolumeIoRequest {
            target_or_request: Some(volume_io_request::TargetOrRequest::Read(VolumeIoRead {
                length,
                offset,
            })),
        };

        let outstanding = Outstanding::Read(tx);
        ((request, outstanding), VolumeIoReadRequest { rx })
    }

    pub async fn request_read(
        &mut self,
        length: u64,
        offset: u64,
    ) -> Result<VolumeIoReadRequest, VolumeIoNoMoreRequests> {
        let (request, read) = Self::prepare_read(length, offset);
        self.send_request(request).await?;
        Ok(read)
    }

    fn prepare_flush() -> ((VolumeIoRequest, Outstanding), VolumeIoFlushRequest) {
        let (tx, rx) = oneshot::channel();
        let request = VolumeIoRequest {
            target_or_request: Some(volume_io_request::TargetOrRequest::Flush(VolumeIoFlush {})),
        };

        let outstanding = Outstanding::Flush(tx);
        ((request, outstanding), VolumeIoFlushRequest { rx })
    }

    pub async fn request_flush(&mut self) -> Result<VolumeIoFlushRequest, VolumeIoNoMoreRequests> {
        let (request, flush) = Self::prepare_flush();
        self.send_request(request).await?;
        Ok(flush)
    }
}

type ReservedRequestPermit = mpsc::OwnedPermit<(VolumeIoRequest, Outstanding)>;
enum IoWrapperState {
    Idle,
    ReserveRequest(BoxFuture<'static, Result<ReservedRequestPermit, mpsc::error::SendError<()>>>),
    Flush(VolumeIoFlushRequest),
    Read(VolumeIoReadRequest),
}

pub struct VolumeIoRW {
    io: VolumeIo,
    info: VolumeTarget,
    // Position in the volume as seen by the user
    pos: u64,
    state: IoWrapperState,
    pending_seek: Option<std::io::SeekFrom>,
    outstanding_writes: VecDeque<VolumeIoWriteRequest>,
    buffered_read: Bytes,
}

impl VolumeIoRW {
    pub fn new(io: VolumeIo, info: VolumeTarget) -> Self {
        Self {
            io,
            info,
            pos: 0,
            state: IoWrapperState::Idle,
            pending_seek: None,
            outstanding_writes: VecDeque::new(),
            buffered_read: Bytes::new(),
        }
    }

    pub fn readable(&self) -> bool {
        self.info.readable
    }

    pub fn writable(&self) -> bool {
        self.info.writable
    }

    pub fn seekable(&self) -> bool {
        self.info.seekable
    }

    pub fn blocksize(&self) -> Option<u32> {
        self.info.blocksize
    }

    pub fn size(&self) -> Option<u64> {
        self.info.size
    }

    fn cleanup_outstanding_writes(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> Result<(), std::io::Error> {
        // Clean up outstanding writes
        while let Some(o) = self.outstanding_writes.front_mut() {
            match Pin::new(o).poll(cx) {
                Poll::Ready(r) => {
                    // TODO handle short writes?
                    self.outstanding_writes.pop_front();
                    r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                }
                Poll::Pending => break,
            }
        }
        Ok(())
    }

    fn copy_buffered_read(&mut self, buf: &mut tokio::io::ReadBuf<'_>) -> bool {
        if !self.buffered_read.is_empty() {
            let available = buf.remaining().min(self.buffered_read.len());
            let tocopy = self.buffered_read.split_to(available);
            buf.put_slice(&tocopy);
            self.pos = self.pos.saturating_add(tocopy.len() as u64);
            true
        } else {
            false
        }
    }

    fn invalidate_read_buffer(&mut self) {
        self.buffered_read.clear();
    }
}

impl AsyncWrite for VolumeIoRW {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        if !self.writable() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Target doesn't support writing",
            )));
        }
        let mut me = self.as_mut();

        me.invalidate_read_buffer();
        if let Err(e) = me.cleanup_outstanding_writes(cx) {
            return Poll::Ready(Err(e));
        }

        loop {
            match me.state {
                IoWrapperState::Idle => {
                    let reserve = me.io.requests.clone().reserve_owned();
                    me.state = IoWrapperState::ReserveRequest(reserve.boxed());
                }
                IoWrapperState::ReserveRequest(ref mut r) => match ready!(r.as_mut().poll(cx)) {
                    Ok(p) => {
                        // Limit size to 3 MB to stay below the default max 4MB gRPC message size
                        let max_write_size = buf.len().min(3 * 1024 * 1024);
                        let len = if let Some(blocksize) = me.blocksize() {
                            let blocksize = blocksize as usize;
                            if max_write_size > blocksize {
                                (max_write_size / blocksize) * blocksize
                            } else {
                                // TODO cache?
                                max_write_size
                            }
                        } else {
                            max_write_size
                        };
                        let bytes = Bytes::copy_from_slice(&buf[0..len]);
                        let (request, write) = VolumeIo::prepare_write(bytes, me.pos);
                        me.outstanding_writes.push_back(write);
                        p.send(request);

                        me.pos = me.pos.saturating_add(len as u64);
                        me.state = IoWrapperState::Idle;

                        // Error will be retrieved by next writes
                        return Poll::Ready(Ok(len));
                    }
                    Err(e) => {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::NotConnected,
                            e,
                        )))
                    }
                },
                IoWrapperState::Flush(_) | IoWrapperState::Read(_) => {
                    // Drop the outstanding flush and read
                    me.state = IoWrapperState::Idle;
                }
            }
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let mut me = self.as_mut();

        if let Err(e) = me.cleanup_outstanding_writes(cx) {
            return Poll::Ready(Err(e));
        }

        if me.outstanding_writes.is_empty() {
            return Poll::Ready(Ok(()));
        }

        loop {
            match me.state {
                IoWrapperState::Idle => {
                    let reserve = me.io.requests.clone().reserve_owned();
                    me.state = IoWrapperState::ReserveRequest(reserve.boxed());
                }
                IoWrapperState::ReserveRequest(ref mut r) => match ready!(r.as_mut().poll(cx)) {
                    Ok(p) => {
                        let (request, flush) = VolumeIo::prepare_flush();
                        me.state = IoWrapperState::Flush(flush);
                        p.send(request);
                    }
                    Err(e) => {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::NotConnected,
                            e,
                        )))
                    }
                },
                IoWrapperState::Flush(ref mut flush) => {
                    let res = ready!(Pin::new(flush).poll(cx));
                    me.state = IoWrapperState::Idle;
                    let res = res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
                    return Poll::Ready(res);
                }
                IoWrapperState::Read(_) => {
                    // Drop outstanding read
                    me.state = IoWrapperState::Idle;
                }
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        self.poll_flush(cx)
    }
}

impl AsyncRead for VolumeIoRW {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.readable() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Target doesn't support reading",
            )));
        }
        let mut me = self.as_mut();

        if me.copy_buffered_read(buf) {
            return Poll::Ready(Ok(()));
        }

        loop {
            match me.state {
                IoWrapperState::Idle => {
                    let reserve = me.io.requests.clone().reserve_owned();
                    me.state = IoWrapperState::ReserveRequest(reserve.boxed());
                }
                IoWrapperState::ReserveRequest(ref mut r) => match ready!(r.as_mut().poll(cx)) {
                    Ok(p) => {
                        let len = buf.remaining();
                        let (request, read) = VolumeIo::prepare_read(len as u64, me.pos);
                        me.state = IoWrapperState::Read(read);
                        p.send(request);
                    }
                    Err(e) => {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::NotConnected,
                            e,
                        )))
                    }
                },
                IoWrapperState::Read(ref mut read) => {
                    let res = ready!(Pin::new(read).poll(cx));
                    me.state = IoWrapperState::Idle;
                    match res {
                        Ok(b) => {
                            me.buffered_read = b;
                            me.copy_buffered_read(buf);
                            return Poll::Ready(Ok(()));
                        }
                        Err(e) => {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                e,
                            )))
                        }
                    }
                }
                IoWrapperState::Flush(_) => {
                    // Drop the previous flush
                    me.state = IoWrapperState::Idle;
                }
            }
        }
    }
}

impl AsyncSeek for VolumeIoRW {
    fn start_seek(mut self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        if self.seekable() {
            self.pending_seek = Some(position);
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Target doesn't support seek",
            ))
        }
    }

    fn poll_complete(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<u64>> {
        if !self.seekable() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Target doesn't support seek",
            )));
        }
        if let Some(position) = self.pending_seek.take() {
            let (base, offset) = match position {
                std::io::SeekFrom::Start(s) => (s, 0),
                std::io::SeekFrom::End(e) => {
                    if let Some(size) = self.size() {
                        (size, e)
                    } else {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "Target has no known size",
                        )));
                    }
                }
                std::io::SeekFrom::Current(offset) => (self.pos, offset),
            };

            if offset > 0 {
                let offset = offset as u64;
                self.pos = base.saturating_add(offset);
            } else {
                let offset = offset.unsigned_abs();
                self.pos = base.saturating_sub(offset)
            }
        }

        Poll::Ready(Ok(self.pos))
    }
}
