use serde::Deserialize;
use tracing::{instrument, warn};

use crate::{
    ActuatorError, Server,
    registry::{self, Properties},
};

pub const PROVIDER: &str = "uhubctl";

#[derive(Deserialize, Debug)]
struct PortConfig {
    name: String,
    port: u32,
}

#[derive(Deserialize, Debug)]
struct UhubctlParameters {
    location: String,
    path: Option<String>,
    ports: Vec<PortConfig>,
}

#[instrument(skip(parameters, server))]
pub fn start_provider(name: String, parameters: serde_yaml::Value, server: Server) {
    let parameters: UhubctlParameters = serde_yaml::from_value(parameters).unwrap();
    let provider_properties = &[
        (registry::PROVIDER_NAME, name.as_str()),
        (registry::PROVIDER, PROVIDER),
    ];

    for port_config in &parameters.ports {
        let actuator_name = format!("{}.{}.port-{}", name, parameters.location, port_config.port);
        let mut properties = Properties::new(actuator_name);
        properties.extend(provider_properties);
        properties.insert("uhubctl.location", parameters.location.clone());
        properties.insert("uhubctl.name", port_config.name.clone());
        properties.insert("uhubctl.port", port_config.port.to_string());

        let actuator = UhubctlActuator {
            path: parameters
                .path
                .clone()
                .unwrap_or_else(|| "/usr/sbin/uhubctl".to_string()),
            location: parameters.location.clone(),
            port: port_config.port,
        };
        server.register_actuator(properties, actuator);
    }
}

#[derive(Debug)]
struct UhubctlActuator {
    path: String,
    location: String,
    port: u32,
}

#[async_trait::async_trait]
impl crate::Actuator for UhubctlActuator {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), ActuatorError> {
        #[derive(Deserialize)]
        struct ModeParameters {
            mode: String,
        }
        let parameters = ModeParameters::deserialize(parameters).unwrap();
        let action = match parameters.mode.as_str() {
            "on" => "on",
            "off" => "off",
            _ => {
                warn!(
                    "Unsupported mode '{}' for uhubctl actuator at {}:{}",
                    parameters.mode, self.location, self.port
                );
                return Err(ActuatorError());
            }
        };

        let status = tokio::process::Command::new(&self.path)
            .args([
                "-l",
                &self.location,
                "-p",
                &self.port.to_string(),
                "-a",
                action,
            ])
            .status()
            .await
            .map_err(|e| {
                warn!("Failed to run uhubctl: {e}");
                ActuatorError()
            })?;

        if status.success() {
            Ok(())
        } else {
            warn!(
                "uhubctl exited with status {} for {}:{}",
                status, self.location, self.port
            );
            Err(ActuatorError())
        }
    }
}
