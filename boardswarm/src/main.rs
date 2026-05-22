use anyhow::{Context, bail};
use axum::{
    http::{Request, StatusCode},
    routing::get,
};
use boardswarm_protocol::item_event::Event;
use boardswarm_protocol::signal_message::SdpMessage;
use boardswarm_protocol::{
    ConsoleConfigureRequest, ConsoleInputRequest, ConsoleOutputRequest, ItemEvent, ItemList,
    ItemPropertiesMsg, ItemPropertiesRequest, ItemTypeRequest, KeyboardRequest, LoginInfoList,
    MediaRequest, MouseRequest, Property, SignalMessage, SignalMessageIceCandidate,
    SignalMessageSdp, VolumeEraseRequest, VolumeInfoMsg, VolumeIoTargetReply, VolumeRequest,
    console_input_request, keyboard_request, media_request, mouse_request, volume_io_reply,
    volume_io_request,
};
use bytes::Bytes;
use clap::Parser;
use futures::Sink;
use futures::prelude::*;
use futures::stream::BoxStream;
use hifive_p550_mcu::HifiveP550MCUProvider;
use mediatek_brom::MediatekBromProvider;
use registry::{Properties, Registry, RegistryIndex};
use std::fmt::Display;
use std::net::{AddrParseError, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tower_http::cors::{Any, CorsLayer};
use tower_oauth2_resource_server::auth_resolver::KidAuthorizerResolver;
use tower_oauth2_resource_server::error::AuthError;
use tower_oauth2_resource_server::jwt_resolver::BearerTokenResolver;
use tower_oauth2_resource_server::jwt_unverified::UnverifiedJwt;
use tower_oauth2_resource_server::server::OAuth2ResourceServer;
use tower_oauth2_resource_server::tenant::TenantConfiguration;
use tracing::{info, instrument, warn};

mod boardswarm_provider;
mod config;
mod config_device;
mod dfu;
mod eswin_eic7700_storage;
mod fastboot;
mod gpio;
mod hid_gadget;
mod hifive_p550_mcu;
mod mediatek_brom;
mod pdudaemon;
mod registry;
mod rockusb;
mod serial;
mod udev;
mod utils;
mod v4l2_provider;
mod ws_console;

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
    #[error("Unavailable: {0}")]
    Unavailable(String),
    #[error("Console was closed")]
    Closed,
}

impl From<ConsoleError> for tonic::Status {
    fn from(e: ConsoleError) -> Self {
        match e {
            ConsoleError::Closed => tonic::Status::aborted(e.to_string()),
            ConsoleError::Unavailable(msg) => tonic::Status::unavailable(msg),
        }
    }
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
    async fn output_stream(&self) -> Result<ConsoleOutputStream, ConsoleError> {
        Ok(Box::pin(self.output().await?.map(|data| {
            Ok(boardswarm_protocol::ConsoleOutput {
                data: data.unwrap(),
            })
        })))
    }
}

impl<C> ConsoleExt for C where C: Console + ?Sized {}

#[derive(Clone, Error, Debug)]
pub enum VolumeError {
    #[error("Unknown target requested")]
    UnknownTargetRequested,
    #[error("Operation not implemented for this volume")]
    NotImplemented,
    #[error("Internal error: {0}")]
    Internal(String),
    #[error("Volume failure: {0}")]
    Failure(String),
}

impl From<VolumeError> for tonic::Status {
    fn from(e: VolumeError) -> Self {
        match e {
            VolumeError::UnknownTargetRequested => tonic::Status::not_found(e.to_string()),
            VolumeError::NotImplemented => tonic::Status::unimplemented(e.to_string()),
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
        Err(VolumeError::NotImplemented)
    }
}

pub enum MediaSignalMsg {
    Offer(String),
    Answer(String),
    Ice(String, u32),
}

impl From<MediaSignalMsg> for SignalMessage {
    fn from(value: MediaSignalMsg) -> Self {
        match value {
            MediaSignalMsg::Offer(sdp) => SignalMessage {
                sdp_message: Some(SdpMessage::Offer(SignalMessageSdp { sdp })),
            },
            MediaSignalMsg::Answer(sdp) => SignalMessage {
                sdp_message: Some(SdpMessage::Answer(SignalMessageSdp { sdp })),
            },

            MediaSignalMsg::Ice(candidate, mline_index) => SignalMessage {
                sdp_message: Some(SdpMessage::Ice(SignalMessageIceCandidate {
                    candidate,
                    mline_index,
                })),
            },
        }
    }
}

#[derive(Clone, Error, Debug)]
pub enum MediaError {
    #[error("Not supported")]
    NotSupported,
    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<MediaError> for tonic::Status {
    fn from(e: MediaError) -> Self {
        match e {
            MediaError::NotSupported => {
                tonic::Status::unimplemented("Media streaming not supported")
            }
            MediaError::Internal(msg) => tonic::Status::internal(msg),
        }
    }
}

pub trait MediaSignallingRx: Send {
    fn offer(&mut self, offer: &str);
    fn answer(&mut self, offer: &str);
    fn ice(&mut self, candidate: &str, mline_index: u32);
}

#[async_trait::async_trait]
pub trait Media: std::fmt::Debug + Send + Sync {
    async fn open(
        &self,
    ) -> Result<
        (
            Box<dyn MediaSignallingRx>,
            stream::BoxStream<'static, MediaSignalMsg>,
        ),
        MediaError,
    >;
}

/// Rust-side representation of a keyboard key event.
#[derive(Copy, Clone)]
pub enum KeyboardEvent {
    /// Key press (key down). Contains the HID Keyboard/Keypad usage ID (see HID Usage Tables §10).
    Down(u8),
    /// Key release (key up). Contains the HID Keyboard/Keypad usage ID.
    Up(u8),
}

impl KeyboardEvent {
    fn is_key(self) -> bool {
        match self {
            Self::Down(k) | Self::Up(k) => k > 0 && k <= 0xdd,
        }
    }

    fn is_modifier(self) -> bool {
        match self {
            Self::Down(m) | Self::Up(m) => m >= 0xe0 && m <= 0xe7,
        }
    }
}

impl TryFrom<boardswarm_protocol::KeyboardEvent> for KeyboardEvent {
    type Error = tonic::Status;

    fn try_from(e: boardswarm_protocol::KeyboardEvent) -> Result<Self, Self::Error> {
        let key = u8::try_from(e.key)
            .map_err(|_| tonic::Status::invalid_argument("Key value out of range"))?;
        match e.r#type() {
            boardswarm_protocol::KeyboardEventType::KeyDown => Ok(KeyboardEvent::Down(key)),
            boardswarm_protocol::KeyboardEventType::KeyUp => Ok(KeyboardEvent::Up(key)),
        }
    }
}

/// Rust-side representation of keyboard LED state returned by a keyboard device.
#[derive(Clone, Debug, Default)]
pub struct KeyboardState {
    pub leds: Vec<boardswarm_protocol::KeyboardLed>,
}

impl From<KeyboardState> for boardswarm_protocol::KeyboardState {
    fn from(s: KeyboardState) -> Self {
        boardswarm_protocol::KeyboardState {
            led: s.leds.into_iter().map(|l| l as i32).collect(),
        }
    }
}

/// Rust-side representation of a mouse input report.
pub struct MouseInput {
    /// Button bitmask (bits 0–7).
    pub buttons: u8,
    /// Absolute X position (signed 16-bit).
    pub x: i16,
    /// Absolute Y position (signed 16-bit).
    pub y: i16,
    /// Vertical scroll wheel (signed 8-bit range).
    pub wheel: i8,
    /// Horizontal scroll wheel (signed 8-bit range).
    pub hwheel: i8,
}

impl TryFrom<boardswarm_protocol::MouseInput> for MouseInput {
    type Error = tonic::Status;

    fn try_from(m: boardswarm_protocol::MouseInput) -> Result<Self, Self::Error> {
        let buttons = u8::try_from(m.buttons)
            .map_err(|_| tonic::Status::invalid_argument("Mouse buttons value out of range"))?;
        let x = i16::try_from(m.x)
            .map_err(|_| tonic::Status::invalid_argument("Mouse x value out of range"))?;
        let y = i16::try_from(m.y)
            .map_err(|_| tonic::Status::invalid_argument("Mouse y value out of range"))?;
        let wheel = i8::try_from(m.wheel)
            .map_err(|_| tonic::Status::invalid_argument("Mouse wheel value out of range"))?;
        let hwheel = i8::try_from(m.hwheel)
            .map_err(|_| tonic::Status::invalid_argument("Mouse hwheel value out of range"))?;
        Ok(MouseInput {
            buttons,
            x,
            y,
            wheel,
            hwheel,
        })
    }
}

#[derive(Clone, Error, Debug)]
pub enum KeyboardError {
    #[error("Keyboard not supported")]
    NotSupported,
    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<KeyboardError> for tonic::Status {
    fn from(e: KeyboardError) -> Self {
        match e {
            KeyboardError::NotSupported => tonic::Status::unimplemented("Keyboard not supported"),
            KeyboardError::Internal(msg) => tonic::Status::internal(msg),
        }
    }
}

#[derive(Clone, Error, Debug)]
pub enum MouseError {
    #[error("Mouse not supported")]
    NotSupported,
    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<MouseError> for tonic::Status {
    fn from(e: MouseError) -> Self {
        match e {
            MouseError::NotSupported => tonic::Status::unimplemented("Mouse not supported"),
            MouseError::Internal(msg) => tonic::Status::internal(msg),
        }
    }
}

#[async_trait::async_trait]
pub trait Keyboard: std::fmt::Debug + Send + Sync {
    /// Open a keyboard session.
    ///
    /// Returns a sender for key events and a stream of LED state updates.
    async fn open(
        &self,
    ) -> Result<
        (
            tokio::sync::mpsc::Sender<KeyboardEvent>,
            stream::BoxStream<'static, KeyboardState>,
        ),
        KeyboardError,
    >;
}

#[async_trait::async_trait]
pub trait Mouse: std::fmt::Debug + Send + Sync {
    /// Open a mouse session.
    ///
    /// Returns a sender for mouse input reports.
    async fn open(&self) -> Result<tokio::sync::mpsc::Sender<MouseInput>, MouseError>;
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

impl DeviceConfigItem for config::Media {
    #[instrument(fields(name = self.name), skip_all, level="error")]
    fn matches(&self, properties: &Properties) -> bool {
        if self.match_.is_empty() {
            warn!("Media matches is empty - will match any media item");
        }
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::Keyboard {
    #[instrument(fields(name = self.name), skip_all, level="error")]
    fn matches(&self, properties: &Properties) -> bool {
        if self.match_.is_empty() {
            warn!("Keyboard matches is empty - will match any keyboard");
        }
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::Mouse {
    #[instrument(fields(name = self.name), skip_all, level="error")]
    fn matches(&self, properties: &Properties) -> bool {
        if self.match_.is_empty() {
            warn!("Mouse matches is empty - will match any mouse");
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
                id: c.id.map(Into::into),
            })
            .collect();
        let volumes = d
            .volumes()
            .into_iter()
            .map(|v| boardswarm_protocol::Volume {
                name: v.name,
                id: v.id.map(Into::into),
            })
            .collect();
        let media = d
            .media()
            .into_iter()
            .map(|m| boardswarm_protocol::Media {
                name: m.name,
                id: m.id.map(Into::into),
            })
            .collect();
        let keyboards = d
            .keyboards()
            .into_iter()
            .map(|k| boardswarm_protocol::Keyboard {
                name: k.name,
                id: k.id.map(Into::into),
            })
            .collect();
        let mice = d
            .mice()
            .into_iter()
            .map(|m| boardswarm_protocol::Mouse {
                name: m.name,
                id: m.id.map(Into::into),
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
            media,
            keyboards,
            mice,
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
    id: Option<ConsoleId>,
}

struct DeviceVolume {
    name: String,
    id: Option<VolumeId>,
}

struct DeviceMedia {
    name: String,
    id: Option<MediaId>,
}

struct DeviceKeyboard {
    name: String,
    id: Option<KeyboardId>,
}

struct DeviceMouse {
    name: String,
    id: Option<MouseId>,
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
    fn media(&self) -> Vec<DeviceMedia>;
    fn keyboards(&self) -> Vec<DeviceKeyboard>;
    fn mice(&self) -> Vec<DeviceMouse>;
    fn modes(&self) -> Vec<DeviceMode>;
    fn current_mode(&self) -> Option<String>;
}

macro_rules! impl_u64_index {
    ($id:ident,$name:ident) => {
        #[derive(Copy, Clone, Debug, Default, Hash, PartialOrd, Ord, PartialEq, Eq)]
        pub struct $id(u64);

        impl Display for $id {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, stringify!($name {}), self.0)
            }
        }

        impl RegistryIndex for $id {
            fn next(&self) -> Self {
                Self(self.0 + 1)
            }
        }

        impl From<$id> for u64 {
            fn from(value: $id) -> Self {
                value.0
            }
        }
    };
}

impl_u64_index!(ActuatorId, Actuator);
impl_u64_index!(ConsoleId, Console);
impl_u64_index!(DeviceId, Device);
impl_u64_index!(VolumeId, Volume);
impl_u64_index!(MediaId, Media);
impl_u64_index!(KeyboardId, Keyboard);
impl_u64_index!(MouseId, Mouse);

struct ServerInner {
    config_dir: PathBuf,
    auth_info: Vec<config::Authentication>,
    devices: Registry<DeviceId, Arc<dyn Device>>,
    consoles: Registry<ConsoleId, Arc<dyn Console>>,
    actuators: Registry<ActuatorId, Arc<dyn Actuator>>,
    volumes: Registry<VolumeId, Arc<dyn Volume>>,
    media: Registry<MediaId, Arc<dyn Media>>,
    keyboards: Registry<KeyboardId, Arc<dyn Keyboard>>,
    mice: Registry<MouseId, Arc<dyn Mouse>>,
}

fn to_item_list<I, T>(registry: &Registry<I, T>) -> ItemList
where
    I: RegistryIndex + Into<u64>,
    T: Clone,
{
    let item = registry
        .contents()
        .into_iter()
        .map(|(id, item)| boardswarm_protocol::Item {
            id: id.into(),
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
                media: Registry::new(),
                keyboards: Registry::new(),
                mice: Registry::new(),
            }),
        }
    }

    fn config_dir(&self) -> &Path {
        &self.inner.config_dir
    }

    fn register_actuator<A>(&self, properties: Properties, actuator: A) -> ActuatorId
    where
        A: Actuator + 'static,
    {
        let (id, item) = self.inner.actuators.add(properties, Arc::new(actuator));
        info!("Registered actuator: {} - {}", id, item);
        id
    }

    fn get_actuator(&self, id: ActuatorId) -> Option<Arc<dyn Actuator>> {
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

    fn unregister_actuator(&self, id: ActuatorId) {
        if let Some(item) = self.inner.actuators.lookup(id) {
            info!("Unregistering actuator: {} - {}", id, item);
            self.inner.actuators.remove(id);
        }
    }

    fn register_console<C>(&self, properties: Properties, console: C) -> ConsoleId
    where
        C: Console + 'static,
    {
        let (id, item) = self.inner.consoles.add(properties, Arc::new(console));
        info!("Registered console: {} - {}", id, item);
        id
    }

    fn unregister_console(&self, id: ConsoleId) {
        if let Some(item) = self.inner.consoles.lookup(id) {
            info!("Unregistering console: {} - {}", id, item);
            self.inner.consoles.remove(id);
        }
    }

    fn get_console(&self, id: ConsoleId) -> Option<Arc<dyn Console>> {
        self.inner
            .consoles
            .lookup(id)
            .map(|item| item.inner().clone())
    }

    fn register_volume<V>(&self, properties: Properties, volume: V) -> VolumeId
    where
        V: Volume + 'static,
    {
        let (id, item) = self.inner.volumes.add(properties, Arc::new(volume));
        info!("Registered volume: {} - {}", id, item);
        id
    }

    fn unregister_volume(&self, id: VolumeId) {
        if let Some(item) = self.inner.volumes.lookup(id) {
            info!("Unregistering volume: {} - {}", id, item.name());
            self.inner.volumes.remove(id);
        }
    }

    pub fn get_volume(&self, id: VolumeId) -> Option<Arc<dyn Volume>> {
        self.inner
            .volumes
            .lookup(id)
            .map(registry::Item::into_inner)
    }

    fn register_media<M>(&self, properties: Properties, media: M) -> MediaId
    where
        M: Media + 'static,
    {
        let (id, item) = self.inner.media.add(properties, Arc::new(media));
        info!("Registered media: {} - {}", id, item);
        id
    }

    fn unregister_media(&self, id: MediaId) {
        if let Some(item) = self.inner.media.lookup(id) {
            info!("Unregistering media: {} - {}", id, item.name());
            self.inner.media.remove(id);
        }
    }

    pub fn get_media(&self, id: MediaId) -> Option<Arc<dyn Media>> {
        self.inner.media.lookup(id).map(registry::Item::into_inner)
    }

    fn register_keyboard<K>(&self, properties: Properties, keyboard: K) -> KeyboardId
    where
        K: Keyboard + 'static,
    {
        let (id, item) = self.inner.keyboards.add(properties, Arc::new(keyboard));
        info!("Registered keyboard: {} - {}", id, item);
        id
    }

    fn unregister_keyboard(&self, id: KeyboardId) {
        if let Some(item) = self.inner.keyboards.lookup(id) {
            info!("Unregistering keyboard: {} - {}", id, item.name());
            self.inner.keyboards.remove(id);
        }
    }

    pub fn get_keyboard(&self, id: KeyboardId) -> Option<Arc<dyn Keyboard>> {
        self.inner
            .keyboards
            .lookup(id)
            .map(registry::Item::into_inner)
    }

    fn register_mouse<M>(&self, properties: Properties, mouse: M) -> MouseId
    where
        M: Mouse + 'static,
    {
        let (id, item) = self.inner.mice.add(properties, Arc::new(mouse));
        info!("Registered mouse: {} - {}", id, item);
        id
    }

    fn unregister_mouse(&self, id: MouseId) {
        if let Some(item) = self.inner.mice.lookup(id) {
            info!("Unregistering mouse: {} - {}", id, item.name());
            self.inner.mice.remove(id);
        }
    }

    pub fn get_mouse(&self, id: MouseId) -> Option<Arc<dyn Mouse>> {
        self.inner.mice.lookup(id).map(registry::Item::into_inner)
    }

    fn register_device<D>(&self, properties: Properties, device: D) -> DeviceId
    where
        D: Device + 'static,
    {
        let (id, item) = self.inner.devices.add(properties, Arc::new(device));
        info!("Registered device: {} - {}", id, item);
        id
    }

    fn unregister_device(&self, id: DeviceId) {
        if let Some(item) = self.inner.devices.lookup(id) {
            info!("Unregistering device: {} - {}", id, item.name());
            self.inner.devices.remove(id);
        }
    }

    fn get_device(&self, id: u64) -> Option<Arc<dyn Device>> {
        self.inner
            .devices
            .lookup(DeviceId(id))
            .map(registry::Item::into_inner)
    }

    fn item_list_for(&self, type_: boardswarm_protocol::ItemType) -> ItemList {
        match type_ {
            boardswarm_protocol::ItemType::Actuator => to_item_list(&self.inner.actuators),
            boardswarm_protocol::ItemType::Device => to_item_list(&self.inner.devices),
            boardswarm_protocol::ItemType::Console => to_item_list(&self.inner.consoles),
            boardswarm_protocol::ItemType::Volume => to_item_list(&self.inner.volumes),
            boardswarm_protocol::ItemType::Media => to_item_list(&self.inner.media),
            boardswarm_protocol::ItemType::Keyboard => to_item_list(&self.inner.keyboards),
            boardswarm_protocol::ItemType::Mouse => to_item_list(&self.inner.mice),
        }
    }
}

type ItemMonitorStream = BoxStream<'static, Result<boardswarm_protocol::ItemEvent, tonic::Status>>;

type MediaSignalStream =
    stream::BoxStream<'static, Result<boardswarm_protocol::SignalMessage, tonic::Status>>;

type KeyboardStateStream =
    stream::BoxStream<'static, Result<boardswarm_protocol::KeyboardState, tonic::Status>>;

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

        fn to_item_stream<I, T>(registry: &Registry<I, T>) -> ItemMonitorStream
        where
            I: RegistryIndex + Into<u64> + Send + 'static,
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
                                        id: id.into(),
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
                                event: Some(Event::Remove(removed.into())),
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
            boardswarm_protocol::ItemType::Media => to_item_stream(&self.inner.media),
            boardswarm_protocol::ItemType::Keyboard => to_item_stream(&self.inner.keyboards),
            boardswarm_protocol::ItemType::Mouse => to_item_stream(&self.inner.mice),
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
                .lookup(ActuatorId(request.item))
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Device => self
                .inner
                .devices
                .lookup(DeviceId(request.item))
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Console => self
                .inner
                .consoles
                .lookup(ConsoleId(request.item))
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Volume => self
                .inner
                .volumes
                .lookup(VolumeId(request.item))
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Media => self
                .inner
                .media
                .lookup(MediaId(request.item))
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Keyboard => self
                .inner
                .keyboards
                .lookup(KeyboardId(request.item))
                .ok_or_else(|| tonic::Status::not_found("Item not found"))?
                .properties(),
            boardswarm_protocol::ItemType::Mouse => self
                .inner
                .mice
                .lookup(MouseId(request.item))
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
        let console = ConsoleId(inner.console);
        match self.get_console(console) {
            Some(console) => {
                console
                    .configure(Box::new(<dyn erased_serde::Deserializer>::erase(
                        inner.parameters.unwrap(),
                    )))
                    .unwrap();
                Ok(tonic::Response::new(()))
            }
            _ => Err(tonic::Status::invalid_argument("Can't find console")),
        }
    }

    type ConsoleStreamOutputStream = ConsoleOutputStream;
    async fn console_stream_output(
        &self,
        request: tonic::Request<ConsoleOutputRequest>,
    ) -> Result<tonic::Response<Self::ConsoleStreamOutputStream>, tonic::Status> {
        let inner = request.into_inner();
        let console = ConsoleId(inner.console);
        match self.get_console(console) {
            Some(console) => {
                let stream = console.output_stream().await?;
                Ok(tonic::Response::new(stream))
            }
            _ => Err(tonic::Status::invalid_argument("Can't find output console")),
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
            self.get_console(ConsoleId(console))
                .ok_or_else(|| tonic::Status::not_found("No console by that name"))?
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
        match self.get_device(request.device) {
            Some(device) => {
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
            }
            _ => Err(tonic::Status::not_found("No such device")),
        }
    }

    async fn device_change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::DeviceModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        match self.get_device(request.device) {
            Some(device) => match device.set_mode(&request.mode).await {
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
            },
            _ => Err(tonic::Status::not_found("No device by that id")),
        }
    }

    async fn actuator_change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::ActuatorModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let inner = request.into_inner();
        let actuator = ActuatorId(inner.actuator);
        match self.get_actuator(actuator) {
            Some(actuator) => {
                actuator
                    .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                        inner.parameters.unwrap(),
                    )))
                    .await
                    .unwrap();
                Ok(tonic::Response::new(()))
            }
            _ => Err(tonic::Status::invalid_argument("Can't find actuator")),
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
                ));
            }
        };

        if let Some(volume_io_request::TargetOrRequest::Target(target)) = msg.target_or_request {
            let volume = VolumeId(target.volume);
            let volume = self
                .get_volume(volume)
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
        let volume = VolumeId(request.volume);
        let volume = self
            .get_volume(volume)
            .ok_or_else(|| tonic::Status::not_found("Volume not found"))?;
        volume.commit().await?;
        Ok(tonic::Response::new(()))
    }

    async fn volume_erase(
        &self,
        request: tonic::Request<VolumeEraseRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        let volume = VolumeId(request.volume);
        let volume = self
            .get_volume(volume)
            .ok_or_else(|| tonic::Status::not_found("Volume not found"))?;
        volume.erase(&request.target).await?;
        Ok(tonic::Response::new(()))
    }

    async fn volume_info(
        &self,
        request: tonic::Request<VolumeRequest>,
    ) -> Result<tonic::Response<VolumeInfoMsg>, tonic::Status> {
        let request = request.into_inner();
        let volume = VolumeId(request.volume);
        let volume = self
            .get_volume(volume)
            .ok_or_else(|| tonic::Status::not_found("Volume not found"))?;

        let (target, exhaustive) = volume.targets();

        let info = VolumeInfoMsg {
            target: target.to_vec(),
            exhaustive,
        };
        Ok(tonic::Response::new(info))
    }

    type MediaSetupStream = MediaSignalStream;
    async fn media_setup(
        &self,
        request: tonic::Request<tonic::Streaming<MediaRequest>>,
    ) -> Result<tonic::Response<Self::MediaSetupStream>, tonic::Status> {
        let mut request = request.into_inner();
        let initial_msg = match request.message().await? {
            Some(msg) => msg,
            None => {
                return Err(tonic::Status::invalid_argument("No media id selection"));
            }
        };

        let Some(media_request::ItemOrSignal::Item(media)) = initial_msg.item_or_signal else {
            return Err(tonic::Status::invalid_argument(
                "First message should be an item",
            ));
        };

        let media = MediaId(media);
        let media = self
            .get_media(media)
            .ok_or_else(|| tonic::Status::not_found("Media not found"))?;
        let (mut rx, tx) = media.open().await?;

        // Handle ongoing incoming stream
        tokio::spawn(async move {
            while let Ok(Some(msg)) = request.message().await {
                match msg.item_or_signal {
                    Some(media_request::ItemOrSignal::Signal(signal)) => match signal.sdp_message {
                        Some(SdpMessage::Offer(offer)) => {
                            rx.offer(&offer.sdp);
                        }
                        Some(SdpMessage::Answer(answer)) => {
                            rx.answer(&answer.sdp);
                        }
                        Some(SdpMessage::Ice(ice)) => {
                            rx.ice(&ice.candidate, ice.mline_index);
                        }
                        None => {
                            warn!("Ignoring empty signalling message (signal)")
                        }
                    },
                    Some(media_request::ItemOrSignal::Item(_)) => {
                        warn!("Not expecting media item after the first message");
                    }
                    None => {
                        warn!("Ignoring empty signalling message")
                    }
                }
            }
        });

        let replies = tx.map(|msg| Ok(msg.into()));

        // Handle outgoing stream
        Ok(tonic::Response::new(replies.boxed()))
    }

    type KeyboardIoStream = KeyboardStateStream;
    async fn keyboard_io(
        &self,
        request: tonic::Request<tonic::Streaming<KeyboardRequest>>,
    ) -> Result<tonic::Response<Self::KeyboardIoStream>, tonic::Status> {
        let mut request = request.into_inner();
        let initial_msg = match request.message().await? {
            Some(msg) => msg,
            None => {
                return Err(tonic::Status::invalid_argument("No keyboard id selection"));
            }
        };

        let Some(keyboard_request::ItemOrSignal::Item(id)) = initial_msg.item_or_signal else {
            return Err(tonic::Status::invalid_argument(
                "First message should be a keyboard item id",
            ));
        };

        let keyboard = self
            .get_keyboard(KeyboardId(id))
            .ok_or_else(|| tonic::Status::not_found("Keyboard not found"))?;
        let (event_tx, state_stream) = keyboard.open().await?;

        // Forward incoming key events to the keyboard device
        tokio::spawn(async move {
            while let Ok(Some(msg)) = request.message().await {
                match msg.item_or_signal {
                    Some(keyboard_request::ItemOrSignal::Event(event)) => {
                        match KeyboardEvent::try_from(event) {
                            Ok(e) => {
                                if event_tx.send(e).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("Ignoring invalid keyboard event: {e}");
                            }
                        }
                    }
                    Some(keyboard_request::ItemOrSignal::Item(_)) => {
                        warn!("Not expecting keyboard item id after the first message");
                    }
                    None => {
                        warn!("Ignoring empty keyboard message");
                    }
                }
            }
        });

        let replies = state_stream.map(|s| Ok(s.into()));
        Ok(tonic::Response::new(replies.boxed()))
    }

    async fn mouse_io(
        &self,
        request: tonic::Request<tonic::Streaming<MouseRequest>>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let mut request = request.into_inner();
        let initial_msg = match request.message().await? {
            Some(msg) => msg,
            None => {
                return Err(tonic::Status::invalid_argument("No mouse id selection"));
            }
        };

        let Some(mouse_request::ItemOrSignal::Item(id)) = initial_msg.item_or_signal else {
            return Err(tonic::Status::invalid_argument(
                "First message should be a mouse item id",
            ));
        };

        let mouse = self
            .get_mouse(MouseId(id))
            .ok_or_else(|| tonic::Status::not_found("Mouse not found"))?;
        let input_tx = mouse.open().await?;

        // Forward incoming mouse inputs to the mouse device
        while let Ok(Some(msg)) = request.message().await {
            match msg.item_or_signal {
                Some(mouse_request::ItemOrSignal::Input(input)) => {
                    match MouseInput::try_from(input) {
                        Ok(i) => {
                            if input_tx.send(i).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!("Ignoring invalid mouse input: {e}");
                        }
                    }
                }
                Some(mouse_request::ItemOrSignal::Item(_)) => {
                    warn!("Not expecting mouse item id after the first message");
                }
                None => {
                    warn!("Ignoring empty mouse message");
                }
            }
        }

        Ok(tonic::Response::new(()))
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

struct QueryTokenResolver;

impl BearerTokenResolver for QueryTokenResolver {
    fn resolve(&self, request: &Request<()>) -> Result<UnverifiedJwt, AuthError> {
        let token = request
            .uri()
            .query()
            .and_then(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, _)| key == "token")
                    .map(|(_, value)| value.into_owned())
            })
            .ok_or(AuthError::MissingAuthorizationHeader)?;

        Ok(UnverifiedJwt::new(token))
    }
}

async fn setup_auth_layer(
    config: &[config::Authentication],
) -> anyhow::Result<OAuth2ResourceServer> {
    setup_auth_layer_with_resolver(config, None).await
}

async fn setup_auth_layer_with_resolver(
    config: &[config::Authentication],
    bearer_token_resolver: Option<Arc<dyn BearerTokenResolver + Send + Sync>>,
) -> anyhow::Result<OAuth2ResourceServer> {
    let mut resource =
        OAuth2ResourceServer::builder().auth_resolver(Arc::new(KidAuthorizerResolver {}));
    if let Some(bearer_token_resolver) = bearer_token_resolver {
        resource = resource.bearer_token_resolver(bearer_token_resolver);
    }
    for auth in config {
        let tenant = match auth {
            config::Authentication::Oidc { uri, audience, .. } => {
                TenantConfiguration::builder(uri)
                    .audiences(audience.as_slice())
                    .build()
                    .await?
            }
            config::Authentication::Jwks { path } => {
                let jwks = tokio::fs::read_to_string(path).await?;
                TenantConfiguration::static_builder(jwks).build()?
            }
        };
        resource = resource.add_tenant(tenant);
    }
    Ok(resource.build().await?)
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

    // Temporarily set the default rustls crypto provider to aws-lc-rs until
    // reqwest allows this by default via a feature
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

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
            hifive_p550_mcu::PROVIDER => match serial {
                Some(ref s) => s.add_provider(HifiveP550MCUProvider::new(
                    p.name,
                    p.parameters.unwrap_or_default(),
                    server.clone(),
                )),
                None => {
                    bail!("Hifive P550 MCU provider requires the serial provider to be enabled")
                }
            },
            mediatek_brom::PROVIDER => match serial {
                Some(ref s) => s.add_provider(MediatekBromProvider::new(
                    p.name,
                    p.parameters.unwrap_or_default(),
                    server.clone(),
                )),
                None => {
                    bail!("Mediatek brom provider requires the serial provider to be enabled")
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
            eswin_eic7700_storage::PROVIDER => {
                local.spawn_local(eswin_eic7700_storage::start_provider(
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
            hid_gadget::PROVIDER => {
                hid_gadget::start_provider(p.name, p.parameters.unwrap_or_default(), server.clone())
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
            v4l2_provider::PROVIDER => {
                local.spawn_local(v4l2_provider::start_provider(
                    p.name,
                    p.parameters.unwrap_or_default(),
                    server.clone(),
                ));
            }

            t => warn!("Unknown provider: {t}"),
        }
    }
    if let Some(serial) = serial {
        local.spawn_local(serial.start());
    }

    let boardswarm = tonic::service::Routes::new(
        boardswarm_protocol::boardswarm_server::BoardswarmServer::new(server.clone()),
    );

    let grpc_auth = setup_auth_layer(&server.inner.auth_info).await?;
    let ws_auth =
        setup_auth_layer_with_resolver(&server.inner.auth_info, Some(Arc::new(QueryTokenResolver)))
            .await?;
    let login_info_path = format!(
        "/{}/LoginInfo",
        <boardswarm_protocol::boardswarm_server::BoardswarmServer<Server> as tonic::server::NamedService>::NAME,
    );
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    let router = boardswarm
        .into_axum_router()
        .layer(grpc_auth.into_layer())
        .route_service(
            &login_info_path,
            boardswarm_protocol::boardswarm_server::BoardswarmServer::new(server.clone()),
        )
        .layer(tonic_web::GrpcWebLayer::new())
        .route(
            "/api/ws/console",
            get(ws_console::handler)
                .layer(ws_auth.into_layer())
                .with_state(server.clone()),
        )
        .fallback(|| async {
            // TODO: Serve static files here.
            StatusCode::NOT_FOUND
        })
        .layer(cors);

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
