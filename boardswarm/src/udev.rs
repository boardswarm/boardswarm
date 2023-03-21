use std::{collections::VecDeque, ffi::OsStr, path::Path, pin::Pin, task::Poll};

use crate::registry::Properties;
use futures::{ready, Stream};
use tokio_udev::{AsyncMonitorSocket, Enumerator};
use tracing::warn;

pub struct DeviceStream {
    existing: VecDeque<Device>,
    monitor: AsyncMonitorSocket,
}

impl DeviceStream {
    pub fn new<O: AsRef<OsStr>>(subsystem: O) -> Result<Self, std::io::Error> {
        let monitor = tokio_udev::MonitorBuilder::new()?
            .match_subsystem(&subsystem)?
            .listen()?;
        let monitor = tokio_udev::AsyncMonitorSocket::new(monitor)?;

        let mut enumerator = Enumerator::new()?;
        enumerator.match_subsystem(&subsystem)?;
        let existing = enumerator.scan_devices()?.map(Device).collect();

        Ok(Self { existing, monitor })
    }
}

pub enum DeviceEvent {
    Add(Device),
    Remove(Device),
}

impl Stream for DeviceStream {
    type Item = DeviceEvent;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let me = self.get_mut();
        if let Some(device) = me.existing.pop_front() {
            Poll::Ready(Some(DeviceEvent::Add(device)))
        } else {
            loop {
                let Some(event) = ready!(Pin::new(&mut me.monitor).poll_next(cx)) else {
                return Poll::Ready(None)
            };
                let event = match event {
                    Ok(event) => event,
                    Err(e) => {
                        warn!("Udev event monitor error: {:?}", e);
                        continue;
                    }
                };
                match event.event_type() {
                    tokio_udev::EventType::Add => {
                        return Poll::Ready(Some(DeviceEvent::Add(Device(event.device()))))
                    }
                    tokio_udev::EventType::Remove => {
                        return Poll::Ready(Some(DeviceEvent::Remove(Device(event.device()))))
                    }
                    _ => continue,
                }
            }
        }
    }
}

const PROPERTY_BLACKLIST: &[&str] = &[
    "ACTION",
    "DRIVER",
    "MAJOR",
    "MODALIAS",
    "MINOR",
    "USEC_INITIALIZED",
];

pub struct Device(tokio_udev::Device);
impl Device {
    #[allow(dead_code)]
    pub fn udev_device(&self) -> &tokio_udev::Device {
        &self.0
    }

    pub fn syspath(&self) -> &Path {
        self.0.syspath()
    }

    pub fn devnode(&self) -> Option<&Path> {
        self.0.devnode()
    }

    pub fn properties<S: Into<String>>(&self, name: S) -> Properties {
        fn find_reasonable_parent(device: tokio_udev::Device) -> VecDeque<tokio_udev::Device> {
            let reasonable = matches!(
                (
                    device.subsystem().and_then(|s| s.to_str()),
                    device.devtype().and_then(|s| s.to_str()),
                ),
                (Some("pci"), _) | (Some("platform"), _) | (Some("usb"), Some("usb_device"))
            );
            let mut v = if reasonable {
                VecDeque::new()
            } else if let Some(p) = device.parent() {
                find_reasonable_parent(p)
            } else {
                VecDeque::new()
            };
            v.push_back(device);
            v
        }

        let mut properties = Properties::new(name);
        let chain = find_reasonable_parent(self.0.clone());
        for d in chain {
            for p in d.properties() {
                let name = p.name().to_string_lossy();
                if PROPERTY_BLACKLIST.contains(&&*name) {
                    continue;
                }
                let key = format!("udev.{}", name);
                let value = p.value().to_string_lossy();
                properties.insert(key, value);
            }
        }

        properties
    }

    pub fn property(&self, property: &str) -> Option<&str> {
        self.0.property_value(property)?.to_str()
    }

    pub fn property_u64(&self, property: &str, radix: u32) -> Option<u64> {
        self.0
            .property_value(property)?
            .to_str()
            .and_then(|u| u64::from_str_radix(u, radix).ok())
    }

    pub fn parent(&self) -> Option<Device> {
        self.0.parent().map(Device)
    }
}
