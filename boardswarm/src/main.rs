use boardswarm_protocol::{
    console_input_request, ConsoleConfigureRequest, ConsoleInputRequest, ConsoleOutputRequest,
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
use tonic::Streaming;

use tracing::{info, warn};

mod config;
mod pdudaemon;
mod serial;
mod udev;

//////////////////////////////////////////////////////////
// TOTRAIT
// Generates actuators and consoles
#[allow(dead_code)]
struct Provider {
    name: String,
}

#[derive(Error, Debug)]
pub enum ActuatorError {}

#[async_trait::async_trait]
trait Actuator: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &str;
    async fn set_mode(&self, parameters: serde_yaml::Value) -> Result<(), ActuatorError>;
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
                    provider.set_mode(step.parameters.clone()).await.unwrap();
                } else {
                    warn!("Provider {} not found", &step.name);
                    return;
                }
                if let Some(duration) = step.stablelisation {
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

    #[allow(dead_code)]
    fn get_console(&self, name: &str) -> Option<Arc<dyn Console>> {
        let consoles = self.inner.consoles.lock().unwrap();
        consoles.iter().find(|c| c.name() == name).cloned()
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
            server,
        ))
        .serve("[::1]:50051".to_socket_addrs().unwrap().next().unwrap());
    info!("Server listening");
    tokio::join!(local, server).1?;

    Ok(())
}
