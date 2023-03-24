use tracing::warn;

pub fn usb_device_from_bus_dev(bus: u8, dev: u8) -> Option<rusb::Device<rusb::GlobalContext>> {
    let devices = match rusb::DeviceList::new() {
        Ok(devices) => devices,
        Err(e) => {
            warn!("Failed to get usb device list: {e}");
            return None;
        }
    };
    devices
        .iter()
        .find(|d| d.bus_number() == bus && d.address() == dev)
}
