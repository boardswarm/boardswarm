use reqwest::Client;
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum PduDaemonError {
    #[error("Could not parse url")]
    ParseUrlError(#[from] url::ParseError),
    #[error("Could not parse url")]
    ReqwestError(#[from] reqwest::Error),
}

#[derive(Debug, Clone)]
pub struct PduDaemon {
    client: Client,
    url: Url,
}

impl PduDaemon {
    pub fn new(url: &str) -> Result<Self, PduDaemonError> {
        let client = Client::new();
        let url = url.parse()?;
        Ok(Self { client, url })
    }

    fn build_url(&self, command: &str, hostname: &str, port: u16) -> Result<Url, PduDaemonError> {
        let mut url = self.url.join("power/control/")?.join(command)?;

        url.query_pairs_mut()
            .append_pair("hostname", hostname)
            .append_pair("port", &port.to_string());
        Ok(url)
    }

    async fn send(&self, url: Url) -> Result<(), PduDaemonError> {
        self.client.get(url).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn on(&self, hostname: &str, port: u16) -> Result<(), PduDaemonError> {
        let url = self.build_url("on", hostname, port)?;
        self.send(url).await
    }

    pub async fn off(&self, hostname: &str, port: u16) -> Result<(), PduDaemonError> {
        let url = self.build_url("off", hostname, port)?;
        self.send(url).await
    }

    pub async fn reboot(
        &self,
        hostname: &str,
        port: u16,
        delay: Option<u32>,
    ) -> Result<(), PduDaemonError> {
        let mut url = self.build_url("on", hostname, port)?;
        if let Some(delay) = delay {
            url.query_pairs_mut()
                .append_pair("delay", &delay.to_string());
        }
        self.send(url).await
    }
}
