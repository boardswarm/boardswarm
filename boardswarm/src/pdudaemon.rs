use std::sync::Arc;

use pdudaemon_client::PduDaemon;
use serde::Deserialize;

use crate::Server;

#[derive(Deserialize, Debug)]
struct Pdu {
    name: String,
    ports: u16,
}

#[derive(Deserialize, Debug)]
struct PduDaemonParameters {
    uri: String,
    pdus: Vec<Pdu>,
}

pub fn start_provider(name: String, parameters: serde_yaml::Value, server: Server) {
    let parameters: PduDaemonParameters = serde_yaml::from_value(parameters).unwrap();

    let daemon = PduDaemon::new(&parameters.uri).unwrap();
    for pdu in parameters.pdus {
        for i in 1..=pdu.ports {
            let name = format!("{}.{}.port-{}", name, pdu.name, i);
            let actuator = PduDaemonActuator::new(name, daemon.clone(), pdu.name.clone(), i);
            server.register_actuator(Arc::new(actuator));
        }
    }
}

#[derive(Debug)]
struct PduDaemonActuator {
    name: String,
    daemon: PduDaemon,
    hostname: String,
    port: u16,
}

impl PduDaemonActuator {
    fn new(name: String, daemon: PduDaemon, hostname: String, port: u16) -> Self {
        Self {
            name,
            daemon,
            hostname,
            port,
        }
    }
}

#[async_trait::async_trait]
impl crate::Actuator for PduDaemonActuator {
    fn name(&self) -> &str {
        &self.name
    }

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
            "on" => self.daemon.on(&self.hostname, self.port).await.unwrap(),
            "off" => self.daemon.off(&self.hostname, self.port).await.unwrap(),
            _ => todo!(),
        }
        Ok(())
    }
}
