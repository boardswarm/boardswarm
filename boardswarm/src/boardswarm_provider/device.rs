use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use boardswarm_cli::client::Boardswarm;
use futures::{pin_mut, Stream, StreamExt};
use tokio::sync::broadcast;
use tracing::warn;

use crate::{DeviceMonitor, DeviceSetModeError};

use super::Provider;

#[derive(Clone)]
pub struct BoardswarmDevice {
    id: u64,
    remote: Boardswarm,
    notifier: broadcast::Sender<()>,
    inner: Arc<Mutex<BoardswarmDeviceInner>>,
}

struct BoardswarmDeviceInner {
    // Remote to local mapping
    console_mapping: HashMap<u64, u64>,
    volume_mapping: HashMap<u64, u64>,
    provider: Arc<Provider>,
    info: boardswarm_protocol::Device,
}

impl BoardswarmDevice {
    pub async fn new(
        id: u64,
        mut remote: Boardswarm,
        provider: Arc<Provider>,
    ) -> Result<Self, tonic::Status> {
        let mut stream = remote.device_info(id).await?;
        let Some(info) = stream.next().await.transpose()? else {
            return Err(tonic::Status::unavailable("Device not available"));
        };

        let inner = Arc::new(Mutex::new(BoardswarmDeviceInner::new(info, provider)));

        let notifier = broadcast::channel(1).0;
        let s = Self {
            id,
            remote,
            notifier,
            inner,
        };
        let s_clone = s.clone();
        tokio::spawn(monitor_device(s_clone, stream));

        Ok(s)
    }
}

impl BoardswarmDeviceInner {
    fn new(info: boardswarm_protocol::Device, provider: Arc<Provider>) -> Self {
        let mut inner = BoardswarmDeviceInner {
            console_mapping: HashMap::new(),
            volume_mapping: HashMap::new(),
            provider,
            info,
        };
        inner.update_mappings();
        inner
    }

    fn update_mappings(&mut self) {
        // Update mappings */
        self.console_mapping.clear();
        for c in &self.info.consoles {
            if let Some(remote) = c.id {
                if let Some(local) = self.provider.console_id(remote) {
                    self.console_mapping.insert(remote, local);
                }
            }
        }

        self.volume_mapping.clear();
        for c in &self.info.volumes {
            if let Some(remote) = c.id {
                if let Some(local) = self.provider.volume_id(remote) {
                    self.volume_mapping.insert(remote, local);
                }
            }
        }
    }

    // Check if the remote id provider had relevant changes changing our mappings
    fn provider_sync(&mut self) -> bool {
        let mut changed = false;
        for (remote, local) in &self.console_mapping {
            if self.provider.console_id(*remote) != Some(*local) {
                changed = true
            }
        }
        for (remote, local) in &self.volume_mapping {
            if self.provider.volume_id(*remote) != Some(*local) {
                changed = true
            }
        }
        if changed {
            self.update_mappings();
        }

        changed
    }

    fn update(&mut self, info: boardswarm_protocol::Device) {
        self.info = info;
        self.update_mappings();
    }
}

async fn monitor_device(
    device: BoardswarmDevice,
    updates: impl Stream<Item = Result<boardswarm_protocol::Device, tonic::Status>>,
) {
    pin_mut!(updates);
    let mut watch = device.inner.lock().unwrap().provider.watch();

    loop {
        tokio::select! {
            item = updates.next() => {
                match item {
                    Some(Ok(info)) => {
                        let mut inner = device.inner.lock().unwrap();
                        inner.update(info);
                        let _ = device.notifier.send(());
                    }
                    Some(Err(e)) => {
                        warn!("Device update error: {e}");
                        return;
                    }
                    None => return,
                }
            }
            _ = watch.recv() => {
                let mut inner = device.inner.lock().unwrap();
                if inner.provider_sync() {
                    let _ = device.notifier.send(());
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::Device for BoardswarmDevice {
    async fn set_mode(&self, mode: &str) -> Result<(), DeviceSetModeError> {
        let mut client = self.remote.clone();
        client
            .device_change_mode(self.id, mode.to_string())
            .await
            .map_err(|e| match e.code() {
                tonic::Code::NotFound => DeviceSetModeError::ModeNotFound,
                tonic::Code::FailedPrecondition => DeviceSetModeError::WrongCurrentMode,
                tonic::Code::Aborted => DeviceSetModeError::ActuatorFailed(crate::ActuatorError {}),
                _ => DeviceSetModeError::ActuatorFailed(crate::ActuatorError {}),
            })
    }

    fn updates(&self) -> DeviceMonitor {
        DeviceMonitor {
            receiver: self.notifier.subscribe(),
        }
    }

    fn consoles(&self) -> Vec<crate::DeviceConsole> {
        let inner = self.inner.lock().unwrap();
        inner
            .info
            .consoles
            .iter()
            .map(|c| crate::DeviceConsole {
                name: c.name.to_string(),
                id: c.id.and_then(|id| inner.console_mapping.get(&id).copied()),
            })
            .collect()
    }

    fn volumes(&self) -> Vec<crate::DeviceVolume> {
        let inner = self.inner.lock().unwrap();
        inner
            .info
            .volumes
            .iter()
            .map(|v| crate::DeviceVolume {
                name: v.name.to_string(),
                id: v.id.and_then(|id| inner.volume_mapping.get(&id).copied()),
            })
            .collect()
    }

    fn modes(&self) -> Vec<crate::DeviceMode> {
        let inner = self.inner.lock().unwrap();
        inner
            .info
            .modes
            .iter()
            .map(|m| crate::DeviceMode {
                name: m.name.to_string(),
                depends: m.depends.clone(),
                available: m.available,
            })
            .collect()
    }

    fn current_mode(&self) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        inner.info.current_mode.clone()
    }
}
