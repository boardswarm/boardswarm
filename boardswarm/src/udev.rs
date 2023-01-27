use std::{collections::HashMap, sync::Arc};

use crate::{dfu::Dfu, Server};
use futures::StreamExt;
use tracing::info;

pub async fn add_serial(server: &Server, device: tokio_udev::Device) {
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

pub async fn monitor_serial(server: Server) {
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
        add_serial(&server, d).await;
    }

    while let Some(Ok(event)) = monitor.next().await {
        match event.event_type() {
            tokio_udev::EventType::Add => add_serial(&server, event.device()).await,
            _ => info!(
                "Udev event: {:?} {:?}",
                event.event_type(),
                event.device().devnode()
            ),
        }
    }
}

pub async fn add_dfu(server: &Server, device: tokio_udev::Device) -> Option<Arc<Dfu>> {
    let interface = device.property_value("ID_USB_INTERFACES")?;
    if interface != ":fe0102:" {
        return None;
    }

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

    let busnum = device
        .property_value("BUSNUM")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let devnum = device
        .property_value("DEVNUM")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let dfu = Arc::new(crate::dfu::Dfu::new(name, busnum, devnum, properties));
    server.register_uploader(dfu.clone());
    Some(dfu)
}

pub async fn monitor_dfu(server: Server) {
    let mut dfu_devices = HashMap::new();
    let monitor = tokio_udev::MonitorBuilder::new()
        .unwrap()
        .match_subsystem("usb")
        .unwrap()
        .listen()
        .unwrap();

    let mut monitor = tokio_udev::AsyncMonitorSocket::new(monitor).unwrap();

    let mut enumerator = tokio_udev::Enumerator::new().unwrap();
    enumerator.match_subsystem("usb").unwrap();
    let devices = enumerator.scan_devices().unwrap();
    for d in devices {
        if let Some(dfu) = add_dfu(&server, d).await {
            dfu_devices.insert(dfu.devnode().to_string(), dfu);
        }
    }

    while let Some(Ok(event)) = monitor.next().await {
        match event.event_type() {
            tokio_udev::EventType::Add => {
                if let Some(dfu) = add_dfu(&server, event.device()).await {
                    dfu_devices.insert(dfu.devnode().to_string(), dfu);
                }
            }
            tokio_udev::EventType::Remove => {
                if let Some(path) = event.device().devnode() {
                    let name = path.to_str().unwrap().to_owned();
                    if let Some(dfu) = dfu_devices.remove(&name) {
                        server.unregister_uploader(dfu.as_ref() as &dyn crate::Uploader);
                    }
                }
            }
            _ => (),
        }
    }
}

pub async fn start_provider(_name: String, server: Server) {
    tokio::task::spawn_local(monitor_serial(server.clone()));
    tokio::task::spawn_local(monitor_dfu(server));
}
