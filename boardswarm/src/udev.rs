use std::collections::HashMap;

use crate::{registry::Properties, Server};
use futures::StreamExt;
use tracing::info;

pub fn properties_from_device(name: String, device: &tokio_udev::Device) -> Properties {
    let mut properties = Properties::new(name);
    for p in device.properties() {
        let key = format!("udev.{}", p.name().to_string_lossy());
        let value = p.value().to_string_lossy();
        properties.insert(key, value);
    }

    properties
}

pub async fn add_serial(server: &Server, device: &tokio_udev::Device) -> Option<u64> {
    if let Some(node) = device.devnode() {
        if let Some(name) = node.file_name() {
            let name = name.to_string_lossy().into_owned();
            let path = node.to_string_lossy().into_owned();
            let properties = properties_from_device(name, device);
            let port = crate::serial::SerialPort::new(path);
            return Some(server.register_console(properties, port));
        }
    }
    None
}

pub async fn monitor_serial(server: Server) {
    let mut serial_devices = HashMap::new();
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
        if let Some(devnode) = d.devnode() {
            if let Some(id) = add_serial(&server, &d).await {
                serial_devices.insert(devnode.to_path_buf(), id);
            }
        }
    }

    while let Some(Ok(event)) = monitor.next().await {
        match event.event_type() {
            tokio_udev::EventType::Add => {
                let device = event.device();
                if let Some(devnode) = device.devnode() {
                    if let Some(id) = add_serial(&server, &device).await {
                        serial_devices.insert(devnode.to_path_buf(), id);
                    }
                }
            }
            tokio_udev::EventType::Remove => {
                if let Some(path) = event.device().devnode() {
                    if let Some(id) = serial_devices.remove(path) {
                        server.unregister_console(id);
                    }
                }
            }
            _ => info!(
                "Udev event: {:?} {:?}",
                event.event_type(),
                event.device().devnode()
            ),
        }
    }
}

pub async fn add_dfu(server: &Server, device: &tokio_udev::Device) -> Option<u64> {
    let interface = device.property_value("ID_USB_INTERFACES")?;
    if interface != ":fe0102:" {
        return None;
    }

    if let Some(node) = device.devnode() {
        let path = node.to_string_lossy().into_owned();
        let properties = properties_from_device(path.clone(), device);

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
        let dfu = crate::dfu::Dfu::new(busnum, devnum).await;
        return Some(server.register_uploader(properties, dfu));
    }
    None
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
        if let Some(devnode) = d.devnode() {
            if let Some(id) = add_dfu(&server, &d).await {
                dfu_devices.insert(devnode.to_path_buf(), id);
            }
        }
    }

    while let Some(Ok(event)) = monitor.next().await {
        match event.event_type() {
            tokio_udev::EventType::Add => {
                let device = event.device();
                if let Some(devnode) = device.devnode() {
                    if let Some(id) = add_dfu(&server, &device).await {
                        dfu_devices.insert(devnode.to_path_buf(), id);
                    }
                }
            }
            tokio_udev::EventType::Remove => {
                if let Some(path) = event.device().devnode() {
                    if let Some(dfu) = dfu_devices.remove(path) {
                        server.unregister_uploader(dfu);
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
