use std::sync::Arc;

use crate::Server;
use futures::StreamExt;
use tracing::info;

pub async fn add_device(server: &Server, device: tokio_udev::Device) {
    let name = device.devnode().unwrap().to_str().unwrap().to_owned();
    let properties = device
        .properties()
        .map(|a| {
            (
                a.name().to_string_lossy().into_owned(),
                a.value().to_string_lossy().into_owned(),
            )
        })
        .collect();

    let port = crate::serial::SerialPort::new(name, properties);
    server.register_console(Arc::new(port));
}

pub async fn start_provider(_name: String, server: Server) {
    let monitor = tokio_udev::MonitorBuilder::new()
        .unwrap()
        .match_subsystem("tty")
        .unwrap()
        .listen()
        .unwrap();

    let mut monitor = tokio_udev::AsyncMonitorSocket::new(monitor).unwrap();

    let mut enumerator = tokio_udev::Enumerator::new().unwrap();
    enumerator.match_subsystem("tty").unwrap();
    let devices = enumerator.scan_devices().unwrap();
    for d in devices {
        if d.parent().is_none() {
            continue;
        }
        add_device(&server, d).await;
    }

    while let Some(Ok(event)) = monitor.next().await {
        match event.event_type() {
            tokio_udev::EventType::Add => add_device(&server, event.device()).await,
            _ => info!(
                "Udev event: {:?} {:?}",
                event.event_type(),
                event.device().devnode()
            ),
        }
    }
}
