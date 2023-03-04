use boardswarm_cli::client::Boardswarm;
use boardswarm_protocol::Parameters;
use serde::Deserialize;

#[derive(Debug)]
pub struct BoardswarmActuator {
    id: u64,
    remote: Boardswarm,
}

impl BoardswarmActuator {
    pub fn new(id: u64, remote: Boardswarm) -> Self {
        Self { id, remote }
    }
}

#[async_trait::async_trait]
impl crate::Actuator for BoardswarmActuator {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), crate::ActuatorError> {
        let mut remote = self.remote.clone();
        let parameters = Parameters::deserialize(parameters).unwrap();
        remote
            .actuator_change_mode(self.id, parameters)
            .await
            .unwrap();
        Ok(())
    }
}
