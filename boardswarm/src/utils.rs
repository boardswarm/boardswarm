use tracing::warn;

#[macro_export]
/// Get the value of a bit at a specific index
macro_rules! get_bit {
    ($value:expr, $index:expr) => {
        ($value >> $index & 0x1)
    };
}

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

#[cfg(test)]
mod test {
    #[test]
    fn get_bit() {
        assert_eq!(get_bit!(0xa5, 0), 1);
        assert_eq!(get_bit!(0xa5, 1), 0);
        assert_eq!(get_bit!(0xa5, 2), 1);
        assert_eq!(get_bit!(0xa5, 3), 0);
        assert_eq!(get_bit!(0xa5, 4), 0);
        assert_eq!(get_bit!(0xa5, 5), 1);
        assert_eq!(get_bit!(0xa5, 6), 0);
        assert_eq!(get_bit!(0xa5, 7), 1);
    }
}
