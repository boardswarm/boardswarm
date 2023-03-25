use std::sync::{Arc, Mutex};

use boardswarm_protocol::VolumeInfoMsg;
use bytes::Bytes;
use futures::{pin_mut, Stream, StreamExt};
use tokio::{select, sync::broadcast};
use tracing::info;

use crate::client::{Boardswarm, VolumeIo, VolumeIoRW};

#[derive(Debug, Clone)]
pub struct DeviceBuilder {
    client: Boardswarm,
}

impl DeviceBuilder {
    pub async fn new<D>(url: D) -> Result<Self, tonic::transport::Error>
    where
        D: TryInto<tonic::transport::Endpoint>,
        D::Error: Into<tonic::codegen::StdError>,
    {
        let client = Boardswarm::connect(url).await?;
        Ok(Self::from_client(client))
    }

    pub fn from_client(client: Boardswarm) -> Self {
        DeviceBuilder { client }
    }

    pub async fn by_id(self, id: u64) -> Result<Device, tonic::Status> {
        Device::new(self.client, id).await
    }

    pub async fn by_name(mut self, name: &str) -> Result<Option<Device>, tonic::Status> {
        let devices = self
            .client
            .list(boardswarm_protocol::ItemType::Device)
            .await?;
        let id = match devices.iter().find(|i| i.name == name) {
            Some(i) => i.id,
            None => return Ok(None),
        };
        Ok(Some(self.by_id(id).await?))
    }
}

#[derive(Clone)]
pub struct DeviceVolume {
    device: Device,
    volume: String,
}

impl DeviceVolume {
    fn new(device: Device, volume: String) -> Self {
        Self { device, volume }
    }

    // todo differention between console no (longer) available
    fn get_id(&self) -> Option<u64> {
        let d = self.device.inner.device.lock().unwrap();
        d.volumes
            .iter()
            .find(|u| u.name == self.volume)
            .and_then(|u| u.id)
    }

    pub fn available(&self) -> bool {
        self.get_id().is_some()
    }

    /// Wait for the volume to become available again
    pub async fn wait(&self) {
        let mut watch = self.device.watch();
        while !self.available() {
            watch.changed().await;
        }
    }

    pub async fn info(&mut self) -> Result<VolumeInfoMsg, tonic::Status> {
        if let Some(id) = self.get_id() {
            self.device.client.volume_info(id).await
        } else {
            Err(tonic::Status::unavailable("Volume currently not available"))
        }
    }

    pub async fn io(
        &mut self,
        target: &str,
        length: Option<u64>,
    ) -> Result<VolumeIo, tonic::Status> {
        let id = self
            .get_id()
            .ok_or_else(|| tonic::Status::unavailable("Volume currently not available"))?;
        self.device.client.volume_io(id, target, length).await
    }

    // TODO handle errors
    // TODO cache info
    pub async fn open<S: Into<String> + AsRef<str>>(
        &mut self,
        target: S,
        length: Option<u64>,
    ) -> Result<VolumeIoRW, tonic::Status> {
        let id = self
            .get_id()
            .ok_or_else(|| tonic::Status::unavailable("Volume currently not available"))?;
        self.device
            .client
            .volume_io_readwrite(id, target, length)
            .await
    }

    pub async fn commit(&mut self) -> Result<(), tonic::Status> {
        if let Some(id) = self.get_id() {
            self.device.client.volume_commit(id).await
        } else {
            Err(tonic::Status::unavailable("Volume currently not available"))
        }
    }
}

#[derive(Clone)]
pub struct DeviceConsole {
    device: Device,
    name: String,
}

impl DeviceConsole {
    fn new(device: Device, name: String) -> Self {
        Self { device, name }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    // todo differention between console no (longer) available
    pub fn get_id(&self) -> Option<u64> {
        let d = self.device.inner.device.lock().unwrap();
        d.consoles
            .iter()
            .find(|c| c.name == self.name)
            .and_then(|c| c.id)
    }

    pub fn available(&self) -> bool {
        self.get_id().is_some()
    }

    pub async fn stream_input<I>(&mut self, input: I) -> Result<(), tonic::Status>
    where
        I: Stream<Item = Bytes> + Send + 'static,
    {
        if let Some(id) = self.get_id() {
            self.device.client.console_stream_input(id, input).await
        } else {
            Err(tonic::Status::unavailable(
                "Console currently not available",
            ))
        }
    }

    pub async fn stream_output(&mut self) -> Result<impl Stream<Item = Bytes>, tonic::Status> {
        if let Some(id) = self.get_id() {
            self.device.client.console_stream_output(id).await
        } else {
            Err(tonic::Status::unavailable(
                "Console currently not available",
            ))
        }
    }
}

struct DeviceWatcher {
    watch: tokio::sync::watch::Receiver<()>,
}

impl DeviceWatcher {
    async fn changed(&mut self) {
        let _ = self.watch.changed().await;
    }
}

struct DeviceInner {
    notifier: tokio::sync::watch::Sender<()>,
    device: Mutex<boardswarm_protocol::Device>,
}

#[derive(Clone)]
pub struct Device {
    client: Boardswarm,
    id: u64,
    _shutdown: broadcast::Sender<()>,
    inner: Arc<DeviceInner>,
}

impl Device {
    pub async fn new(mut client: Boardswarm, id: u64) -> Result<Self, tonic::Status> {
        let mut monitor = client.device_info(id).await?;

        let device = match monitor.next().await {
            Some(v) => v,
            None => Err(tonic::Status::not_found("bla")),
        }?;
        let device = Mutex::new(device);
        let (shutdown, rx) = broadcast::channel(1);
        let notifier = tokio::sync::watch::channel(()).0;

        let inner = Arc::new(DeviceInner { notifier, device });
        tokio::spawn(Self::monitor(monitor, rx, inner.clone()));
        Ok(Self {
            client,
            id,
            _shutdown: shutdown,
            inner,
        })
    }

    fn watch(&self) -> DeviceWatcher {
        DeviceWatcher {
            watch: self.inner.notifier.subscribe(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub async fn change_mode<S: Into<String>>(&self, mode: S) -> Result<(), tonic::Status> {
        let mut client = self.client.clone();
        client.device_change_mode(self.id, mode.into()).await?;
        Ok(())
    }

    /// Get the default console
    pub fn console(&self) -> Option<DeviceConsole> {
        let d = self.inner.device.lock().unwrap();
        d.consoles
            .first()
            .map(|c| DeviceConsole::new(self.clone(), c.name.clone()))
    }

    pub fn consoles(&self) -> Vec<DeviceConsole> {
        let d = self.inner.device.lock().unwrap();
        d.consoles
            .iter()
            .map(|c| DeviceConsole::new(self.clone(), c.name.clone()))
            .collect()
    }

    pub fn console_by_name(&self, name: &str) -> Option<DeviceConsole> {
        let d = self.inner.device.lock().unwrap();
        d.consoles
            .iter()
            .find(|c| c.name == name)
            .map(|c| DeviceConsole::new(self.clone(), c.name.clone()))
    }

    pub fn volumes(&self) -> Vec<DeviceVolume> {
        let d = self.inner.device.lock().unwrap();
        d.volumes
            .iter()
            .map(|u| DeviceVolume::new(self.clone(), u.name.clone()))
            .collect()
    }

    pub fn volume_by_name(&self, name: &str) -> Option<DeviceVolume> {
        let d = self.inner.device.lock().unwrap();
        d.volumes
            .iter()
            .find(|u| u.name == name)
            .map(|u| DeviceVolume::new(self.clone(), u.name.clone()))
    }

    async fn monitor<M>(monitor: M, mut shutdown: broadcast::Receiver<()>, inner: Arc<DeviceInner>)
    where
        M: futures::Stream<Item = Result<boardswarm_protocol::Device, tonic::Status>> + 'static,
    {
        pin_mut!(monitor);
        loop {
            select! {
                _ = shutdown.recv() => { break },
                update = monitor.next() => {
                    if let Some(Ok(update))  = update {
                        info!("=> {:#?}", update);
                        *inner.device.lock().unwrap() = update;
                        let _ = inner.notifier.send(());
                    } else {
                        break;
                    }
                }
            }
        }
    }
}
