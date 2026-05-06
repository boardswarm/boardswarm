use crate::{
    registry::Properties,
    serial::SerialProvider,
    udev::{Device, DeviceRegistrations, PreRegistration},
    ActuatorError, Server,
};

use regex::Regex;

use serde::Deserialize;

use std::{
    borrow::Cow::{Borrowed, Owned},
    collections::HashMap,
    io,
    io::ErrorKind,
    path::PathBuf,
    str,
    sync::Mutex,
    time::Instant,
};

use tokio_serial::SerialPortBuilderExt;

use tracing::{debug, info, warn};

pub const PROVIDER: &str = "ti-xds-tai";

#[derive(Deserialize, Debug, Default)]
struct TiXdsParameters {
    #[serde(rename = "match")]
    match_: HashMap<String, String>,
    #[serde(default)]
    model: Option<String>,
}

pub struct TiXdsProvider {
    name: String,
    registrations: DeviceRegistrations,
    parameters: TiXdsParameters,
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
            registrations: DeviceRegistrations::new(server),
            parameters,
        }
    }
}

impl SerialProvider for TiXdsProvider {
    fn handle(&mut self, device: &crate::udev::Device, seqnum: u64) -> bool {
        let name = format!("{}.{}", PROVIDER, &self.name);
        let mut properties = device.properties(name);
        if !properties.matches(&self.parameters.match_) {
            debug!(
                "Ignoring device {} - {:?}",
                device.syspath().display(),
                properties,
            );

            return false;
        }

        // Need to set DUT type. We provide 2 ways to do this:
        //  - Specify the model in the configuration file (which realistically needs
        //    device specific matching, such as ID_PATH or serial number
        //  - Autodetection via serial number. TI appear to be prefixing their device
        //    serial numbers with a model specific code, where possible utilise this
        //    if a model isn't specifically provided.
        let model;

        match &self.parameters.model {
            Some(value) => {
                model = value.clone();
            }
            None => {
                let Some(serial) = properties.get("udev.ID_SERIAL_SHORT") else {
                    warn!("Failed to find device serial number");
                    // Return true so device isn't picked up by another provider
                    return false;
                };

                debug!("Serial Number: {serial}");

                model = match &serial[0..4] {
                    "S62A" => "am62xx-evm".to_string(),
                    "S62G" => "am62pxx-sk".to_string(),
                    _ => {
                        warn!("Failed to determine device model");
                        // Return true so device isn't picked up by another provider
                        return false;
                    }
                };
            }
        }

        debug!("Model: {}", model);

        properties.insert("model".to_string(), model.to_string());

        if let Some(node) = device.devnode() {
            let prereg = self.registrations.pre_register(device, seqnum);
            tokio::spawn(setup_connection(prereg, node.to_path_buf(), properties));

            return true;
        }
        false
    }

    fn remove(&mut self, device: &Device) {
        self.registrations.remove(device);
    }
}

async fn setup_connection(r: PreRegistration, node: PathBuf, properties: Properties) {
    info!("Setting up TI TAI connection for {}", node.display());
    let port = match tokio_serial::new(node.to_string_lossy(), 9600).open_native_async() {
        Ok(port) => port,
        Err(e) => {
            warn!("Failed to open serial port: {e}");
            return;
        }
    };

    let connection = TiXdsConnection::new(port, "=>");

    // Read the version string to ensure we are connected to a device configured as
    // the Test Automation Interface
    let _ = connection.command("version\n");
    if !connection.contains("Test Automation Interface") {
        warn!("Test Automation interface does not appear to be present");
        // Return true so device isn't picked up by another provider
        return;
    };

    let model = properties.get("model").unwrap();
    let type_command = format!("auto set dut {}\n", model);
    let _ = connection.command(&type_command);
    if connection.contains("Error setting dut") {
        warn!("Unable to set DUT type: {}", model);
        // Return true so device isn't picked up by another provider
        return;
    };

    // Retrieve available modes
    let mut modes = HashMap::new();
    let _ = connection.command("auto boot list\n");
    let re = Regex::new("(?<name>[a-z]+) - (?<code>[0-9a-zA-Z]+)").unwrap();
    match connection.output() {
        Some(output) => {
            for mode in re.captures_iter(output.as_str()) {
                modes.insert(mode["name"].to_string(), mode["code"].to_string());
            }
        }
        None => {
            warn!("Unable to parse boot modes");
            // Return true so device isn't picked up by another provider
            return;
        }
    }

    debug!("Modes: {modes:#?}");

    r.register_actuator(properties, TiXdsActuator::new(connection, modes));
}

#[derive(Debug)]
struct TiXdsConnection {
    port: Mutex<tokio_serial::SerialStream>,
    prompt: Mutex<String>,
    buf: Mutex<String>,
}

impl TiXdsConnection {
    pub fn new(port: tokio_serial::SerialStream, prompt: &str) -> Self {
        Self {
            port: Mutex::new(port),
            prompt: Mutex::new(prompt.to_string().clone()),
            buf: Mutex::new(String::new()),
        }
    }

    pub fn command(&self, command: &str) -> Result<(), io::Error> {
        let mut handle = self.port.lock().unwrap();

        debug!("Sending: {command}");
        match handle.try_write(command.as_bytes()) {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("Failed to write serial port: {e}");
                Err(e)
            }
        }
    }

    fn expect(&self, expect: &str) -> Option<String> {
        let now = Instant::now();

        loop {
            let mut handle = self.port.lock().unwrap();
            let mut buf = self.buf.lock().unwrap();

            let mut data: Vec<u8> = vec![0; 100];
            let count = match handle.try_read(&mut data) {
                Ok(num) => num,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(e) => {
                    warn!("Failed to read serial port: {e}");
                    break;
                }
            };

            match String::from_utf8_lossy(&data[0..count]) {
                Owned(data) => {
                    buf.push_str(data.as_str());
                }
                Borrowed(data) => {
                    buf.push_str(data);
                }
            };

            if let Some(split) = buf.find(expect) {
                let remainder = buf.split_off(split + expect.len());
                let ret = buf.clone();
                *buf = remainder;
                debug!("Buf:\n{buf}");
                debug!("Returning:\n{ret}");
                return Some(ret);
            };

            if now.elapsed().as_secs() > 1 {
                break;
            };
        }

        None
    }

    pub fn output(&self) -> Option<String> {
        let prompt = self.prompt.lock().unwrap();
        self.expect(prompt.as_str())
    }

    pub fn contains(&self, pattern: &str) -> bool {
        let prompt = self.prompt.lock().unwrap();
        if let Some(output) = self.expect(prompt.as_str()) {
            output.contains(pattern)
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn prompt(&mut self, prompt: &str) {
        let mut handle = self.prompt.lock().unwrap();
        *handle = prompt.to_string().clone();
    }
}

#[derive(Debug)]
pub struct TiXdsActuator {
    connection: TiXdsConnection,
    modes: HashMap<String, String>,
}

impl TiXdsActuator {
    fn new(connection: TiXdsConnection, modes: HashMap<String, String>) -> Self {
        Self { connection, modes }
    }

    fn power(&self, on: bool) -> Result<(), ActuatorError> {
        let send = format!("auto power {}\n", if on { "on" } else { "off" });
        let value = format!("Powering {} DUT", if on { "On" } else { "Off" });

        let _ = self.connection.command(send.as_str());
        if !self.connection.contains(value.as_str()) {
            warn!("Failed to change power state");
            return Err(ActuatorError {});
        };

        Ok(())
    }

    fn boot_mode(&self, mode: &str) -> Result<(), ActuatorError> {
        let send = format!("auto sysboot {}\n", self.modes[mode]);

        let _ = self.connection.command(send.as_str());
        if !self.connection.contains("Sysboot set on DUT") {
            warn!("Failed to change boot mode");
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
            power: bool,
            #[serde(default)]
            mode: String,
        }
        let parameters = ModeParameters::deserialize(parameters).unwrap();
        let mode = parameters.mode.as_str();

        if !mode.is_empty() {
            if self.modes.contains_key(mode) {
                let _ = TiXdsActuator::boot_mode(self, mode);
            } else {
                warn!("Boot mode \"{mode}\" not recognised");
                return Err(ActuatorError {});
            }
        }

        self.power(parameters.power)
    }
}
