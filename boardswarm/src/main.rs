use anyhow::{bail, Context};
use boardswarm_protocol::item_event::Event;
use boardswarm_protocol::{
    console_input_request, volume_io_reply, volume_io_request, ConsoleConfigureRequest,
    ConsoleInputRequest, ConsoleOutputRequest, ItemEvent, ItemList, ItemPropertiesMsg,
    ItemPropertiesRequest, ItemTypeRequest, LoginInfoList, Property, VolumeEraseRequest,
    VolumeInfoMsg, VolumeIoTargetReply, VolumeRequest,
};
use bytes::Bytes;
use clap::Parser;
use futures::prelude::*;
use futures::stream::BoxStream;
use futures::Sink;
use jwt_authorizer::{Authorizer, IntoLayer, JwtAuthorizer, RegisteredClaims, Validation};
use mediatek_brom::MediatekBromProvider;
use qcomlt_debug_board::QCOMLTDebugBoardProvider;
use registry::{Properties, Registry};
use std::net::{AddrParseError, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tracing::{info, instrument, warn};

mod boardswarm_provider;
mod config;
mod config_device;
mod dfu;
mod fastboot;
mod gpio;
mod mediatek_brom;
mod pdudaemon;
mod qcomlt_debug_board;
mod registry;
mod rockusb;
mod serial;
mod udev;
mod utils;

#[derive(Error, Debug)]
#[error("Actuator failed")]
pub struct ActuatorError();

#[async_trait::async_trait]
trait Actuator: std::fmt::Debug + Send + Sync {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), ActuatorError>;
}

#[derive(Error, Debug)]
pub enum ConsoleError {
    #[error("Console was closed")]
    Closed,
}

#[async_trait::async_trait]
trait Console: std::fmt::Debug + Send + Sync {
    fn configure(
        &self,
        parameters: Box<dyn erased_serde::Deserializer>,
    ) -> Result<(), ConsoleError>;
    async fn input(
        &self,
    ) -> Result<Pin<Box<dyn Sink<Bytes, Error = ConsoleError> + Send>>, ConsoleError>;
    async fn output(&self)
        -> Result<BoxStream<'static, Result<Bytes, ConsoleError>>, ConsoleError>;
}

type ConsoleOutputStream =
    stream::BoxStream<'static, Result<boardswarm_protocol::ConsoleOutput, tonic::Status>>;

#[async_trait::async_trait]
trait ConsoleExt: Console {
    async fn output_stream(&self) -> ConsoleOutputStream {
        Box::pin(self.output().await.unwrap().map(|data| {
            Ok(boardswarm_protocol::ConsoleOutput {
                data: data.unwrap(),
            })
        }))
    }
}

impl<C> ConsoleExt for C where C: Console + ?Sized {}

#[derive(Clone, Error, Debug)]
pub enum VolumeError {
    #[error("Unknown target requested")]
    UnknownTargetRequested,
    #[error("Internal error: {0}")]
    Internal(String),
    #[error("Volume failure: {0}")]
    Failure(String),
}

impl From<VolumeError> for tonic::Status {
    fn from(e: VolumeError) -> Self {
        match e {
            VolumeError::UnknownTargetRequested => tonic::Status::not_found(e.to_string()),
            VolumeError::Internal(e) => tonic::Status::internal(e),
            VolumeError::Failure(e) => tonic::Status::aborted(e),
        }
    }
}

type VolumeIoReplyStream =
    ReceiverStream<Result<boardswarm_protocol::VolumeIoReply, tonic::Status>>;

enum VolumeIoReply {
    Target(VolumeTargetInfo),
    Read(oneshot::Receiver<Result<Bytes, tonic::Status>>),
    Write(oneshot::Receiver<Result<u64, tonic::Status>>),
    Flush(oneshot::Receiver<Result<(), tonic::Status>>),
    Shutdown(oneshot::Receiver<Result<(), tonic::Status>>),
    FatalError(tonic::Status),
}

pub struct VolumeIoReplies {
    completion_tx: tokio::sync::mpsc::UnboundedSender<VolumeIoReply>,
}

impl VolumeIoReplies {
    fn new() -> (Self, VolumeIoReplyStream) {
        let (reply_tx, reply_rx) = mpsc::channel(8);
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Some(completion) = completion_rx.recv().await {
                let reply = match completion {
                    VolumeIoReply::Target(t) => Ok(boardswarm_protocol::VolumeIoReply {
                        reply: Some(volume_io_reply::Reply::Target(VolumeIoTargetReply {
                            target: Some(t),
                        })),
                    }),
                    VolumeIoReply::Read(r) => {
                        let Ok(r) = r.await else { break };
                        r.map(|data| boardswarm_protocol::VolumeIoReply {
                            reply: Some(volume_io_reply::Reply::Read(
                                boardswarm_protocol::VolumeIoReadReply { data },
                            )),
                        })
                    }
                    VolumeIoReply::Write(w) => {
                        let Ok(w) = w.await else { break };
                        w.map(|written| boardswarm_protocol::VolumeIoReply {
                            reply: Some(volume_io_reply::Reply::Write(
                                boardswarm_protocol::VolumeIoWriteReply { written },
                            )),
                        })
                    }
                    VolumeIoReply::Flush(f) => {
                        let Ok(f) = f.await else { break };
                        f.map(|_| boardswarm_protocol::VolumeIoReply {
                            reply: Some(volume_io_reply::Reply::Flush(
                                boardswarm_protocol::VolumeIoFlushReply {},
                            )),
                        })
                    }
                    VolumeIoReply::Shutdown(s) => {
                        let Ok(s) = s.await else { break };
                        s.map(|_| boardswarm_protocol::VolumeIoReply {
                            reply: Some(volume_io_reply::Reply::Shutdown(
                                boardswarm_protocol::VolumeIoShutdownReply {},
                            )),
                        })
                    }
                    VolumeIoReply::FatalError(e) => Err(e),
                };
                if reply_tx.send(reply).await.is_err() {
                    break;
                };
            }
        });
        (Self { completion_tx }, ReceiverStream::new(reply_rx))
    }

    fn enqueue_target_reply(&mut self, info: VolumeTargetInfo) {
        let _ = self.completion_tx.send(VolumeIoReply::Target(info));
    }

    fn enqueue_write_reply(&mut self, rx: oneshot::Receiver<Result<u64, tonic::Status>>) {
        let _ = self.completion_tx.send(VolumeIoReply::Write(rx));
    }

    fn enqueue_read_reply(&mut self, rx: oneshot::Receiver<Result<Bytes, tonic::Status>>) {
        let _ = self.completion_tx.send(VolumeIoReply::Read(rx));
    }

    fn enqueue_flush_reply(&mut self, rx: oneshot::Receiver<Result<(), tonic::Status>>) {
        let _ = self.completion_tx.send(VolumeIoReply::Flush(rx));
    }

    fn enqueue_shutdown_reply(&mut self, rx: oneshot::Receiver<Result<(), tonic::Status>>) {
        let _ = self.completion_tx.send(VolumeIoReply::Shutdown(rx));
    }

    fn enqueue_fatal_error(&mut self, error: tonic::Status) {
        let _ = self.completion_tx.send(VolumeIoReply::FatalError(error));
    }
}

type VolumeTargetInfo = boardswarm_protocol::VolumeTarget;
#[async_trait::async_trait]
pub trait Volume: std::fmt::Debug + Send + Sync {
    /// List of known targets and whether it's exhaustive
    fn targets(&self) -> (&[VolumeTargetInfo], bool);
    async fn open(
        &self,
        target: &str,
        length: Option<u64>,
    ) -> Result<(VolumeTargetInfo, Box<dyn VolumeTarget>), VolumeError>;
    async fn commit(&self) -> Result<(), VolumeError>;
    async fn erase(&self, _target: &str) -> Result<(), VolumeError> {
        Err(VolumeError::UnknownTargetRequested)
    }
}

pub struct ReadCompletion(oneshot::Sender<Result<Bytes, tonic::Status>>);
impl ReadCompletion {
    fn new() -> (Self, oneshot::Receiver<Result<Bytes, tonic::Status>>) {
        let (tx, rx) = oneshot::channel();
        (Self(tx), rx)
    }
    pub fn complete(self, result: Result<Bytes, tonic::Status>) {
        let _ = self.0.send(result);
    }
}

pub struct WriteCompletion(oneshot::Sender<Result<u64, tonic::Status>>);
impl WriteCompletion {
    fn new() -> (Self, oneshot::Receiver<Result<u64, tonic::Status>>) {
        let (tx, rx) = oneshot::channel();
        (Self(tx), rx)
    }
    pub fn complete(self, result: Result<u64, tonic::Status>) {
        let _ = self.0.send(result);
    }
}

pub struct FlushCompletion(oneshot::Sender<Result<(), tonic::Status>>);
impl FlushCompletion {
    fn new() -> (Self, oneshot::Receiver<Result<(), tonic::Status>>) {
        let (tx, rx) = oneshot::channel();
        (Self(tx), rx)
    }
    pub fn complete(self, result: Result<(), tonic::Status>) {
        let _ = self.0.send(result);
    }
}

pub struct ShutdownCompletion(oneshot::Sender<Result<(), tonic::Status>>);
impl ShutdownCompletion {
    fn new() -> (Self, oneshot::Receiver<Result<(), tonic::Status>>) {
        let (tx, rx) = oneshot::channel();
        (Self(tx), rx)
    }
    pub fn complete(self, result: Result<(), tonic::Status>) {
        let _ = self.0.send(result);
    }
}

#[async_trait::async_trait]
pub trait VolumeTarget: Send {
    async fn read(&mut self, _length: u64, _offset: u64, completion: ReadCompletion) {
        completion.complete(Err(tonic::Status::unimplemented("Target is not readable")));
    }

    async fn write(&mut self, _data: Bytes, _offset: u64, completion: WriteCompletion) {
        completion.complete(Err(tonic::Status::unimplemented("Target is not writable")));
    }

    async fn flush(&mut self, completion: FlushCompletion) {
        completion.complete(Ok(()))
    }

    async fn shutdown(&mut self, completion: ShutdownCompletion) {
        // Take advantage of flush and shutdown returning an result, so we can convert one into
        // the other
        let rx = completion.0;
        let completion = FlushCompletion(rx);
        self.flush(completion).await
    }
}

trait DeviceConfigItem {
    fn matches(&self, properties: &Properties) -> bool;
}

impl DeviceConfigItem for config::Console {
    #[instrument(fields(name = self.name), skip_all, level="error")]
    fn matches(&self, properties: &Properties) -> bool {
        if self.match_.is_empty() {
            warn!("Console matches is empty - will match any console");
        }
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::Volume {
    #[instrument(fields(name = self.name), skip_all, level="error")]
    fn matches(&self, properties: &Properties) -> bool {
        if self.match_.is_empty() {
            warn!("Volume matches is empty - will match any volume");
        }
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::ModeStep {
    #[instrument(skip_all, level = "error")]
    fn matches(&self, properties: &Properties) -> bool {
        if self.match_.is_empty() {
            warn!("ModeStep matches is empty - will match any device");
        }
        properties.matches(&self.match_)
    }
}

impl From<&dyn Device> for boardswarm_protocol::Device {
    fn from(d: &dyn Device) -> Self {
        let consoles = d
            .consoles()
            .into_iter()
            .map(|c| boardswarm_protocol::Console {
                name: c.name,
                id: c.id,
            })
            .collect();
        let volumes = d
            .volumes()
            .into_iter()
            .map(|v| boardswarm_protocol::Volume {
                name: v.name,
                id: v.id,
            })
            .collect();
        let modes = d
            .modes()
            .into_iter()
            .map(|m| boardswarm_protocol::Mode {
                name: m.name,
                depends: m.depends,
                available: m.available,
            })
            .collect();
        let current_mode = d.current_mode();
        boardswarm_protocol::Device {
            consoles,
            volumes,
            current_mode,
            modes,
        }
    }
}

#[derive(Debug, Error)]
#[error("Device is no longer there")]
struct DeviceGone();
#[derive(Debug, Error)]
enum DeviceSetModeError {
    #[error("Mode not found")]
    ModeNotFound,
    #[error("Wrong current mode")]
    WrongCurrentMode,
    #[error("Actuator failed: {0}")]
    ActuatorFailed(#[from] ActuatorError),
}

struct DeviceMonitor {
    receiver: broadcast::Receiver<()>,
}

impl DeviceMonitor {
    async fn wait(&mut self) -> Result<(), DeviceGone> {
        while let Err(e) = self.receiver.recv().await {
            match e {
                broadcast::error::RecvError::Closed => return Err(DeviceGone()),
                broadcast::error::RecvError::Lagged(_) => continue,
            }
        }
        Ok(())
    }
}

struct DeviceConsole {
    name: String,
    id: Option<u64>,
}

struct DeviceVolume {
    name: String,
    id: Option<u64>,
}

struct DeviceMode {
    name: String,
    depends: Option<String>,
    available: bool,
}

#[async_trait::async_trait]
trait Device: Send + Sync {
    async fn set_mode(&self, mode: &str) -> Result<(), DeviceSetModeError>;
    fn updates(&self) -> DeviceMonitor;
    fn consoles(&self) -> Vec<DeviceConsole>;
    fn volumes(&self) -> Vec<DeviceVolume>;
    fn modes(&self) -> Vec<DeviceMode>;
    fn current_mode(&self) -> Option<String>;
}

struct ServerInner {
    config_dir: PathBuf,
    auth_info: Vec<config::Authentication>,
    devices: Registry<Arc<dyn Device>>,
    consoles: Registry<Arc<dyn Console>>,
    actuators: Registry<Arc<dyn Actuator>>,
    volumes: Registry<Arc<dyn Volume>>,
}

fn to_item_list<T: Clone>(registry: &Registry<T>) -> ItemList {
    let item = registry
        .contents()
        .into_iter()
        .map(|(id, item)| boardswarm_protocol::Item {
            id,
            name: item.properties().name().to_string(),
            instance: item.properties().instance().map(ToOwned::to_owned),
        })
        .collect();
    ItemList { item }
}

#[derive(Clone)]
pub struct Server {
    inner: Arc<ServerInner>,
}

impl Server {
    fn new(auth_info: Vec<config::Authentication>, config_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(ServerInner {
                auth_info,
                config_dir,
                consoles: Registry::new(),
                devices: Registry::new(),
                actuators: Registry::new(),
                volumes: Registry::new(),
            }),
        }
    }

    fn config_dir(&self) -> &Path {
        &self.inner.config_dir
    }

    fn register_actuator<A>(&self, properties: Properties, actuator: A) -> u64
    where
        A: Actuator + 'static,
    {
        let (id, item) = self.inner.actuators.add(properties, Arc::new(actuator));
        info!("Registered actuator: {} - {}", id, item);
        id
    }

    fn get_actuator(&self, id: u64) -> Option<Arc<dyn Actuator>> {
        self.inner
            .actuators
            .lookup(id)
            .map(|item| item.inner().clone())
    }

    fn find_actuator<'a, K, V, I>(&self, matches: &'a I) -> Option<Arc<dyn Actuator>>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        &'a I: IntoIterator<Item = (K, V)>,
    {
        self.inner
            .actuators
            .find(matches)
            .map(|(_, item)| item.inner().clone())
    }

    fn unregister_actuator(&self, id: u64) {
        if let Some(item) = self.inner.actuators.lookup(id) {
            info!("Unregistering actuator: {} - {}", id, item);
            self.inner.actuators.remove(id);
        }
    }

    fn register_console<C>(&self, properties: Properties, console: C) -> u64
    where
        C: Console + 'static,
    {
        let (id, item) = self.inner.consoles.add(properties, Arc::new(console));
        info!("Registered console: {} - {}", id, item);
        id
    }

    fn unregister_console(&self, id: u64) {
        if let Some(item) = self.inner.consoles.lookup(id) {
            info!("Unregistering console: {} - {}", id, item);
            self.inner.consoles.remove(id);
        }
    }

    fn get_console(&self, id: u64) -> Option<Arc<dyn Console>> {
        self.inner
            .consoles
            .lookup(id)
            .map(|item| item.inner().clone())
    }

    fn register_volume<V>(&self, properties: Properties, volume: V) -> u64
    where
        V: Volume + 'static,
    {
        let (id, item) = self.inner.volumes.add(properties, Arc::new(volume));
        info!("Registered volume: {} - {}", id, item);
        id
    }

    fn unregister_volume(&self, id: u64) {
        if let Some(item) = self.inner.volumes.lookup(id) {
            info!("Unregistering volume: {} - {}", id, item.name());
            self.inner.volumes.remove(id);
        }
    }

    pub fn get_volume(&self, id: u64) -> Option<Arc<dyn Volume>> {
        self.inner
            .volumes
            .lookup(id)
            .map(registry::Item::into_inner)
    }

    fn register_device<D>(&self, properties: Properties, device: D) -> u64
    where
        D: Device + 'static,
    {
        let (id, item) = self.inner.devices.add(properties, Arc::new(device));
        info!("Registered device: {} - {}", id, item);
        id
    }

    fn unregister_device(&self, id: u64) {
        if let Some(item) = self.inner.devices.lookup(id) {
            info!("Unregistering device: {} - {}", id, item.name());
            self.inner.devices.remove(id);
        }
    }

    fn get_device(&self, id: u64) -> Option<Arc<dyn Device>> {
        self.inner
            .devices
            .lookup(id)
            .map(registry::Item::into_inner)
    }

    fn item_list_for(&self, type_: boardswarm_protocol::ItemType) -> ItemList {
        match type_ {
            boardswarm_protocol::ItemType::Actuator => to_item_list(&self.inner.actuators),
            boardswarm_protocol::ItemType::Device => to_item_list(&self.inner.devices),
            boardswarm_protocol::ItemType::Console => to_item_list(&self.inner.consoles),
            boardswarm_protocol::ItemType::Volume => to_item_list(&self.inner.volumes),
        }
    }
}

type ItemMonitorStream = BoxStream<'static, Result<boardswarm_protocol::ItemEvent, tonic::Status>>;

#[async_trait::async_trait]
impl boardswarm_protocol::boardswarm_server::Boardswarm for Server {
    async fn login_info(
        &self,
        _request: tonic::Request<()>,
    ) -> Result<tonic::Response<LoginInfoList>, tonic::Status> {
        let info = self
            .inner
            .auth_info
            .iter()
            .filter_map(|a| match a {
                config::Authentication::Oidc {
                    description,
                    uri,
                    client,
                    ..
                } => Some(boardswarm_protocol::LoginInfo {
                    description: description.clone(),
                    method: Some(boardswarm_protocol::login_info::Method::Oidc(
                        boardswarm_protocol::OidcInfo {
                            url: uri.clone(),
                            client_id: client.clone(),
                        },
                    )),
                }),
                config::Authentication::Jwks { .. } => None,
            })
            .collect();
        Ok(tonic::Response::new(LoginInfoList { info }))
    }

    async fn list(
        &self,
        request: tonic::Request<ItemTypeRequest>,
    ) -> Result<tonic::Response<ItemList>, tonic::Status> {
        let request = request.into_inner();
        let type_ = request
            .r#type
            .try_into()
            .map_err(|_e| tonic::Status::invalid_argument("Unknown item type "))?;

        Ok(tonic::Response::new(self.item_list_for(type_)))
    }

    type MonitorStream = ItemMonitorStream;
    async fn monitor(
        &self,
        request: tonic::Request<ItemTypeRequest>,
    ) -> Result<tonic::Response<Self::MonitorStream>, tonic::Status> {
        let request = request.into_inner();
        let type_ = request
            .r#type
            .try_into()
            .map_err(|_e| tonic::Status::invalid_argument("Unknown item type "))?;

        fn to_item_stream<T>(registry: &Registry<T>) -> ItemMonitorStream
        where
            T: Clone + Send + 'static,
        {
            let monitor = registry.monitor();
            let initial = Ok(ItemEvent {
                event: Some(Event::Add(to_item_list(registry))),
            });
            stream::once(async move { initial })
                .chain(stream::unfold(monitor, |mut monitor| async move {
                    let event = monitor.recv().await.ok()?;
                    match event {
                        registry::RegistryChange::Added { id, item } => Some((
                            Ok(ItemEvent {
                                event: Some(Event::Add(ItemList {
                                    item: vec![boardswarm_protocol::Item {
                                        id,
                                        name: item.name().to_string(),
                                        instance: item
                                            .properties()
                                            .instance()
                                            .map(ToOwned::to_owned),
                                    }],
                                })),
                            }),
                            monitor,
                        )),
                        registry::RegistryChange::Removed(removed) => Some((
                            Ok(boardswarm_protocol::ItemEvent {
                                event: Some(Event::Remove(removed)),
                            }),
                            monitor,
                        )),
                    }
                }))
                .boxed()
        }
        let response = match type_ {
            boardswarm_protocol::ItemType::Actuator => to_item_stream(&self.inner.actuators),
            boardswarm_protocol::ItemType::Device => to_item_stream(&self.inner.devices),
            boardswarm_protocol::ItemType::Console => to_item_stream(&self.inner.consoles),
            boardswarm_protocol::ItemType::Volume => to_item_stream(&self.inner.volumes),
        };
        Ok(tonic::Response::new(response))
    }

    async fn item_properties(
        &self,
        request: tonic::Request<ItemPropertiesRequest>,
    ) -> Result<tonic::Response<ItemPropertiesMsg>, tonic::Status> {
        let request = request.into_inner();
        let type_ = request
            .r#type
            .try_into()
            .map_err(|_e| tonic::Status::invalid_argument("Unknown item type "))?;
        let properties = match type_ {
            boardswarm_protocol::ItemType::Actuator => self
                .inner
                .actuators
                .lookup(request.item)
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Device => self
                .inner
                .devices
                .lookup(request.item)
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Console => self
                .inner
                .consoles
                .lookup(request.item)
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Volume => self
                .inner
                .volumes
                .lookup(request.item)
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
        };

        let properties = properties
            .iter()
            .map(|(k, v)| Property {
                key: k.clone(),
                value: v.clone(),
            })
            .collect();

        Ok(tonic::Response::new(ItemPropertiesMsg {
            property: properties,
        }))
    }

    async fn console_configure(
        &self,
        request: tonic::Request<ConsoleConfigureRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(console) = self.get_console(inner.console) {
            console
                .configure(Box::new(<dyn erased_serde::Deserializer>::erase(
                    inner.parameters.unwrap(),
                )))
                .unwrap();
            Ok(tonic::Response::new(()))
        } else {
            Err(tonic::Status::invalid_argument("Can't find console"))
        }
    }

    type ConsoleStreamOutputStream = ConsoleOutputStream;
    async fn console_stream_output(
        &self,
        request: tonic::Request<ConsoleOutputRequest>,
    ) -> Result<tonic::Response<Self::ConsoleStreamOutputStream>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(console) = self.get_console(inner.console) {
            Ok(tonic::Response::new(console.output_stream().await))
        } else {
            Err(tonic::Status::invalid_argument("Can't find output console"))
        }
    }

    async fn console_stream_input(
        &self,
        request: tonic::Request<Streaming<ConsoleInputRequest>>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let mut rx = request.into_inner();

        /* First message must select the target */
        let msg = match rx.message().await? {
            Some(msg) => msg,
            None => return Ok(tonic::Response::new(())),
        };
        let console = if let Some(console_input_request::TargetOrData::Console(console)) =
            msg.target_or_data
        {
            self.get_console(console)
                .ok_or_else(|| tonic::Status::not_found("No serial console by that name"))?
        } else {
            return Err(tonic::Status::invalid_argument(
                "Target should be set first",
            ));
        };

        let mut input = console.input().await.unwrap();
        while let Some(request) = rx.message().await? {
            match request.target_or_data {
                Some(console_input_request::TargetOrData::Data(data)) => {
                    input.send(data).await.unwrap()
                }
                _ => return Err(tonic::Status::invalid_argument("Target cannot be changed")),
            }
        }
        Ok(tonic::Response::new(()))
    }

    type DeviceInfoStream = BoxStream<'static, Result<boardswarm_protocol::Device, tonic::Status>>;
    async fn device_info(
        &self,
        request: tonic::Request<boardswarm_protocol::DeviceRequest>,
    ) -> Result<tonic::Response<Self::DeviceInfoStream>, tonic::Status> {
        let request = request.into_inner();
        if let Some(item) = self.inner.devices.lookup(request.device) {
            let device = item.into_inner();
            let info = (&*device).into();
            let monitor = device.updates();
            let stream = Box::pin(stream::once(async move { Ok(info) }).chain(stream::unfold(
                (device, monitor),
                |(device, mut monitor)| async move {
                    monitor.wait().await.ok()?;
                    let info = (&*device).into();
                    Some((Ok(info), (device, monitor)))
                },
            )));
            Ok(tonic::Response::new(stream))
        } else {
            Err(tonic::Status::not_found("No such device"))
        }
    }

    async fn device_change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::DeviceModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        if let Some(device) = self.get_device(request.device) {
            match device.set_mode(&request.mode).await {
                Ok(()) => Ok(tonic::Response::new(())),
                Err(DeviceSetModeError::ModeNotFound) => {
                    Err(tonic::Status::not_found("No mode by that name"))
                }
                Err(DeviceSetModeError::WrongCurrentMode) => Err(
                    tonic::Status::failed_precondition("Not in the right mode to switch"),
                ),
                Err(DeviceSetModeError::ActuatorFailed(_)) => {
                    Err(tonic::Status::aborted("Actuator failed"))
                }
            }
        } else {
            Err(tonic::Status::not_found("No device by that id"))
        }
    }

    async fn actuator_change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::ActuatorModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(actuator) = self.get_actuator(inner.actuator) {
            actuator
                .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                    inner.parameters.unwrap(),
                )))
                .await
                .unwrap();
            Ok(tonic::Response::new(()))
        } else {
            Err(tonic::Status::invalid_argument("Can't find actuator"))
        }
    }

    type VolumeIoStream = VolumeIoReplyStream;
    async fn volume_io(
        &self,
        request: tonic::Request<tonic::Streaming<boardswarm_protocol::VolumeIoRequest>>,
    ) -> Result<tonic::Response<Self::VolumeIoStream>, tonic::Status> {
        let mut rx = request.into_inner();
        let msg = match rx.message().await? {
            Some(msg) => msg,
            None => {
                return Err(tonic::Status::invalid_argument(
                    "No uploader/target selection",
                ))
            }
        };

        if let Some(volume_io_request::TargetOrRequest::Target(target)) = msg.target_or_request {
            let volume = self
                .inner
                .volumes
                .lookup(target.volume)
                .map(registry::Item::into_inner)
                .ok_or_else(|| tonic::Status::not_found("No volume by that name"))?;

            let (mut reply, reply_stream) = VolumeIoReplies::new();
            let (info, mut target) = volume.open(&target.target, target.length).await?;
            reply.enqueue_target_reply(info);

            tokio::spawn(async move {
                while let Some(msg) = rx.message().await.transpose() {
                    let request = match msg {
                        Ok(request) => request,
                        Err(e) => {
                            warn!("Received error: {}", e);
                            return;
                        }
                    };

                    let Some(request) = request.target_or_request else {
                        warn!("Invalid request, no actualy request");
                        return;
                    };

                    match request {
                        volume_io_request::TargetOrRequest::Target(_) => {
                            reply.enqueue_fatal_error(tonic::Status::invalid_argument(
                                "Target request sent out of order",
                            ));
                            break;
                        }
                        volume_io_request::TargetOrRequest::Read(read) => {
                            let (completion, rx) = ReadCompletion::new();
                            reply.enqueue_read_reply(rx);
                            target.read(read.length, read.offset, completion).await;
                        }
                        volume_io_request::TargetOrRequest::Write(write) => {
                            let (completion, rx) = WriteCompletion::new();
                            reply.enqueue_write_reply(rx);
                            target.write(write.data, write.offset, completion).await;
                        }
                        volume_io_request::TargetOrRequest::Flush(_f) => {
                            let (completion, rx) = FlushCompletion::new();
                            reply.enqueue_flush_reply(rx);
                            target.flush(completion).await;
                        }
                        volume_io_request::TargetOrRequest::Shutdown(_s) => {
                            let (completion, rx) = ShutdownCompletion::new();
                            reply.enqueue_shutdown_reply(rx);
                            target.shutdown(completion).await;
                        }
                    }
                }
            });

            Ok(tonic::Response::new(reply_stream))
        } else {
            Err(tonic::Status::invalid_argument(
                "Target should be set first",
            ))
        }
    }

    async fn volume_commit(
        &self,
        request: tonic::Request<VolumeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        let volume = self
            .get_volume(request.volume)
            .ok_or_else(|| tonic::Status::not_found("Volume not found"))?;
        volume.commit().await?;
        Ok(tonic::Response::new(()))
    }

    async fn volume_erase(
        &self,
        request: tonic::Request<VolumeEraseRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        let volume = self
            .get_volume(request.volume)
            .ok_or_else(|| tonic::Status::not_found("Volume not found"))?;
        volume.erase(&request.target).await?;
        Ok(tonic::Response::new(()))
    }

    async fn volume_info(
        &self,
        request: tonic::Request<VolumeRequest>,
    ) -> Result<tonic::Response<VolumeInfoMsg>, tonic::Status> {
        let request = request.into_inner();
        let volume = self
            .get_volume(request.volume)
            .ok_or_else(|| tonic::Status::not_found("Volume not found"))?;

        let (target, exhaustive) = volume.targets();

        let info = VolumeInfoMsg {
            target: target.to_vec(),
            exhaustive,
        };
        Ok(tonic::Response::new(info))
    }
}

fn parse_listen_address(addr: &str) -> Result<SocketAddr, AddrParseError> {
    if let Ok(a) = addr.parse() {
        Ok(a)
    } else {
        let ip = addr.parse()?;
        Ok(SocketAddr::new(ip, boardswarm_protocol::DEFAULT_PORT))
    }
}

async fn setup_auth_layer(config: &[config::Authentication]) -> anyhow::Result<Vec<Authorizer>> {
    let mut authorizers = Vec::new();
    for auth in config {
        let a = match auth {
            config::Authentication::Oidc { uri, audience, .. } => {
                let v = Validation::new().aud(audience);
                JwtAuthorizer::<RegisteredClaims>::from_oidc(uri)
                    .validation(v)
                    .build()
                    .await?
            }
            config::Authentication::Jwks { path } => {
                JwtAuthorizer::<RegisteredClaims>::from_jwks(path.to_str().unwrap())
                    .build()
                    .await
                    .context(format!("Failed to load jwks file {}", path.display()))?
            }
        };
        authorizers.push(a);
    }
    Ok(authorizers)
}

#[derive(Debug, clap::Parser)]
struct Opts {
    #[clap(short, long)]
    #[arg(value_parser = parse_listen_address)]
    listen: Option<SocketAddr>,
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let opts = Opts::parse();
    let config = config::Config::from_file(&opts.config).context(format!(
        "Failed to load configuration file {}",
        opts.config.display()
    ))?;

    let listen_config = config
        .server
        .listen
        .map(|l| parse_listen_address(&l))
        .transpose()?;

    let listen_addr = match (opts.listen, listen_config) {
        (Some(l), _) => l,
        (_, Some(c)) => c,
        (None, None) => SocketAddr::new("::1".parse().unwrap(), boardswarm_protocol::DEFAULT_PORT),
    };

    let authentication: Vec<_> = config
        .server
        .authentication
        .iter()
        .map(|a| {
            if let config::Authentication::Jwks { path } = a {
                config::Authentication::Jwks {
                    path: opts.config.with_file_name(path),
                }
            } else {
                a.clone()
            }
        })
        .collect();

    if authentication.is_empty() {
        bail!("No authentication methods found in configuration");
    }

    let server = Server::new(
        authentication,
        opts.config
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
    );
    for d in config.devices {
        let device = crate::config_device::Device::from_config(d, server.clone());
        let properties = Properties::new(device.name());
        server.register_device(properties, device);
    }

    let local = tokio::task::LocalSet::new();
    let serial = config
        .providers
        .iter()
        .find(|p| p.name == serial::PROVIDER)
        .map(|p| serial::SerialDevices::new(&p.name, server.clone()));
    for p in config.providers {
        match p.provider.as_str() {
            dfu::PROVIDER => {
                local.spawn_local(dfu::start_provider(p.name, server.clone()));
            }
            mediatek_brom::PROVIDER => match serial {
                Some(ref s) => s.add_provider(MediatekBromProvider::new(p.name, server.clone())),
                None => {
                    bail!("Mediatek brom provider requires the serial provider to be enabled")
                }
            },
            qcomlt_debug_board::PROVIDER => match serial {
                Some(ref s) => {
                    s.add_provider(QCOMLTDebugBoardProvider::new(p.name, server.clone()))
                }
                None => {
                    bail!("QCOMLT Debug Board provider requires the serial provider to be enabled")
                }
            },
            rockusb::PROVIDER => {
                local.spawn_local(rockusb::start_provider(p.name, server.clone()));
            }
            serial::PROVIDER => {
                // Precreated already
            }
            fastboot::PROVIDER => {
                local.spawn_local(fastboot::start_provider(
                    p.name,
                    p.parameters,
                    server.clone(),
                ));
            }
            gpio::PROVIDER => {
                local.spawn_local(gpio::start_provider(
                    p.name,
                    p.parameters.context("Missing gpio provider parameters")?,
                    server.clone(),
                ));
            }
            pdudaemon::PROVIDER => pdudaemon::start_provider(
                p.name,
                p.parameters
                    .context("Missing pdudaemon provider parameters")?,
                server.clone(),
            ),
            boardswarm_provider::PROVIDER => boardswarm_provider::start_provider(
                p.name,
                p.parameters
                    .context("Missing boardswarm provider parameters")?,
                server.clone(),
            ),
            t => warn!("Unknown provider: {t}"),
        }
    }
    if let Some(serial) = serial {
        local.spawn_local(serial.start());
    }

    let boardswarm = tonic::service::Routes::new(
        boardswarm_protocol::boardswarm_server::BoardswarmServer::new(server.clone()),
    );

    let auth = setup_auth_layer(&server.inner.auth_info).await?;
    let router = boardswarm
        .into_axum_router()
        .layer(auth.into_layer())
        .route_service(
            &format!("/{}/LoginInfo",
          <boardswarm_protocol::boardswarm_server::BoardswarmServer<Server>
          as tonic::server::NamedService>::NAME),
            boardswarm_protocol::boardswarm_server::BoardswarmServer::new(server.clone()),
        );

    if let Some(cert) = config.server.certificate {
        info!("Server listening on {}", listen_addr);
        let tls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(cert.chain, cert.key).await?;

        let s = axum_server::bind_rustls(listen_addr, tls_config).serve(router.into_make_service());
        tokio::join!(local, s).1?;
    } else {
        let s = axum_server::bind(listen_addr).serve(router.into_make_service());
        tokio::join!(local, s).1?;
    }

    Ok(())
}
