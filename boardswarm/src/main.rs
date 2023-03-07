use boardswarm_protocol::item_event::Event;
use boardswarm_protocol::{
    console_input_request, volume_io_reply, volume_io_request, ConsoleConfigureRequest,
    ConsoleInputRequest, ConsoleOutputRequest, ItemEvent, ItemList, ItemPropertiesMsg,
    ItemPropertiesRequest, ItemTypeRequest, Property, VolumeInfoMsg, VolumeRequest,
};
use bytes::Bytes;
use clap::Parser;
use futures::prelude::*;
use futures::stream::BoxStream;
use futures::Sink;
use registry::{Properties, Registry};
use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;

use tracing::{info, warn};

use crate::registry::RegistryChange;

mod boardswarm_provider;
mod config;
mod dfu;
mod gpio;
mod pdudaemon;
mod registry;
mod serial;
mod udev;

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
pub enum ConsoleError {}

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
}

type VolumeIoReplyStream =
    ReceiverStream<Result<boardswarm_protocol::VolumeIoReply, tonic::Status>>;

enum VolumeIoReply {
    Read(oneshot::Receiver<Result<Bytes, tonic::Status>>),
    Write(oneshot::Receiver<Result<u64, tonic::Status>>),
    Flush(oneshot::Receiver<Result<(), tonic::Status>>),
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
                    VolumeIoReply::FatalError(e) => Err(e),
                };
                if reply_tx.send(reply).await.is_err() {
                    break;
                };
            }
        });
        (Self { completion_tx }, ReceiverStream::new(reply_rx))
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

    fn enqueue_fatal_error(&mut self, error: tonic::Status) {
        let _ = self.completion_tx.send(VolumeIoReply::FatalError(error));
    }
}

type VolumeTargetInfo = boardswarm_protocol::VolumeTarget;
#[async_trait::async_trait]
pub trait Volume: std::fmt::Debug + Send + Sync {
    fn targets(&self) -> &[VolumeTargetInfo];
    async fn open(
        &self,
        target: &str,
        length: Option<u64>,
    ) -> Result<Box<dyn VolumeTarget>, VolumeError>;
    async fn commit(&self) -> Result<(), VolumeError>;
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
}

trait DeviceConfigItem {
    fn matches(&self, properties: &Properties) -> bool;
}

impl DeviceConfigItem for config::Console {
    fn matches(&self, properties: &Properties) -> bool {
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::Volume {
    fn matches(&self, properties: &Properties) -> bool {
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::ModeStep {
    fn matches(&self, properties: &Properties) -> bool {
        properties.matches(&self.match_)
    }
}

struct DeviceItem<C> {
    config: C,
    id: Mutex<Option<u64>>,
}

impl<C> DeviceItem<C>
where
    C: DeviceConfigItem,
{
    fn new(config: C) -> Self {
        Self {
            config,
            id: Mutex::new(None),
        }
    }

    fn config(&self) -> &C {
        &self.config
    }

    fn set(&self, id: Option<u64>) {
        *self.id.lock().unwrap() = id;
    }

    fn get(&self) -> Option<u64> {
        *self.id.lock().unwrap()
    }

    fn unset_if_matches(&self, id: u64) -> bool {
        let mut i = self.id.lock().unwrap();
        match *i {
            Some(item_id) if item_id == id => {
                *i = None;
                true
            }
            _ => false,
        }
    }

    fn set_if_matches(&self, id: u64, properties: &Properties) -> bool {
        if self.config.matches(properties) {
            self.set(Some(id));
            true
        } else {
            false
        }
    }
}

impl From<&Device> for boardswarm_protocol::Device {
    fn from(d: &Device) -> Self {
        let consoles = d
            .inner
            .consoles
            .iter()
            .map(|c| boardswarm_protocol::Console {
                name: c.config().name.clone(),
                id: c.get(),
            })
            .collect();
        let volumes = d
            .inner
            .volumes
            .iter()
            .map(|u| boardswarm_protocol::Volume {
                name: u.config().name.clone(),
                id: u.get(),
            })
            .collect();
        let modes = d
            .inner
            .modes
            .iter()
            .map(|m| boardswarm_protocol::Mode {
                name: m.name.clone(),
                depends: m.depends.clone(),
                available: m.sequence.iter().all(|s| s.get().is_some()),
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

struct DeviceMonitor {
    receiver: broadcast::Receiver<()>,
}

#[derive(Debug, Error)]
enum DeviceSetModeError {
    #[error("Mode not found")]
    ModeNotFound,
    #[error("Wrong current mode")]
    WrongCurrentMode,
    #[error("Actuator failed: {0}")]
    ActuatorFailed(#[from] ActuatorError),
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

// TODO deal with closing
struct DeviceNotifier {
    sender: broadcast::Sender<()>,
}

impl DeviceNotifier {
    fn new() -> Self {
        Self {
            sender: broadcast::channel(1).0,
        }
    }

    async fn notify(&self) {
        let _ = self.sender.send(());
    }

    fn watch(&self) -> DeviceMonitor {
        DeviceMonitor {
            receiver: self.sender.subscribe(),
        }
    }
}

struct DeviceMode {
    name: String,
    depends: Option<String>,
    sequence: Vec<DeviceItem<config::ModeStep>>,
}

impl From<config::Mode> for DeviceMode {
    fn from(config: config::Mode) -> Self {
        let sequence = config.sequence.into_iter().map(DeviceItem::new).collect();
        DeviceMode {
            name: config.name,
            depends: config.depends,
            sequence,
        }
    }
}

struct DeviceInner {
    notifier: DeviceNotifier,
    name: String,
    current_mode: Mutex<Option<String>>,
    consoles: Vec<DeviceItem<config::Console>>,
    volumes: Vec<DeviceItem<config::Volume>>,
    modes: Vec<DeviceMode>,
    server: Server,
}

#[derive(Clone)]
struct Device {
    inner: Arc<DeviceInner>,
}

impl Device {
    fn from_config(config: config::Device, server: Server) -> Device {
        let name = config.name;
        let consoles = config.consoles.into_iter().map(DeviceItem::new).collect();
        let volumes = config.volumes.into_iter().map(DeviceItem::new).collect();
        let notifier = DeviceNotifier::new();
        let modes = config.modes.into_iter().map(Into::into).collect();
        Device {
            inner: Arc::new(DeviceInner {
                notifier,
                name,
                current_mode: Mutex::new(None),
                consoles,
                volumes,
                modes,
                server,
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn updates(&self) -> DeviceMonitor {
        self.inner.notifier.watch()
    }

    // TODO add a semaphore ot only allow one sequence to run at a time
    pub async fn set_mode(&self, mode: &str) -> Result<(), DeviceSetModeError> {
        let target = self
            .inner
            .modes
            .iter()
            .find(|m| m.name == mode)
            .ok_or(DeviceSetModeError::ModeNotFound)?;
        {
            let mut current = self.inner.current_mode.lock().unwrap();
            if let Some(depend) = &target.depends {
                if current.as_ref() != Some(depend) {
                    return Err(DeviceSetModeError::WrongCurrentMode);
                }
            }
            *current = None;
        }

        for step in &target.sequence {
            let step = step.config();
            if let Some(provider) = self.inner.server.find_actuator(&step.match_) {
                provider
                    .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                        step.parameters.clone(),
                    )))
                    .await?;
            } else {
                warn!("Provider {:?} not found", &step.match_);
                return Err(ActuatorError {}.into());
            }
            if let Some(duration) = step.stabilisation {
                tokio::time::sleep(duration).await;
            }
        }
        {
            let mut current = self.inner.current_mode.lock().unwrap();
            *current = Some(mode.to_string());
        }
        self.inner.notifier.notify().await;
        Ok(())
    }

    fn current_mode(&self) -> Option<String> {
        let mode = self.inner.current_mode.lock().unwrap();
        mode.clone()
    }

    async fn monitor_items(&self) {
        fn add_item_with<'a, C, I, F, IT>(items: I, id: u64, item: registry::Item<IT>, f: F) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
            F: Fn(&DeviceItem<C>, &IT),
        {
            items.fold(false, |changed, i| {
                if i.set_if_matches(id, &item.properties()) {
                    f(i, item.inner());
                    true
                } else {
                    changed
                }
            })
        }

        fn add_item<'a, T, C, I>(items: I, id: u64, item: registry::Item<T>) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
        {
            add_item_with(items, id, item, |_, _| {})
        }

        fn change_with<'a, T, C, I, F>(items: I, change: RegistryChange<T>, f: F) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
            F: Fn(&DeviceItem<C>, &T),
        {
            match change {
                registry::RegistryChange::Added { id, item } => add_item_with(items, id, item, f),
                registry::RegistryChange::Removed(id) => {
                    items.fold(false, |changed, c| c.unset_if_matches(id) || changed)
                }
            }
        }
        fn change<'a, T, C: DeviceConfigItem + 'a, I: Iterator<Item = &'a DeviceItem<C>>>(
            items: I,
            change: RegistryChange<T>,
        ) -> bool {
            change_with(items, change, |_, _| {})
        }
        fn setup_console(dev: &DeviceItem<config::Console>, console: &Arc<dyn Console>) {
            if let Err(e) = console.configure(Box::new(<dyn erased_serde::Deserializer>::erase(
                dev.config().parameters.clone(),
            ))) {
                warn!("Failed to configure console: {}", e);
            }
        }

        let mut actuator_monitor = self.inner.server.inner.actuators.monitor();
        let mut console_monitor = self.inner.server.inner.consoles.monitor();
        let mut uploader_monitor = self.inner.server.inner.volumes.monitor();
        let mut changed = false;

        for (id, item) in self.inner.server.inner.actuators.contents() {
            changed |= add_item(
                self.inner.modes.iter().flat_map(|m| m.sequence.iter()),
                id,
                item,
            );
        }

        for (id, item) in self.inner.server.inner.consoles.contents() {
            changed |= add_item_with(self.inner.consoles.iter(), id, item, setup_console);
        }

        for (id, item) in self.inner.server.inner.volumes.contents() {
            changed |= add_item(self.inner.volumes.iter(), id, item);
        }

        if changed {
            self.inner.notifier.notify().await;
        }

        loop {
            let changed = tokio::select! {
                msg = console_monitor.recv() => {
                    match msg {
                        Ok(c) => change_with(self.inner.consoles.iter(), c, setup_console),
                        Err(e) => {
                            warn!("Issue with monitoring consoles: {:?}", e); return },
                    }
                }
                msg = actuator_monitor.recv() => {
                    match msg {
                        Ok(c) => change(
                            self.inner.modes.iter().flat_map(|m| m.sequence.iter()),
                            c),
                        Err(e) => {
                            warn!("Issue with monitoring actuators: {:?}", e); return },
                        }
                }
                msg = uploader_monitor.recv() => {
                    match msg {
                        Ok(c) => change(self.inner.volumes.iter(), c),
                        Err(e) => {
                            warn!("Issue with monitoring volumes: {:?}", e); return },
                    }
                }
            };
            if changed {
                self.inner.notifier.notify().await;
            }
        }
    }
}

struct ServerInner {
    devices: Registry<Device>,
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
    fn new() -> Self {
        Self {
            inner: Arc::new(ServerInner {
                consoles: Registry::new(),
                devices: Registry::new(),
                actuators: Registry::new(),
                volumes: Registry::new(),
            }),
        }
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
        info!("Registered uploader: {} - {}", id, item);
        id
    }

    fn unregister_volume(&self, id: u64) {
        if let Some(item) = self.inner.volumes.lookup(id) {
            info!("Unregistering uploader: {} - {}", id, item.name());
            self.inner.volumes.remove(id);
        }
    }

    pub fn get_volume(&self, id: u64) -> Option<Arc<dyn Volume>> {
        self.inner
            .volumes
            .lookup(id)
            .map(registry::Item::into_inner)
    }

    fn register_device(&self, device: Device) {
        let properties = Properties::new(device.name());
        let (id, item) = self.inner.devices.add(properties, device);
        info!("Registered device: {} - {}", id, item);
    }

    fn get_device(&self, id: u64) -> Option<Device> {
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
    async fn list(
        &self,
        request: tonic::Request<ItemTypeRequest>,
    ) -> Result<tonic::Response<ItemList>, tonic::Status> {
        let request = request.into_inner();
        let type_ = boardswarm_protocol::ItemType::from_i32(request.r#type)
            .ok_or_else(|| tonic::Status::invalid_argument("Unknown item type "))?;

        Ok(tonic::Response::new(self.item_list_for(type_)))
    }

    type MonitorStream = ItemMonitorStream;
    async fn monitor(
        &self,
        request: tonic::Request<ItemTypeRequest>,
    ) -> Result<tonic::Response<Self::MonitorStream>, tonic::Status> {
        let request = request.into_inner();
        let type_ = boardswarm_protocol::ItemType::from_i32(request.r#type)
            .ok_or_else(|| tonic::Status::invalid_argument("Unknown item type "))?;

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
        let type_ = boardswarm_protocol::ItemType::from_i32(request.r#type)
            .ok_or_else(|| tonic::Status::invalid_argument("Unknown item type "))?;
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
            let info = (&device).into();
            let monitor = device.updates();
            let stream = Box::pin(stream::once(async move { Ok(info) }).chain(stream::unfold(
                (device, monitor),
                |(device, mut monitor)| async move {
                    monitor.wait().await.ok()?;
                    let info = (&device).into();
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

            let mut target = volume
                .open(&target.target, target.length)
                .await
                .map_err(|_| tonic::Status::not_found("No target by that name"))?;

            let (mut reply, reply_stream) = VolumeIoReplies::new();
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
                    warn!("Invalid request, no actualy request"); return; };

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
        volume
            .commit()
            .await
            .map_err(|_e| tonic::Status::unknown("Commit failed"))?;
        Ok(tonic::Response::new(()))
    }

    async fn volume_info(
        &self,
        request: tonic::Request<VolumeRequest>,
    ) -> Result<tonic::Response<VolumeInfoMsg>, tonic::Status> {
        let request = request.into_inner();
        let volume = self
            .get_volume(request.volume)
            .ok_or_else(|| tonic::Status::not_found("Uploader not found"))?;

        let info = VolumeInfoMsg {
            target: volume.targets().to_vec(),
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

#[derive(Debug, clap::Parser)]
struct Opts {
    #[clap(short, long)]
    #[arg(value_parser = parse_listen_address)]
    listen: Option<SocketAddr>,
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let opts = Opts::parse();
    let config = config::Config::from_file(opts.config)?;
    let listen_config = config
        .listen
        .map(|l| parse_listen_address(&l))
        .transpose()?;

    let listen_addr = match (opts.listen, listen_config) {
        (Some(l), _) => l,
        (_, Some(c)) => c,
        (None, None) => SocketAddr::new("::1".parse().unwrap(), boardswarm_protocol::DEFAULT_PORT),
    };

    let server = Server::new();
    for d in config.devices {
        let device = Device::from_config(d, server.clone());
        server.register_device(device.clone());
        tokio::spawn(async move {
            loop {
                device.monitor_items().await
            }
        });
    }

    for p in config.providers {
        match p.type_.as_str() {
            "gpio" => gpio::start_provider(p.name, p.parameters.unwrap(), server.clone()),
            "pdudaemon" => pdudaemon::start_provider(p.name, p.parameters.unwrap(), server.clone()),
            "boardswarm" => {
                boardswarm_provider::start_provider(p.name, p.parameters.unwrap(), server.clone())
            }
            t => warn!("Unknown provider type: {t}"),
        }
    }

    let local = tokio::task::LocalSet::new();
    local.spawn_local(udev::start_provider("udev".to_string(), server.clone()));

    let server = tonic::transport::Server::builder()
        .add_service(boardswarm_protocol::boardswarm_server::BoardswarmServer::new(server.clone()))
        .serve(listen_addr);
    info!("Server listening on {}", listen_addr);
    tokio::join!(local, server).1?;

    Ok(())
}
