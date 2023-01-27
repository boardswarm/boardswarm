use std::{collections::HashMap, io::Read};

use anyhow::bail;
use bytes::{Buf, Bytes};
use futures::{stream::BoxStream, StreamExt};
use tokio::sync::mpsc::{self, Receiver};
use tracing::{info, warn};

use crate::{UploadProgress, Uploader, UploaderError};

#[derive(Debug)]
pub struct Dfu {
    devnode: String,
    device: DfuDevice,
    attributes: HashMap<String, String>,
}

impl Dfu {
    pub fn new(
        devnode: String,
        busnum: u8,
        address: u8,
        attributes: HashMap<String, String>,
    ) -> Self {
        let device = DfuDevice::new(busnum, address);
        Self {
            devnode,
            device,
            attributes,
        }
    }

    pub fn devnode(&self) -> &str {
        &self.devnode
    }
}

#[async_trait::async_trait]
impl Uploader for Dfu {
    fn name(&self) -> &str {
        self.devnode()
    }

    fn matches(&self, filter: serde_yaml::Value) -> bool {
        let filter: HashMap<String, String> = serde_yaml::from_value(filter).unwrap();
        for (k, v) in filter.iter() {
            match self.attributes.get(k) {
                Some(a_v) if a_v == v => (),
                _ => return false,
            }
        }
        true
    }

    async fn upload(
        &self,
        target: &str,
        data: BoxStream<'static, Bytes>,
        length: u64,
        progress: UploadProgress,
    ) -> Result<(), UploaderError> {
        info!("Uploading to: {}", target);
        let device = self.device.clone();
        let target = target.to_string();

        progress.update("Badgers".to_string());
        let mut updates = device.start_download(target, data, length as u32).await;

        while let Ok(_) = updates.changed().await {
            progress.update(format!("Written: {}", *updates.borrow_and_update()));
        }

        Ok(())
    }

    async fn commit(&self) -> Result<(), UploaderError> {
        self.device.reset().await;
        Ok(())
    }
}

#[derive(Debug)]
struct DfuRead {
    receiver: Receiver<Bytes>,
    bytes: Option<Bytes>,
}

impl DfuRead {
    fn new(receiver: Receiver<Bytes>) -> Self {
        DfuRead {
            receiver,
            bytes: None,
        }
    }
}

impl Read for DfuRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = if let Some(left) = &mut self.bytes {
            left
        } else {
            self.bytes = self.receiver.blocking_recv();
            match &mut self.bytes {
                Some(b) => b,
                None => return Ok(0),
            }
        };

        let bytes_left = left.len();
        let copied = if buf.len() >= bytes_left {
            left.copy_to_slice(&mut buf[0..bytes_left]);
            self.bytes = None;
            bytes_left
        } else {
            let mut copy = left.split_to(buf.len());
            copy.copy_to_slice(buf);
            buf.len()
        };

        Ok(copied)
    }
}

#[derive(Debug)]
enum DfuCommand {
    Interfaces,
    Download {
        target: String,
        length: u32,
        reader: DfuRead,
        progress: tokio::sync::watch::Sender<usize>,
    },
    Reset,
}

#[derive(Clone, Debug)]
struct DfuDevice(mpsc::Sender<DfuCommand>);
impl DfuDevice {
    fn new(bus: u8, dev: u8) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        std::thread::spawn(move || dfu_thread(bus, dev, rx));
        Self(tx)
    }

    async fn start_download(
        &self,
        target: String,
        mut data: BoxStream<'static, Bytes>,
        length: u32,
    ) -> tokio::sync::watch::Receiver<usize> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (progress, watch_rx) = tokio::sync::watch::channel(0);
        let reader = DfuRead::new(rx);

        let command = DfuCommand::Download {
            target,
            length,
            reader,
            progress,
        };

        self.0.send(command).await.unwrap();
        tokio::spawn(async move {
            while let Some(bytes) = data.next().await {
                let _ = tx.send(bytes).await;
            }
        });
        watch_rx
    }

    async fn reset(&self) {
        let _ = self.0.send(DfuCommand::Reset).await;
    }
}

fn find_device(bus: u8, dev: u8) -> anyhow::Result<rusb::Device<rusb::GlobalContext>> {
    let devices = rusb::DeviceList::new()?;
    for d in devices.iter() {
        if d.bus_number() == bus && d.address() == dev {
            return Ok(d);
        }
    }
    bail!("Dfu device not found");
}

struct DfuData<C: rusb::UsbContext> {
    device: rusb::Device<C>,
    interfaces: HashMap<String, DfuInterface>,
}

#[derive(Debug, Clone)]
struct DfuInterface {
    interface: u8,
    alt: u8,
}

fn setup_device(bus: u8, dev: u8) -> anyhow::Result<DfuData<rusb::GlobalContext>> {
    let device = find_device(bus, dev)?;
    let handle = device.open()?;
    let mut interfaces = HashMap::new();

    let desc = device.device_descriptor()?;
    for c in 0..desc.num_configurations() {
        let config = device.config_descriptor(c)?;
        for i in config.interfaces() {
            for i_desc in i.descriptors() {
                if i_desc.class_code() == 0xfe && i_desc.sub_class_code() == 0x1 {
                    info!("{:?}", device);
                    info!(" interface: {}", i_desc.setting_number());
                    match i_desc.protocol_code() {
                        0x1 => {
                            info!("     Application mode");
                            continue;
                        }
                        0x2 => info!("     DFU mode"),
                        m => {
                            info!("     Unknown mode: {}", m);
                            continue;
                        }
                    }
                    if let Some(s_i) = i_desc.description_string_index() {
                        let descriptor = handle.read_string_descriptor_ascii(s_i)?;
                        info!("  descriptor: {}", descriptor);
                        interfaces.insert(
                            descriptor,
                            DfuInterface {
                                interface: i.number(),
                                alt: i_desc.setting_number(),
                            },
                        );
                    }
                }
            }
        }
    }

    Ok(DfuData { device, interfaces })
}

fn dfu_download<C: rusb::UsbContext>(
    device: &DfuData<C>,
    interface: &DfuInterface,
    length: u32,
    reader: DfuRead,
    progress: tokio::sync::watch::Sender<usize>,
) -> anyhow::Result<()> {
    let handle = device.device.open()?;
    let mut dfu = dfu_libusb::DfuLibusb::from_usb_device(
        device.device.clone(),
        handle,
        interface.interface,
        interface.alt,
    )?;
    dfu.with_progress({
        let mut total = 0;
        move |count| {
            total += count;
            let _ = progress.send(total);
        }
    });
    dfu.download(reader, length)?;
    Ok(())
}

fn dfu_reset<C: rusb::UsbContext>(
    device: &DfuData<C>,
    interface: &DfuInterface,
) -> anyhow::Result<()> {
    let handle = device.device.open()?;
    let dfu = dfu_libusb::DfuLibusb::from_usb_device(
        device.device.clone(),
        handle,
        interface.interface,
        interface.alt,
    )?;
    let _ = dfu.detach();
    dfu.usb_reset()?;
    Ok(())
}

fn dfu_thread(bus: u8, dev: u8, mut control: mpsc::Receiver<DfuCommand>) {
    let device = match setup_device(bus, dev) {
        Ok(d) => d,
        Err(e) => {
            warn!("Failed to setup device: {}", e);
            return;
        }
    };

    while let Some(command) = control.blocking_recv() {
        match command {
            DfuCommand::Interfaces => todo!(),
            DfuCommand::Download {
                target,
                length,
                reader,
                progress,
            } => {
                if let Some(interface) = device.interfaces.get(&target) {
                    if let Err(e) = dfu_download(&device, interface, length, reader, progress) {
                        warn!("Download failed: {}", e);
                    }
                } else {
                    warn!("Target not found: {}", target);
                }
            }
            DfuCommand::Reset => {
                // Shouldn't really matter what interface gets reset;  on the first interface
                if let Some(interface) = device.interfaces.values().next() {
                    if let Err(e) = dfu_reset(&device, interface) {
                        warn!("Reset failed: {}", e);
                    }
                } else {
                    warn!("No interfaces");
                }
            }
        }
    }
}
