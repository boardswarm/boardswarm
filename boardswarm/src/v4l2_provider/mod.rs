use std::collections::HashMap;

use tokio_stream::StreamExt;
use tracing::{trace, warn};

use crate::{Server, registry, v4l2_provider::device::V4l2Device};

pub const PROVIDER: &str = "v4l2";
mod device;

pub async fn start_provider(name: String, _parameters: serde_yaml::Value, server: Server) {
    let provider_properties = &[
        (registry::PROVIDER_NAME, name.as_str()),
        (registry::PROVIDER, PROVIDER),
    ];

    let mut registrations = HashMap::new();
    let mut devices = crate::udev::DeviceStream::new("video4linux").unwrap();

    while let Some(d) = devices.next().await {
        match d {
            crate::udev::DeviceEvent::Add { device, .. } => {
                let Some(node) = device.devnode() else {
                    continue;
                };

                let Some(name) = node.file_name() else {
                    continue;
                };

                let Some(capabilities) = device.property("ID_V4L_CAPABILITIES") else {
                    warn!(
                        "Ignoring v42l2 device ({}) without ID_V4L_CAPABILITIES",
                        device.syspath().display()
                    );
                    continue;
                };

                if !capabilities.contains(":capture:") {
                    trace!(
                        "Ignoring v42l2 device ({}) without ID_V4L_CAPABILITIES",
                        device.syspath().display()
                    );
                    continue;
                }

                let d = V4l2Device::new(node.to_string_lossy().into_owned());
                let mut properties = device.properties(name.to_string_lossy().into_owned());
                properties.extend(provider_properties);
                let id = server.register_media(properties, d);
                registrations.insert(device.syspath().to_path_buf(), id);
            }
            crate::udev::DeviceEvent::Remove(device) => {
                if let Some(id) = registrations.remove(device.syspath()) {
                    server.unregister_media(id)
                }
            }
        }
    }
}
