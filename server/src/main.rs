use bytes::Bytes;
use clap::Parser;
use futures::prelude::*;
use futures::stream::BoxStream;
use futures::Sink;
use protocol::protocol::serial_server;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::{collections::HashMap, net::ToSocketAddrs, sync::Arc, time::Duration};
use thiserror::Error;

use tracing::info;

mod config;
mod serial;
mod udev;

//////////////////////////////////////////////////////////
// TOTRAIT
// Generates actuators and consoles
#[allow(dead_code)]
struct Provider {
    name: String,
}

// TOTRAIT
#[allow(dead_code)]
#[derive(Clone, Debug)]
struct Actuator {
    name: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct Actuation {
    name: String,
    controller: String,
    state: String, /* on, off, flashing */
    parameters: HashMap<String, String>,
    stablelisation: Duration,
}

// Actions to do to put a device in a certain mode
#[allow(dead_code)]
#[derive(Clone, Debug)]
struct DeviceMode {
    name: String,
    steps: Vec<Actuation>,
}

#[derive(Error, Debug)]
pub enum ConsoleError {}

// TOTRAIT
#[async_trait::async_trait]
trait Console: std::fmt::Debug + Send + Sync {
    fn name(&self) -> &str;
    fn configure(&self, parameters: serde_yaml::Value) -> Result<(), ConsoleError>;
    async fn input(
        &self,
    ) -> Result<Pin<Box<dyn Sink<Bytes, Error = ConsoleError> + Send>>, ConsoleError>;
    async fn output(&self)
        -> Result<BoxStream<'static, Result<Bytes, ConsoleError>>, ConsoleError>;
    fn matches(&self, filter: serde_yaml::Value) -> bool;
}

#[derive(Debug)]
struct DeviceInner {
    config: config::Device,
    //consoles: Vec<DeviceConsole>,
    //modes: Vec<DeviceMode>,
    // TODO sensors
}
#[derive(Clone, Debug)]
struct Device {
    inner: Arc<DeviceInner>,
}

impl Device {
    fn from_config(config: config::Device) -> Device {
        Device {
            inner: Arc::new(DeviceInner { config }),
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
}

#[derive(Debug)]
struct ServerInner {
    devices: Mutex<Vec<Device>>,
    consoles: Mutex<Vec<Arc<dyn Console>>>,
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
            }),
        }
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

    fn console_for_device(&self, device: &str, console: &str) -> Option<Arc<dyn Console>> {
        let device = self.get_device(device)?;
        let config_console = device.get_console(console)?;
        let device_console = self
            .inner
            .consoles
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.matches(config_console.match_.filter.clone()))?
            .clone();
        device_console
            .configure(config_console.parameters.clone())
            .ok()?;

        Some(device_console)
    }
}

#[tonic::async_trait]
impl protocol::protocol::serial_server::Serial for Server {
    type StreamOutputStream =
        stream::BoxStream<'static, Result<protocol::protocol::Output, tonic::Status>>;

    async fn stream_output(
        &self,
        request: tonic::Request<protocol::protocol::OutputRequest>,
    ) -> Result<tonic::Response<Self::StreamOutputStream>, tonic::Status> {
        if let Some(console) = self.console_for_device(&request.into_inner().name, "main") {
            let output = console.output().await.unwrap().map(|data| {
                Ok(protocol::protocol::Output {
                    data: data.unwrap(),
                })
            });
            Ok(tonic::Response::new(Box::pin(output)))
        } else {
            Err(tonic::Status::invalid_argument("Can't find output console"))
        }
    }

    async fn stream_input(
        &self,
        request: tonic::Request<tonic::Streaming<protocol::protocol::InputRequest>>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let mut rx = request.into_inner();
        let mut console = None;
        while let Ok(Some(request)) = rx.message().await {
            if console.is_none() {
                console = self.console_for_device(&request.name.unwrap(), "main");
            }
            if let Some(console) = &console {
                let mut input = console.input().await.unwrap();
                input.send(request.data).await.unwrap();
            } else {
                break;
            }
        }
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
        server.register_device(Device::from_config(d))
    }

    let local = tokio::task::LocalSet::new();
    local.spawn_local(udev::start_provider("udev".to_string(), server.clone()));

    let server = tonic::transport::Server::builder()
        .add_service(serial_server::SerialServer::new(server))
        .serve("[::1]:50051".to_socket_addrs().unwrap().next().unwrap());
    info!("Server listening");
    tokio::join!(local, server).1?;

    Ok(())
}
