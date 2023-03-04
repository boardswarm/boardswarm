use std::collections::HashMap;
use std::time::Duration;

use boardswarm_cli::client::{Boardswarm, ItemEvent};
use boardswarm_protocol::ItemType;
use futures::{pin_mut, TryStreamExt};
use serde::Deserialize;
use tokio::join;
use tonic::transport::Uri;
use tracing::info;
use tracing::trace;

use crate::{registry::Properties, Server};

use self::actuator::BoardswarmActuator;
use self::console::BoardswarmConsole;

mod actuator;
mod console;

#[derive(Deserialize, Debug)]
struct BoardswarmParameters {
    uri: String,
}

async fn add_item(
    type_: ItemType,
    server: &Server,
    mut remote: Boardswarm,
    id: u64,
    instance: &str,
) -> Option<u64> {
    let properties = remote.properties(type_, id).await.unwrap();
    let mut properties: Properties = properties.into();
    properties.insert(crate::registry::INSTANCE, instance);

    match type_ {
        ItemType::Console => {
            Some(server.register_console(properties, BoardswarmConsole::new(id, remote)))
        }
        ItemType::Actuator => {
            Some(server.register_actuator(properties, BoardswarmActuator::new(id, remote)))
        }
        _ => unimplemented!(),
    }
}

fn remove_item(type_: ItemType, server: &Server, id: u64) {
    match type_ {
        ItemType::Console => server.unregister_console(id),
        ItemType::Actuator => server.unregister_actuator(id),
        _ => unimplemented!(),
    }
}

async fn monitor_items(type_: ItemType, mut remote: Boardswarm, server: Server, instance: &str) {
    // remote id to local id mapping
    let mut mapping = HashMap::new();
    let monitor = remote.monitor(type_).await.unwrap();
    pin_mut!(monitor);
    while let Ok(Some(event)) = monitor.try_next().await {
        match event {
            ItemEvent::Added(items) => {
                for i in items {
                    if let Some(id) = add_item(type_, &server, remote.clone(), i.id, instance).await
                    {
                        mapping.insert(i.id, id);
                    }
                }
            }
            ItemEvent::Removed(removed) => {
                if let Some(local) = mapping.remove(&removed) {
                    remove_item(type_, &server, local);
                }
            }
        }
    }

    // If the connection breaks; drop all registrations
    for id in mapping.values() {
        remove_item(type_, &server, *id);
    }
}

pub fn start_provider(name: String, parameters: serde_yaml::Value, server: Server) {
    let parameters: BoardswarmParameters = serde_yaml::from_value(parameters).unwrap();
    let uri: Uri = parameters.uri.parse().unwrap();

    tokio::spawn(async move {
        loop {
            let _span = tracing::span!(tracing::Level::INFO, "boardswarm", name);
            if let Ok(remote) = Boardswarm::connect(uri.clone()).await {
                trace!("Connected to {}", name);
                let consoles =
                    monitor_items(ItemType::Console, remote.clone(), server.clone(), &name);
                let actuators = monitor_items(ItemType::Actuator, remote, server.clone(), &name);

                join!(consoles, actuators);
                info!("Connection to {} failed", name);
            }
            // TODO move to exponential backoff
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}
