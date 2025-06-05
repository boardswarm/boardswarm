use rexpect::spawn_stream;
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info, warn};

use crate::{
    registry::{self, Properties},
    serial::SerialProvider,
    udev::Device,
    Server,
};
use serialport::TTYPort;

pub const PROVIDER: &str = "hifive-p550-mcu";

const HIFIVE_P550_MCU_COMMANDS: &[&str] = &["hifive-p550-mcu-sompower", "hifive-p550-mcu-bootsel"];
const HIFIVE_P550_MCU_TIMEOUT: u64 = 5000;

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
    let port = match tokio_serial::new(node.to_string_lossy(), 921600)
        .timeout(Duration::from_millis(10))
        .open_native()
    {
        Ok(port) => Arc::new(AsyncMutex::new(port)),
        Err(e) => {
            warn!("Failed to open serial port: {e}");
            return;
        }
    };

    for &command in HIFIVE_P550_MCU_COMMANDS {
        let mut properties = properties.clone();
        properties.insert(registry::NAME, command);
        server.register_actuator(
            properties,
            HifiveP550MCUActuator {
                name: command.into(),
                port: port.clone(),
            },
        );
    }
}

#[derive(Deserialize)]
struct ParametersSomPower {
    value: bool,
}

struct HifiveP550MCUActuator {
    name: String,
    port: Arc<AsyncMutex<TTYPort>>,
}

impl std::fmt::Debug for HifiveP550MCUActuator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO make more meaningful
        f.debug_struct("HifiveP550MCUActuator")
            .finish_non_exhaustive()
    }
}

impl From<rexpect::error::Error> for crate::ActuatorError {
    fn from(_: rexpect::error::Error) -> Self {
        crate::ActuatorError {}
    }
}

#[async_trait::async_trait]
impl crate::Actuator for HifiveP550MCUActuator {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), crate::ActuatorError> {
        let (command, response) = match self.name.as_str() {
            "hifive-p550-mcu-sompower" => self.handle_sompower(parameters).await,
            "hifive-p550-mcu-bootsel" => self.handle_bootsel(parameters).await,
            _ => return Err(crate::ActuatorError {}),
        };
        let port = self.port.lock().await;
        let reader = port.try_clone_native().unwrap();
        let writer = port.try_clone_native().unwrap();
        let mut session = spawn_stream(reader, writer, Some(HIFIVE_P550_MCU_TIMEOUT));
        debug!("Sending: {:?}", &command);
        session.send_line(command)?;
        debug!("Waiting: {:?}", &response);
        session.exp_string(response)?;
        Ok(())
    }
}

impl HifiveP550MCUActuator {
    async fn handle_sompower(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> (&str, &str) {
        let parameters = ParametersSomPower::deserialize(parameters).unwrap();
        if parameters.value {
            ("sompower-s 1", "POWERON")
        } else {
            ("sompower-s 0", "STOP_POWER")
        }
    }

    async fn handle_bootsel(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> (&str, &str) {
        let parameters = ParametersSomPower::deserialize(parameters).unwrap();
        if parameters.value {
            (
                "bootsel-s sw 0x3",
                "Set: Bootsel Controlled by: SW, bootsel[3 2 1 0]:0 0 1 1",
            )
        } else {
            (
                "bootsel-s hw 0x0",
                "Set: Bootsel Controlled by: HW, bootsel[3 2 1 0]:0 0 1 0",
            )
        }
    }
}
