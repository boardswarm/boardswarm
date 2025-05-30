use serde::Deserialize;
use std::{collections::HashMap, io::Write, path::PathBuf};
use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, info, warn};

use crate::{
    registry::{self, Properties},
    serial::SerialProvider,
    udev::Device,
    Server,
};

pub const PROVIDER: &str = "serial-command";

#[derive(Deserialize, Clone, Debug)]
struct Command {
    name: String,
    message: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
struct SerialCommandParameters {
    #[serde(rename = "match")]
    match_: HashMap<String, String>,
    rate: u32,
    commands: Vec<Command>,
}

pub struct SerialCommandProvider {
    name: String,
    parameters: SerialCommandParameters,
    server: Server,
}

impl SerialCommandProvider {
    pub fn new(name: String, parameters: serde_yaml::Value, server: Server) -> Self {
        let parameters: SerialCommandParameters = serde_yaml::from_value(parameters).unwrap();
        Self {
            name,
            parameters,
            server,
        }
    }
}

impl SerialProvider for SerialCommandProvider {
    fn handle(&mut self, device: &crate::udev::Device, _seqnum: u64) -> bool {
        let provider_properties = &[
            (registry::PROVIDER_NAME, self.name.as_str()),
            (registry::PROVIDER, PROVIDER),
        ];
        if let Some(node) = device.devnode() {
            if let Some(name) = node.file_name() {
                let name = name.to_string_lossy().into_owned();
                let mut properties = device.properties(name);
                if !properties.matches(&self.parameters.match_) {
                    debug!(
                        "Ignoring device {} - {:?}",
                        device.syspath().display(),
                        properties,
                    );
                    return false;
                }
                properties.extend(provider_properties);
                tokio::spawn(setup_serial_command(
                    node.to_path_buf(),
                    properties,
                    self.parameters.clone(),
                    self.server.clone(),
                ));
                return true;
            }
        }
        false
    }

    fn remove(&mut self, device: &Device) {
        // TODO: implement this!
        warn!(
            "Remove not implemented, ignoring device {}",
            device.syspath().display()
        );
    }
}

async fn setup_serial_command(
    node: PathBuf,
    properties: Properties,
    parameters: SerialCommandParameters,
    server: Server,
) {
    info!("Setting up serial cmd for {}", node.display());
    let port = match tokio_serial::new(node.to_string_lossy(), parameters.rate).open_native_async()
    {
        Ok(port) => port,
        Err(e) => {
            warn!("Failed to open serial port: {e}");
            return;
        }
    };

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(process(port, rx));

    for command in parameters.commands {
        let mut properties = properties.clone();
        properties.insert(registry::NAME, command.name.clone());
        server.register_actuator(
            properties,
            SerialCommand {
                command,
                tx: tx.clone(),
            },
        );
    }
}

async fn process(
    mut port: tokio_serial::SerialStream,
    mut rx: tokio::sync::mpsc::Receiver<Command>,
) {
    while let Some(command) = rx.recv().await {
        let buf = format!("{}\n", command.message);
        debug!("Writing serial command: {}", &buf);
        let _ = port.write_all(buf.as_bytes());
    }
}
struct SerialCommand {
    command: Command,
    tx: tokio::sync::mpsc::Sender<Command>,
}

impl std::fmt::Debug for SerialCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO make more meaningful
        f.debug_struct("SerialCommand").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl crate::Actuator for SerialCommand {
    async fn set_mode(
        &self,
        _parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), crate::ActuatorError> {
        self.tx.send(self.command.clone()).await.unwrap();
        Ok(())
    }
}
