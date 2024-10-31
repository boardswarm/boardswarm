use tracing::warn;

pub fn nusb_info_from_bus_dev(bus: u8, dev: u8) -> Option<nusb::DeviceInfo> {
    let mut devices = match nusb::list_devices() {
        Ok(d) => d,
        Err(e) => {
            warn!("Failed to get usb device list: {e}");
            return None;
        }
    };
    devices.find(|d| d.bus_number() == bus && d.device_address() == dev)
}
