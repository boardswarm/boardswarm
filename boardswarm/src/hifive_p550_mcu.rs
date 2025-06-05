use serde::Deserialize;
use std::str::FromStr;
use std::time::Duration;
use std::{collections::HashMap, path::PathBuf};
use strum::{EnumIter, EnumString, IntoEnumIterator};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::time::sleep;

use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, error, info, warn};

use crate::ActuatorError;
use crate::{
    registry::{self, Properties},
    serial::SerialProvider,
    udev::Device,
    Server,
};

pub const PROVIDER: &str = "hifive-p550-mcu";

const MCU_RESPONSE_TIMEOUT_SECS: u64 = 10;

#[derive(Error, Debug)]
enum McuError {
    #[error("Timeout error")]
    Timeout(),
    #[error("I/O error: {0}")]
    IO(#[from] std::io::Error),
}

#[derive(Debug, Eq, PartialEq, strum_macros::Display, EnumIter, EnumString)]
#[strum(serialize_all = "kebab-case")]
enum McuCommand {
    HifiveP550McuSomPower(SomPowerParameters),
    HifiveP550McuBootSel(BootSelParameters),
}

#[derive(Debug, Eq, PartialEq, Default, Deserialize)]
struct SomPowerParameters {
    mode: SomPowerMode,
}

#[derive(Debug, Eq, PartialEq, Default, Deserialize)]
enum SomPowerMode {
    #[default]
    #[serde(rename = "off")]
    SomPowerModeOff,
    #[serde(rename = "on")]
    SomPowerModeOn,
}

#[derive(Debug, Eq, PartialEq, Default, Deserialize)]
struct BootSelParameters {
    mode: BootSelMode,
    value: u8,
}

#[derive(Debug, Eq, PartialEq, Default, Deserialize)]
enum BootSelMode {
    #[default]
    #[serde(rename = "hardware")]
    BootSelModeHardware,
    #[serde(rename = "software")]
    BootSelModeSoftware,
}

impl McuCommand {
    fn to_request_response(&self) -> (String, String) {
        match &self {
            McuCommand::HifiveP550McuSomPower(parameters) => {
                let (value, response) = match parameters.mode {
                    SomPowerMode::SomPowerModeOff => (0, "STOP_POWER"),
                    SomPowerMode::SomPowerModeOn => (1, "POWERON"),
                };
                (format!("sompower-s {}", value), response.into())
            }
            McuCommand::HifiveP550McuBootSel(parameters) => {
                let mode = match parameters.mode {
                    BootSelMode::BootSelModeHardware => "hw",
                    BootSelMode::BootSelModeSoftware => "sw",
                };
                let value = format!("{:04b}", parameters.value)
                    .chars()
                    .map(String::from)
                    .collect::<Vec<String>>()
                    .join(" ");
                (
                    format!("bootsel-s {} 0x{:x}", mode, parameters.value),
                    format!(
                        "Set: Bootsel Controlled by: {}, bootsel[3 2 1 0]:{}",
                        mode.to_uppercase(),
                        value
                    ),
                )
            }
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
struct HifiveP550MCUParameters {
    #[serde(rename = "match")]
    match_: HashMap<String, String>,
}

pub struct HifiveP550MCUProvider {
    name: String,
    parameters: HifiveP550MCUParameters,
    server: Server,
}

impl HifiveP550MCUProvider {
    pub fn new(name: String, parameters: serde_yaml::Value, server: Server) -> Self {
        let parameters: HifiveP550MCUParameters = serde_yaml::from_value(parameters).unwrap();
        Self {
            name,
            parameters,
            server,
        }
    }
}

impl SerialProvider for HifiveP550MCUProvider {
    fn handle(&mut self, device: &crate::udev::Device, _seqnum: u64) -> bool {
        let provider_properties = &[
            (registry::PROVIDER_NAME, self.name.as_str()),
            (registry::PROVIDER, PROVIDER),
        ];
        if device.property_u64("ID_VENDOR_ID", 16) != Some(0x0403) {
            return false;
        };
        if device.property_u64("ID_MODEL_ID", 16) != Some(0x6011) {
            return false;
        };
        if let Some(node) = device.devnode() {
            if let Some(name) = node.file_name() {
                let mut properties = device.properties(name.to_string_lossy());
                if !properties.matches(&self.parameters.match_) {
                    debug!(
                        "Ignoring device {} - {:?}",
                        device.syspath().display(),
                        properties,
                    );
                    return false;
                }
                properties.extend(provider_properties);
                tokio::spawn(setup_hifive_p550_mcu(
                    node.to_path_buf(),
                    properties,
                    self.server.clone(),
                ));
                return true;
            }
        }
        false
    }

    fn remove(&mut self, _device: &Device) {
        todo!("Remove not implemented");
    }
}

async fn setup_hifive_p550_mcu(node: PathBuf, properties: Properties, server: Server) {
    info!(
        "Setting up Hifive P550 MCU serial connection for {}",
        node.display()
    );
    let port = match tokio_serial::new(node.to_string_lossy(), 115200).open_native_async() {
        Ok(port) => port,
        Err(e) => {
            warn!("Failed to open serial port: {e}");
            return;
        }
    };

    let (sender, exec) = tokio::sync::mpsc::channel(16);
    tokio::spawn(process(port, exec));

    for command in McuCommand::iter() {
        let mut properties = properties.clone();
        properties.insert(registry::NAME, command.to_string());
        server.register_actuator(
            properties,
            HifiveP550MCUActuator {
                name: command.to_string(),
                sender: sender.clone(),
            },
        );
    }
}

async fn process(
    port: tokio_serial::SerialStream,
    mut exec: tokio::sync::mpsc::Receiver<(
        McuCommand,
        tokio::sync::oneshot::Sender<Result<(), McuError>>,
    )>,
) {
    let (read, mut write) = tokio::io::split(port);
    let reader = BufReader::new(read);
    let mut lines = reader.lines();

    while let Some((command, resp)) = exec.recv().await {
        let (request, response) = command.to_request_response();
        debug!("Sending: {:?}", request);
        write.write_all(request.as_bytes()).await.unwrap();
        debug!("Waiting: {:?}", response);
        loop {
            tokio::select! {
                _ = sleep(Duration::from_secs(MCU_RESPONSE_TIMEOUT_SECS)) => {
                    let _ = resp.send(Err(McuError::Timeout()));
                    break;
                },
                line = lines.next_line() => {
                    match line {
                        Ok(Some(ret)) => {
                            debug!("Response: {:?}", &ret);
                            if ret.contains(&response) {
                                let _ = resp.send(Ok(()));
                                break;
                            }
                        },
                        Ok(None) => {
                            let _ = resp.send(Err(McuError::Timeout()));
                            break;
                        },
                        Err(e) => {
                            let _ = resp.send(Err(e.into()));
                            break;
                        },
                    }
                }
            }
        }
    }
}

struct HifiveP550MCUActuator {
    name: String,
    sender: tokio::sync::mpsc::Sender<(
        McuCommand,
        tokio::sync::oneshot::Sender<Result<(), McuError>>,
    )>,
}

impl std::fmt::Debug for HifiveP550MCUActuator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO make more meaningful
        f.debug_struct("HifiveP550MCUActuator")
            .finish_non_exhaustive()
    }
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for crate::ActuatorError {
    fn from(_: tokio::sync::mpsc::error::SendError<T>) -> Self {
        crate::ActuatorError {}
    }
}

impl From<tokio::sync::oneshot::error::RecvError> for crate::ActuatorError {
    fn from(_: tokio::sync::oneshot::error::RecvError) -> Self {
        crate::ActuatorError {}
    }
}

#[async_trait::async_trait]
impl crate::Actuator for HifiveP550MCUActuator {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), crate::ActuatorError> {
        let command = self.parse_command(parameters);
        let (tx, rx) = oneshot::channel();
        self.sender.send((command, tx)).await?;
        if let Err(e) = rx.await? {
            error!("Actuator {} failed: {:?}", &self.name, e);
            return Err(ActuatorError());
        }
        Ok(())
    }
}

impl HifiveP550MCUActuator {
    fn parse_command(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> McuCommand {
        match McuCommand::from_str(&self.name).unwrap() {
            McuCommand::HifiveP550McuSomPower(_) => {
                let parameters = SomPowerParameters::deserialize(parameters).unwrap();
                McuCommand::HifiveP550McuSomPower(parameters)
            }
            McuCommand::HifiveP550McuBootSel(_) => {
                let parameters = BootSelParameters::deserialize(parameters).unwrap();
                McuCommand::HifiveP550McuBootSel(parameters)
            }
        }
    }
}
