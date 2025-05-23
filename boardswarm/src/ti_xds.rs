use crate::{serial::SerialProvider, udev::Device, ActuatorError, Server};

use serde::Deserialize;

use std::{
    collections::HashMap,
    io::ErrorKind,
    str,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use tokio_serial::{SerialPortBuilderExt, SerialStream};

use tracing::{debug, info, warn};

pub const PROVIDER: &str = "ti-xds-tai";

#[derive(Deserialize, Debug, Default)]
struct TiXdsParameters {
    #[serde(rename = "match")]
    match_: HashMap<String, String>,
    model: String,
}

pub struct TiXdsProvider {
    name: String,
    id: Option<u64>,
    parameters: TiXdsParameters,
    server: Server,
}

impl TiXdsProvider {
    pub fn new(name: String, parameters: Option<serde_yaml::Value>, server: Server) -> Self {
        let parameters: TiXdsParameters = if let Some(parameters) = parameters {
            serde_yaml::from_value(parameters).unwrap()
        } else {
            Default::default()
        };

        Self {
            name,
            id: None,
            parameters,
            server,
        }
    }
}

fn command(port: &mut SerialStream, command: &str, prompt: &str, expect: &str) -> bool {
    let mut data = String::new();

    match port.try_write(command.as_bytes()) {
        Ok(_) => (),
        Err(e) => {
            warn!("Failed to write to serial port: {e}");
            return false;
        }
    };

    let now = Instant::now();

    loop {
        thread::sleep(Duration::from_millis(50));

        let mut buf: Vec<u8> = vec![0; 100];
        match port.try_read(&mut buf) {
            Ok(_) => (),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => {
                warn!("Failed to read serial port: {e}");
                return false;
            }
        };

        data.push_str(str::from_utf8(&buf).expect("Failed to convert to string"));

        if data.find(prompt).is_some() {
            break;
        };

        if now.elapsed().as_secs() > 1 {
            break;
        };
    }

    match data.find(expect) {
        Some(_) => true,
        None => {
            info!("\"{expect}\" not found in:\n{data}");
            false
        }
    }
}

impl SerialProvider for TiXdsProvider {
    fn handle(&mut self, device: &crate::udev::Device, _seqnum: u64) -> bool {
        let name = format!("{}.{}", PROVIDER, &self.name);
        let properties = device.properties(name);
        if !properties.matches(&self.parameters.match_) {
            debug!(
                "Ignoring device {} - {:?}",
                device.syspath().display(),
                properties,
            );

            return false;
        }

        if let Some(node) = device.devnode() {
            let mut port = match tokio_serial::new(node.to_string_lossy(), 9600).open_native_async()
            {
                Ok(port) => port,
                Err(e) => {
                    warn!("Failed to open serial port: {e}");
                    return false;
                }
            };

            // Read the version string to ensure we are connected to a device configured as
            // the Test Automation Interface
            if !command(&mut port, "version\n", "=>", "Test Automation Interface") {
                warn!("Test Automation interface does not appear to be present");
                return false;
            };

            // Need to set DUT type
            let type_command = format!("auto set dut {}\n", self.parameters.model);
            if !command(&mut port, &type_command, "=>", "DUT set to") {
                warn!("Unable to set DUT type: {}", self.parameters.model);
                return false;
            };

            let actuator = TiXdsActuator::new(port);

            let id = self.server.register_actuator(properties, actuator);

            self.id = Some(id);

            return true;
        }

        false
    }

    fn remove(&mut self, device: &Device) {
        let properties = device.properties(&self.name);
        if !properties.matches(&self.parameters.match_) {
            return;
        }

        match self.id {
            None => (),
            Some(id) => {
                self.server.unregister_actuator(id);
            }
        };
    }
}

#[derive(Debug)]
pub struct TiXdsActuator {
    port: Mutex<tokio_serial::SerialStream>,
}

impl TiXdsActuator {
    fn new(port: tokio_serial::SerialStream) -> Self {
        Self {
            port: Mutex::new(port),
        }
    }

    fn power(&self, on: bool) -> Result<(), ActuatorError> {
        let send = format!("auto power {}\n", if on { "on" } else { "off" });
        let expect = format!("Powering {} DUT", if on { "On" } else { "Off" });

        let mut handle = match self.port.lock() {
            Ok(handle) => handle,
            Err(e) => {
                warn!("Failed to enable access to device: {e}");
                return Err(ActuatorError {});
            }
        };

        if !command(&mut handle, send.as_str(), "=>", expect.as_str()) {
            warn!("Failed to change power state");
            return Err(ActuatorError {});
        };

        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::Actuator for TiXdsActuator {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), ActuatorError> {
        #[derive(Deserialize)]
        struct ModeParameters {
            mode: String,
        }
        let parameters = ModeParameters::deserialize(parameters).unwrap();
        match parameters.mode.as_str() {
            "on" => TiXdsActuator::power(self, true),
            "off" => TiXdsActuator::power(self, false),
            _ => todo!(),
        }
    }
}
