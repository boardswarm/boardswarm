use boardswarm_protocol::{
    console_input_request, device_upload_request, upload_request, ConsoleConfigureRequest,
    ConsoleInputRequest, ConsoleOutputRequest, DeviceUploaderRequest, UploadStatus,
    UploaderRequest,
};
use bytes::Bytes;
use clap::Parser;
use futures::prelude::*;
use futures::stream::BoxStream;
use futures::Sink;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::{net::ToSocketAddrs, sync::Arc};
use thiserror::Error;
use tokio_stream::wrappers::WatchStream;
use tonic::Streaming;

use tracing::{info, warn};

mod config;
mod dfu;
mod pdudaemon;
mod serial;
mod udev;

#[derive(Error, Debug)]
pub enum ActuatorError {}

#[async_trait::async_trait]
trait Actuator: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &str;
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), ActuatorError>;
}

#[derive(Error, Debug)]
pub enum ConsoleError {}

#[async_trait::async_trait]
trait Console: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &str;
    fn configure(
        &self,
        parameters: Box<dyn erased_serde::Deserializer>,
    ) -> Result<(), ConsoleError>;
    async fn input(
        &self,
    ) -> Result<Pin<Box<dyn Sink<Bytes, Error = ConsoleError> + Send>>, ConsoleError>;
    async fn output(&self)
        -> Result<BoxStream<'static, Result<Bytes, ConsoleError>>, ConsoleError>;
    fn matches(&self, filter: serde_yaml::Value) -> bool;
}

type ConsoleOutputStream =
    stream::BoxStream<'static, Result<boardswarm_protocol::ConsoleOutput, tonic::Status>>;

#[async_trait::async_trait]
trait ConsoleExt: Console {
    async fn output_stream(&self) -> ConsoleOutputStream {
        Box::pin(self.output().await.unwrap().map(|data| {
            Ok(boardswarm_protocol::ConsoleOutput {
                data_or_state: Some(boardswarm_protocol::console_output::DataOrState::Data(
                    data.unwrap(),
                )),
            })
        }))
    }
}

impl<C> ConsoleExt for C where C: Console + ?Sized {}

#[derive(Clone, Error, Debug)]
pub enum UploaderError {}

type UploadProgressStream = WatchStream<Result<UploadStatus, tonic::Status>>;

#[derive(Debug)]
pub struct UploadProgress {
    tx: tokio::sync::watch::Sender<Result<UploadStatus, tonic::Status>>,
}

impl UploadProgress {
    fn new() -> (Self, UploadProgressStream) {
        let (tx, rx) = tokio::sync::watch::channel(Ok(UploadStatus {
            progress: "init".to_string(),
        }));
        (Self { tx }, WatchStream::new(rx))
    }

    fn update(&self, progress: String) {
        let _ = self.tx.send(Ok(UploadStatus { progress }));
    }
}

#[async_trait::async_trait]
pub trait Uploader: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &str;
    fn matches(&self, filter: serde_yaml::Value) -> bool;
    async fn upload(
        &self,
        target: &str,
        data: BoxStream<'static, Bytes>,
        length: u64,
        progress: UploadProgress,
    ) -> Result<(), UploaderError>;

    async fn commit(&self) -> Result<(), UploaderError>;
}

#[derive(Debug)]
struct DeviceInner {
    config: config::Device,
    server: Server,
}

#[derive(Clone, Debug)]
struct Device {
    inner: Arc<DeviceInner>,
}

impl Device {
    fn from_config(config: config::Device, server: Server) -> Device {
        Device {
            inner: Arc::new(DeviceInner { config, server }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.config.name
    }

    pub fn get_console(&self, console: &str) -> Option<&config::Console> {
        self.inner
            .config
            .consoles
            .iter()
            .find(|c| c.name == console)
    }

    pub fn get_uploader(&self, uploader: &str) -> Option<&config::Uploader> {
        self.inner
            .config
            .uploaders
            .iter()
            .find(|u| u.name == uploader)
    }

    pub fn get_default_console(&self) -> Option<&config::Console> {
        if self.inner.config.consoles.len() == 1 {
            Some(&self.inner.config.consoles[0])
        } else {
            self.inner.config.consoles.iter().find(|c| c.default)
        }
    }

    pub async fn set_mode(&self, mode: &str) {
        if let Some(mode) = self.inner.config.modes.iter().find(|m| m.name == mode) {
            for step in &mode.sequence {
                if let Some(provider) = self.inner.server.get_actuator(&step.name) {
                    provider
                        .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                            step.parameters.clone(),
                        )))
                        .await
                        .unwrap();
                } else {
                    warn!("Provider {} not found", &step.name);
                    return;
                }
                if let Some(duration) = step.stabilisation {
                    tokio::time::sleep(duration).await;
                }
            }
        }
    }
}

#[derive(Debug)]
struct ServerInner {
    devices: Mutex<Vec<Device>>,
    consoles: Mutex<Vec<Arc<dyn Console>>>,
    actuators: Mutex<Vec<Arc<dyn Actuator>>>,
    uploaders: Mutex<Vec<Arc<dyn Uploader>>>,
}

#[derive(Debug, Clone)]
pub struct Server {
    inner: Arc<ServerInner>,
}

impl Server {
    fn new() -> Self {
        Self {
            inner: Arc::new(ServerInner {
                consoles: Mutex::new(Vec::new()),
                devices: Mutex::new(Vec::new()),
                actuators: Mutex::new(Vec::new()),
                uploaders: Mutex::new(Vec::new()),
            }),
        }
    }

    fn register_actuator(&self, actuator: Arc<dyn Actuator>) {
        info!("Registering new actuator: {}", actuator.name());
        let mut actuators = self.inner.actuators.lock().unwrap();
        actuators.push(actuator)
    }

    #[allow(dead_code)]
    fn get_actuator(&self, name: &str) -> Option<Arc<dyn Actuator>> {
        let actuators = self.inner.actuators.lock().unwrap();
        actuators.iter().find(|c| c.name() == name).cloned()
    }

    fn register_console(&self, console: Arc<dyn Console>) {
        info!("Registering new console: {}", console.name());
        let mut consoles = self.inner.consoles.lock().unwrap();
        consoles.push(console)
    }

    fn get_console(&self, name: &str) -> Option<Arc<dyn Console>> {
        let consoles = self.inner.consoles.lock().unwrap();
        consoles.iter().find(|c| c.name() == name).cloned()
    }

    fn register_uploader(&self, uploader: Arc<dyn Uploader>) {
        info!("Registering new uploader: {}", uploader.name());
        let mut uploaders = self.inner.uploaders.lock().unwrap();
        uploaders.push(uploader)
    }

    fn unregister_uploader(&self, uploader: &dyn Uploader) {
        info!("Unregistering uploader: {}", uploader.name());
        let mut uploaders = self.inner.uploaders.lock().unwrap();
        uploaders.retain(|u| u.name() != uploader.name())
    }

    pub fn get_uploader(&self, name: &str) -> Option<Arc<dyn Uploader>> {
        let uploaders = self.inner.uploaders.lock().unwrap();
        uploaders.iter().find(|u| u.name() == name).cloned()
    }

    fn register_device(&self, device: Device) {
        info!("Registering new device: {}", device.name());
        let mut devices = self.inner.devices.lock().unwrap();
        devices.push(device);
    }

    fn get_device(&self, name: &str) -> Option<Device> {
        let devices = self.inner.devices.lock().unwrap();
        devices.iter().find(|d| d.name() == name).cloned()
    }

    fn console_for_device(&self, device: &str, console: Option<&str>) -> Option<Arc<dyn Console>> {
        let device = self.get_device(device)?;
        let config_console = match console {
            Some(console) => device.get_console(console)?,
            _ => device.get_default_console()?,
        };
        let device_console = self
            .inner
            .consoles
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.matches(config_console.match_.filter.clone()))?
            .clone();
        device_console
            .configure(Box::new(<dyn erased_serde::Deserializer>::erase(
                config_console.parameters.clone(),
            )))
            .ok()?;

        Some(device_console)
    }

    fn uploader_for_device(&self, device: &str, uploader: &str) -> Option<Arc<dyn Uploader>> {
        let device = self.get_device(device)?;
        let config_uploader = device.get_uploader(uploader)?;
        let device_uploader = self
            .inner
            .uploaders
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.matches(config_uploader.match_.filter.clone()))?
            .clone();

        Some(device_uploader)
    }
}

#[tonic::async_trait]
impl boardswarm_protocol::consoles_server::Consoles for Server {
    type StreamOutputStream = ConsoleOutputStream;

    async fn list(
        &self,
        _request: tonic::Request<()>,
    ) -> Result<tonic::Response<boardswarm_protocol::ConsoleList>, tonic::Status> {
        let consoles = self.inner.consoles.lock().unwrap();
        Ok(tonic::Response::new(boardswarm_protocol::ConsoleList {
            consoles: consoles.iter().map(|d| d.name().to_string()).collect(),
        }))
    }

    async fn configure(
        &self,
        request: tonic::Request<ConsoleConfigureRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(console) = self.get_console(&inner.console) {
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

    async fn stream_output(
        &self,
        request: tonic::Request<ConsoleOutputRequest>,
    ) -> Result<tonic::Response<Self::StreamOutputStream>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(console) = self.get_console(&inner.console) {
            Ok(tonic::Response::new(console.output_stream().await))
        } else {
            Err(tonic::Status::invalid_argument("Can't find output console"))
        }
    }

    async fn stream_input(
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
            self.get_console(&console)
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
}

#[tonic::async_trait]
impl boardswarm_protocol::devices_server::Devices for Server {
    type StreamOutputStream = ConsoleOutputStream;

    async fn stream_output(
        &self,
        request: tonic::Request<boardswarm_protocol::DeviceTarget>,
    ) -> Result<tonic::Response<Self::StreamOutputStream>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(console) = self.console_for_device(&inner.device, inner.console.as_deref()) {
            Ok(tonic::Response::new(console.output_stream().await))
        } else {
            Err(tonic::Status::invalid_argument("Can't find output console"))
        }
    }

    async fn stream_input(
        &self,
        request: tonic::Request<tonic::Streaming<boardswarm_protocol::DeviceInputRequest>>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let mut rx = request.into_inner();

        /* First message must select the target */
        let msg = match rx.message().await? {
            Some(msg) => msg,
            None => return Ok(tonic::Response::new(())),
        };
        let console =
            if let Some(boardswarm_protocol::device_input_request::TargetOrData::Target(target)) =
                msg.target_or_data
            {
                self.console_for_device(&target.device, target.console.as_deref())
                    .ok_or_else(|| tonic::Status::not_found("No serial console by that name"))?
            } else {
                return Err(tonic::Status::invalid_argument(
                    "Target should be set first",
                ));
            };

        let mut input = console.input().await.unwrap();

        while let Some(request) = rx.message().await? {
            match request.target_or_data {
                Some(boardswarm_protocol::device_input_request::TargetOrData::Data(data)) => {
                    input.send(data).await.unwrap()
                }
                _ => return Err(tonic::Status::invalid_argument("Target cannot be changed")),
            }
        }
        Ok(tonic::Response::new(()))
    }

    async fn change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::DeviceModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        if let Some(device) = self.get_device(&request.device) {
            device.set_mode(&request.mode).await;
            Ok(tonic::Response::new(()))
        } else {
            Err(tonic::Status::not_found("No device by that name"))
        }
    }

    async fn list(
        &self,
        _request: tonic::Request<()>,
    ) -> Result<tonic::Response<boardswarm_protocol::DeviceList>, tonic::Status> {
        let devices = self.inner.devices.lock().unwrap();
        Ok(tonic::Response::new(boardswarm_protocol::DeviceList {
            devices: devices.iter().map(|d| d.name().to_string()).collect(),
        }))
    }

    type UploadStream = UploadProgressStream;
    async fn upload(
        &self,
        request: tonic::Request<tonic::Streaming<boardswarm_protocol::DeviceUploadRequest>>,
    ) -> Result<tonic::Response<Self::UploadStream>, tonic::Status> {
        let mut rx = request.into_inner();
        let msg = match rx.message().await? {
            Some(msg) => msg,
            None => {
                return Err(tonic::Status::invalid_argument(
                    "No device/uploader/target selection",
                ))
            }
        };

        if let Some(device_upload_request::TargetOrData::Target(target)) = msg.target_or_data {
            let uploader = self
                .uploader_for_device(&target.device, &target.uploader)
                .ok_or_else(|| tonic::Status::not_found("No uploader console by that name"))?;

            let data = stream::unfold(rx, |mut rx| async move {
                // TODO handle errors
                if let Some(msg) = rx.message().await.ok()? {
                    match msg.target_or_data {
                        Some(device_upload_request::TargetOrData::Data(data)) => Some((data, rx)),
                        _ => None, // TODO this is an error!
                    }
                } else {
                    None
                }
            })
            .boxed();

            let (progress, progress_stream) = UploadProgress::new();
            tokio::spawn(async move {
                uploader
                    .upload(&target.target, data, target.length, progress)
                    .await
                    .unwrap()
            });

            Ok(tonic::Response::new(progress_stream))
        } else {
            Err(tonic::Status::invalid_argument(
                "Target should be set first",
            ))
        }
    }

    async fn upload_commit(
        &self,
        request: tonic::Request<DeviceUploaderRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        let uploader = self
            .uploader_for_device(&request.device, &request.uploader)
            .ok_or_else(|| tonic::Status::not_found("No uploader by that name"))?;
        uploader
            .commit()
            .await
            .map_err(|_e| tonic::Status::unknown("Commit failed"))?;
        Ok(tonic::Response::new(()))
    }
}

#[tonic::async_trait]
impl boardswarm_protocol::actuators_server::Actuators for Server {
    async fn list(
        &self,
        _request: tonic::Request<()>,
    ) -> Result<tonic::Response<boardswarm_protocol::ActuatorList>, tonic::Status> {
        let actuators = self.inner.actuators.lock().unwrap();
        Ok(tonic::Response::new(boardswarm_protocol::ActuatorList {
            actuators: actuators.iter().map(|d| d.name().to_string()).collect(),
        }))
    }

    async fn change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::ActuatorModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(actuator) = self.get_actuator(&inner.actuator) {
            actuator
                .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                    inner.parameters.unwrap(),
                )))
                .await
                .unwrap();
            Ok(tonic::Response::new(()))
        } else {
            Err(tonic::Status::invalid_argument("Can't find console"))
        }
    }
}

#[tonic::async_trait]
impl boardswarm_protocol::uploaders_server::Uploaders for Server {
    async fn list(
        &self,
        _request: tonic::Request<()>,
    ) -> Result<tonic::Response<boardswarm_protocol::UploaderList>, tonic::Status> {
        let uploaders = self.inner.uploaders.lock().unwrap();
        Ok(tonic::Response::new(boardswarm_protocol::UploaderList {
            uploaders: uploaders.iter().map(|d| d.name().to_string()).collect(),
        }))
    }

    type UploadStream = UploadProgressStream;
    async fn upload(
        &self,
        request: tonic::Request<tonic::Streaming<boardswarm_protocol::UploadRequest>>,
    ) -> Result<tonic::Response<Self::UploadStream>, tonic::Status> {
        let mut rx = request.into_inner();
        let msg = match rx.message().await? {
            Some(msg) => msg,
            None => {
                return Err(tonic::Status::invalid_argument(
                    "No uploader/target selection",
                ))
            }
        };

        if let Some(upload_request::TargetOrData::Target(target)) = msg.target_or_data {
            let uploader = self
                .get_uploader(&target.uploader)
                .ok_or_else(|| tonic::Status::not_found("No uploader console by that name"))?;

            let data = stream::unfold(rx, |mut rx| async move {
                // TODO handle errors
                if let Some(msg) = rx.message().await.ok()? {
                    match msg.target_or_data {
                        Some(upload_request::TargetOrData::Data(data)) => Some((data, rx)),
                        _ => None, // TODO this is an error!
                    }
                } else {
                    None
                }
            })
            .boxed();

            let (progress, progress_stream) = UploadProgress::new();
            tokio::spawn(async move {
                uploader
                    .upload(&target.target, data, target.length, progress)
                    .await
                    .unwrap()
            });

            Ok(tonic::Response::new(progress_stream))
        } else {
            Err(tonic::Status::invalid_argument(
                "Target should be set first",
            ))
        }
    }

    async fn commit(
        &self,
        request: tonic::Request<UploaderRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        let uploader = self
            .get_uploader(&request.uploader)
            .ok_or_else(|| tonic::Status::not_found("No uploader by that name"))?;
        uploader
            .commit()
            .await
            .map_err(|_e| tonic::Status::unknown("Commit failed"))?;
        Ok(tonic::Response::new(()))
    }
}

#[derive(Debug, clap::Parser)]
struct Opts {
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let opts = Opts::parse();
    let config = config::Config::from_file(opts.config)?;

    let server = Server::new();
    for d in config.devices {
        server.register_device(Device::from_config(d, server.clone()))
    }

    for p in config.providers {
        if p.type_ == "pdudaemon" {
            pdudaemon::start_provider(p.name, p.parameters.unwrap(), server.clone());
        }
    }

    let local = tokio::task::LocalSet::new();
    local.spawn_local(udev::start_provider("udev".to_string(), server.clone()));

    let server = tonic::transport::Server::builder()
        .add_service(boardswarm_protocol::devices_server::DevicesServer::new(
            server.clone(),
        ))
        .add_service(boardswarm_protocol::consoles_server::ConsolesServer::new(
            server.clone(),
        ))
        .add_service(boardswarm_protocol::actuators_server::ActuatorsServer::new(
            server.clone(),
        ))
        .add_service(boardswarm_protocol::uploaders_server::UploadersServer::new(
            server,
        ))
        .serve("[::1]:50051".to_socket_addrs().unwrap().next().unwrap());
    info!("Server listening");
    tokio::join!(local, server).1?;

    Ok(())
}
