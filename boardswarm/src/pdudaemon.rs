use pdudaemon_client::PduDaemon;
use serde::Deserialize;

use crate::{
    registry::{self, Properties},
    Server,
};

pub const PROVIDER: &str = "pdudaemon";

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum Ports {
    Num(u16),
    Ports(Vec<String>),
}

#[derive(Deserialize, Debug)]
struct Pdu {
    name: String,
    ports: Ports,
}

#[derive(Deserialize, Debug)]
struct PduDaemonParameters {
    uri: String,
    pdus: Vec<Pdu>,
}

pub fn start_provider(name: String, parameters: serde_yaml::Value, server: Server) {
    let parameters: PduDaemonParameters = serde_yaml::from_value(parameters).unwrap();
    let provider_properties = &[
        (registry::PROVIDER_NAME, name.as_str()),
        (registry::PROVIDER, PROVIDER),
    ];

    let daemon = PduDaemon::new(&parameters.uri).unwrap();
    for pdu in parameters.pdus {
        match pdu.ports {
            Ports::Num(ports) => {
                for i in 1..=ports {
                    let name = format!("{}.{}.port-{}", name, pdu.name, i);
                    let mut properties = Properties::new(name);
                    properties.extend(provider_properties);
                    let actuator =
                        PduDaemonActuator::new(daemon.clone(), pdu.name.clone(), i.to_string());
                    server.register_actuator(properties, actuator);
                }
            }
            Ports::Ports(ports) => {
                for i in ports {
                    let name = format!("{}.{}.port-{}", name, pdu.name, i);
                    let mut properties = Properties::new(name);
                    properties.extend(provider_properties);
                    let actuator =
                        PduDaemonActuator::new(daemon.clone(), pdu.name.clone(), i.to_string());
                    server.register_actuator(properties, actuator);
                }
            }
        }
    }
}

#[derive(Debug)]
struct PduDaemonActuator {
    daemon: PduDaemon,
    hostname: String,
    port: String,
}

impl PduDaemonActuator {
    fn new(daemon: PduDaemon, hostname: String, port: String) -> Self {
        Self {
            daemon,
            hostname,
            port,
        }
    }
}

#[async_trait::async_trait]
impl crate::Actuator for PduDaemonActuator {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), crate::ActuatorError> {
        #[derive(Deserialize)]
        struct ModeParameters {
            mode: String,
        }
        let parameters = ModeParameters::deserialize(parameters).unwrap();
        match parameters.mode.as_str() {
            "on" => self.daemon.on(&self.hostname, &self.port).await.unwrap(),
            "off" => self.daemon.off(&self.hostname, &self.port).await.unwrap(),
            _ => todo!(),
        }
        Ok(())
    }
}
