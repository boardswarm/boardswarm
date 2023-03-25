use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use boardswarm_cli::client::{Boardswarm, ItemEvent};
use boardswarm_protocol::ItemType;
use futures::{pin_mut, TryStreamExt};
use serde::Deserialize;
use tokio::join;
use tokio::sync::broadcast;
use tonic::transport::Uri;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::{registry::Properties, Server};

use self::actuator::BoardswarmActuator;
use self::console::BoardswarmConsole;
use self::device::BoardswarmDevice;

mod actuator;
mod console;
mod device;

#[derive(Deserialize, Debug)]
struct BoardswarmParameters {
    uri: String,
}

pub struct Provider {
    /* Remote to local mappings */
    actuators: Mutex<HashMap<u64, u64>>,
    consoles: Mutex<HashMap<u64, u64>>,
    devices: Mutex<HashMap<u64, u64>>,
    volumes: Mutex<HashMap<u64, u64>>,
    notifier: broadcast::Sender<()>,
}

impl Provider {
    fn new() -> Self {
        let actuators = Mutex::new(HashMap::new());
        let consoles = Mutex::new(HashMap::new());
        let volumes = Mutex::new(HashMap::new());
        let devices = Mutex::new(HashMap::new());
        Self {
            actuators,
            consoles,
            volumes,
            devices,
            notifier: broadcast::channel(1).0,
        }
    }

    pub fn console_id(&self, remote: u64) -> Option<u64> {
        self.consoles.lock().unwrap().get(&remote).copied()
    }

    #[allow(dead_code)]
    pub fn actuator_id(&self, remote: u64) -> Option<u64> {
        self.actuators.lock().unwrap().get(&remote).copied()
    }

    pub fn volume_id(&self, remote: u64) -> Option<u64> {
        self.volumes.lock().unwrap().get(&remote).copied()
    }

    pub fn watch(&self) -> broadcast::Receiver<()> {
        self.notifier.subscribe()
    }
}

async fn add_item(
    provider: Arc<Provider>,
    type_: ItemType,
    server: &Server,
    mut remote: Boardswarm,
    id: u64,
    instance: &str,
) {
    let properties = remote.properties(type_, id).await.unwrap();
    let mut properties: Properties = properties.into();
    properties.insert(crate::registry::INSTANCE, instance);

    match type_ {
        ItemType::Console => {
            let local = server.register_console(properties, BoardswarmConsole::new(id, remote));
            provider.consoles.lock().unwrap().insert(id, local);
        }
        ItemType::Actuator => {
            let local = server.register_actuator(properties, BoardswarmActuator::new(id, remote));
            provider.actuators.lock().unwrap().insert(id, local);
        }
        ItemType::Device => match BoardswarmDevice::new(id, remote, provider.clone()).await {
            Ok(d) => {
                let local = server.register_device(properties, d);
                provider.devices.lock().unwrap().insert(id, local);
            }
            Err(e) => warn!("Failed to montior remote device: {e}"),
        },
        _ => unimplemented!(),
    }
    let _ = provider.notifier.send(());
}

fn remove_item(provider: &Provider, type_: ItemType, server: &Server, id: u64) {
    match type_ {
        ItemType::Console => {
            let mut consoles = provider.consoles.lock().unwrap();
            if let Some(local) = consoles.remove(&id) {
                server.unregister_console(local)
            }
        }
        ItemType::Actuator => {
            let mut actuators = provider.actuators.lock().unwrap();
            if let Some(local) = actuators.remove(&id) {
                server.unregister_console(local)
            }
        }
        ItemType::Device => {
            let mut devices = provider.devices.lock().unwrap();
            if let Some(local) = devices.remove(&id) {
                server.unregister_device(local)
            }
        }
        _ => unimplemented!(),
    }
    let _ = provider.notifier.send(());
}

async fn monitor_items(
    provider: Arc<Provider>,
    type_: ItemType,
    mut remote: Boardswarm,
    server: Server,
    instance: &str,
) {
    let monitor = remote.monitor(type_).await.unwrap();
    pin_mut!(monitor);
    while let Ok(Some(event)) = monitor.try_next().await {
        match event {
            ItemEvent::Added(items) => {
                for i in items {
                    add_item(
                        provider.clone(),
                        type_,
                        &server,
                        remote.clone(),
                        i.id,
                        instance,
                    )
                    .await
                }
            }
            ItemEvent::Removed(removed) => {
                remove_item(&provider, type_, &server, removed);
            }
        }
    }

    // If the connection breaks; drop all registrations
    match type_ {
        ItemType::Console => {
            for (_remote, local) in provider.consoles.lock().unwrap().drain() {
                server.unregister_console(local);
            }
        }
        ItemType::Actuator => {
            for (_remote, local) in provider.actuators.lock().unwrap().drain() {
                server.unregister_actuator(local);
            }
        }
        ItemType::Device => {
            for (_remote, local) in provider.devices.lock().unwrap().drain() {
                server.unregister_device(local);
            }
        }
        _ => unimplemented!(),
    }
}

pub fn start_provider(name: String, parameters: serde_yaml::Value, server: Server) {
    let provider = Arc::new(Provider::new());
    let parameters: BoardswarmParameters = serde_yaml::from_value(parameters).unwrap();
    let uri: Uri = parameters.uri.parse().unwrap();

    tokio::spawn(async move {
        loop {
            let _span = tracing::span!(tracing::Level::INFO, "boardswarm", name);
            if let Ok(remote) = Boardswarm::connect(uri.clone()).await {
                trace!("Connected to {}", name);
                let consoles = monitor_items(
                    provider.clone(),
                    ItemType::Console,
                    remote.clone(),
                    server.clone(),
                    &name,
                );
                let actuators = monitor_items(
                    provider.clone(),
                    ItemType::Actuator,
                    remote.clone(),
                    server.clone(),
                    &name,
                );
                let devices = monitor_items(
                    provider.clone(),
                    ItemType::Device,
                    remote,
                    server.clone(),
                    &name,
                );

                join!(consoles, actuators, devices);
                info!("Connection to {} failed", name);
            }
            // TODO move to exponential backoff
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}
