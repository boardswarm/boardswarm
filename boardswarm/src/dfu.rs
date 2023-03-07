use std::{collections::HashMap, io::Read};

use anyhow::bail;
use bytes::{Buf, Bytes};
use tokio::sync::{
    mpsc::{self, Receiver},
    oneshot,
};
use tracing::{info, warn};

use crate::{Volume, VolumeError, VolumeTarget, VolumeTargetInfo};

#[derive(Debug)]
pub struct Dfu {
    device: DfuDevice,
    targets: Vec<VolumeTargetInfo>,
}

impl Dfu {
    pub async fn new(busnum: u8, address: u8) -> Self {
        let device = DfuDevice::new(busnum, address);
        let targets = device
            .targets()
            .await
            .into_iter()
            .map(|name| VolumeTargetInfo {
                name,
                readable: false,
                writable: true,
                seekable: false,
                size: None,
                blocksize: None,
            })
            .collect();
        Self { device, targets }
    }
}

#[async_trait::async_trait]
impl Volume for Dfu {
    fn targets(&self) -> &[VolumeTargetInfo] {
        self.targets.as_slice()
    }

    async fn open(
        &self,
        target: &str,
        length: Option<u64>,
    ) -> Result<Box<dyn VolumeTarget>, VolumeError> {
        if self.targets.iter().any(|t| t.name == target) {
            let tx = self
                .device
                .start_download(target.to_owned(), length.unwrap_or_default() as u32)
                .await;
            Ok(Box::new(DfuTarget { tx }))
        } else {
            warn!("Unknown target requested");
            Err(VolumeError::UnknownTargetRequested)
        }
    }

    async fn commit(&self) -> Result<(), VolumeError> {
        self.device.reset().await;
        Ok(())
    }
}

struct DfuTarget {
    tx: mpsc::Sender<Bytes>,
}

#[async_trait::async_trait]
impl VolumeTarget for DfuTarget {
    async fn write(&mut self, data: Bytes, _offset: u64, completion: crate::WriteCompletion) {
        let len = data.len();
        let _ = self.tx.send(data).await;
        completion.complete(Ok(len as u64));
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
    Targets(oneshot::Sender<Vec<String>>),
    Download {
        target: String,
        length: u32,
        reader: DfuRead,
    },
    Reset(oneshot::Sender<()>),
}

#[derive(Clone, Debug)]
struct DfuDevice(mpsc::Sender<DfuCommand>);
impl DfuDevice {
    fn new(bus: u8, dev: u8) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        std::thread::spawn(move || dfu_thread(bus, dev, rx));
        Self(tx)
    }

    async fn targets(&self) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(DfuCommand::Targets(tx)).await;
        rx.await.unwrap_or_default()
    }

    async fn start_download(&self, target: String, length: u32) -> mpsc::Sender<Bytes> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let reader = DfuRead::new(rx);

        let command = DfuCommand::Download {
            target,
            length,
            reader,
        };

        self.0.send(command).await.unwrap();
        tx
    }

    async fn reset(&self) {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(DfuCommand::Reset(tx)).await;
        let _ = rx.await;
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
) -> anyhow::Result<()> {
    let handle = device.device.open()?;
    let mut dfu = dfu_libusb::DfuLibusb::from_usb_device(
        device.device.clone(),
        handle,
        interface.interface,
        interface.alt,
    )?;
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
            DfuCommand::Targets(tx) => {
                let _ = tx.send(device.interfaces.keys().cloned().collect());
            }
            DfuCommand::Download {
                target,
                length,
                reader,
            } => {
                if let Some(interface) = device.interfaces.get(&target) {
                    if let Err(e) = dfu_download(&device, interface, length, reader) {
                        warn!("Download failed: {}", e);
                    }
                } else {
                    warn!("Target not found: {}", target);
                }
            }
            DfuCommand::Reset(tx) => {
                // Shouldn't really matter what interface gets reset;  on the first interface
                if let Some(interface) = device.interfaces.values().next() {
                    if let Err(e) = dfu_reset(&device, interface) {
                        warn!("Reset failed: {}", e);
                    }
                } else {
                    warn!("No interfaces");
                }
                let _ = tx.send(());
            }
        }
    }
}
