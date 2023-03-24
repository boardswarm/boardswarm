use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom, Write},
};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use rockusb::libusb::Transport;
use rusb::GlobalContext;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::{udev::DeviceEvent, Server, Volume, VolumeError, VolumeTarget, VolumeTargetInfo};

pub async fn start_provider(server: Server) {
    let mut registrations = HashMap::new();
    let mut devices = crate::udev::DeviceStream::new("usb").unwrap();
    while let Some(d) = devices.next().await {
        match d {
            DeviceEvent::Add(d) => {
                let Some(vendor) = d.property_u64("ID_VENDOR_ID", 16) else { continue };

                if vendor != 0x2207 || d.devnode().is_none() {
                    continue;
                }

                let Some(busnum): Option<u8> = d.property_u64("BUSNUM", 10)
                    .and_then( | v | v.try_into().ok()) else { continue } ;
                let Some(devnum): Option<u8> = d.property_u64("DEVNUM", 10)
                    .and_then(|v|v.try_into().ok()) else { continue } ;

                let name = if let Some(model) = d.property("ID_MODEL") {
                    format!("{}/{} {}", busnum, devnum, model)
                } else {
                    format!("{}/{}", devnum, devnum)
                };
                info!("New rockusb volume: {name}");

                match Rockusb::new(busnum, devnum).await {
                    Ok(rockusb) => {
                        let id = server.register_volume(d.properties(name), rockusb);
                        registrations.insert(d.syspath().to_path_buf(), id);
                    }
                    Err(e) => {
                        warn!("Rockusb device setup failure: {}", e);
                    }
                }
            }
            DeviceEvent::Remove(d) => {
                if let Some(id) = registrations.remove(d.syspath()) {
                    server.unregister_volume(id)
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum RockUsbError {
    #[error("Failed to send command to rockusb thread")]
    CommandsSend,
    #[error("Failed to getting reply from rockusb thread: {0}")]
    CommandsRecv(oneshot::error::RecvError),
    #[error("usb error: {0}")]
    Usb(#[from] rusb::Error),
    #[error("Rockusb usb error: {0}")]
    RockUsb(#[from] rockusb::libusb::Error),
    #[error("Rockusb device not available: {0}")]
    RockDeviceUnavailable(#[from] rockusb::libusb::DeviceUnavalable),
}

impl From<RockUsbError> for tonic::Status {
    fn from(e: RockUsbError) -> Self {
        match e {
            RockUsbError::CommandsSend | RockUsbError::CommandsRecv(_) => {
                tonic::Status::internal(e.to_string())
            }
            RockUsbError::Usb(_) | RockUsbError::RockUsb(_) => {
                tonic::Status::aborted(e.to_string())
            }
            RockUsbError::RockDeviceUnavailable(_) => tonic::Status::unavailable(e.to_string()),
        }
    }
}

impl From<RockUsbError> for VolumeError {
    fn from(e: RockUsbError) -> Self {
        match e {
            RockUsbError::CommandsSend => VolumeError::Internal(e.to_string()),
            RockUsbError::CommandsRecv(_) => VolumeError::Internal(e.to_string()),
            RockUsbError::RockDeviceUnavailable(_)
            | RockUsbError::RockUsb(_)
            | RockUsbError::Usb(_) => VolumeError::Failure(e.to_string()),
        }
    }
}

#[derive(Debug)]
enum RockUsbMode {
    MaskRom,
    Loader((String, u64)),
}

#[derive(Debug)]
pub struct Rockusb {
    commands: mpsc::Sender<RockUsbCommand>,
    targets: Vec<VolumeTargetInfo>,
    mode: RockUsbMode,
}

impl Rockusb {
    pub async fn new(bus: u8, dev: u8) -> Result<Self, RockUsbError> {
        let (commands, rx) = tokio::sync::mpsc::channel(1);
        std::thread::spawn(move || rockusb_thread(bus, dev, rx));

        let mode = Self::determine_mode(&commands).await?;

        let targets = match &mode {
            RockUsbMode::MaskRom => vec![
                VolumeTargetInfo {
                    name: "471".to_string(),
                    readable: false,
                    writable: true,
                    seekable: false,
                    size: None,
                    blocksize: None,
                },
                VolumeTargetInfo {
                    name: "472".to_string(),
                    readable: false,
                    writable: true,
                    seekable: false,
                    size: None,
                    blocksize: None,
                },
            ],
            RockUsbMode::Loader(l) => vec![VolumeTargetInfo {
                name: l.0.clone(),
                readable: true,
                writable: true,
                seekable: true,
                size: Some(l.1),
                blocksize: Some(512),
            }],
        };

        Ok(Self {
            commands,
            mode,
            targets,
        })
    }

    async fn determine_mode(
        commands: &mpsc::Sender<RockUsbCommand>,
    ) -> Result<RockUsbMode, RockUsbError> {
        let (mode_tx, mode_rx) = oneshot::channel();

        commands
            .send(RockUsbCommand::Mode(mode_tx))
            .await
            .map_err(|_e| RockUsbError::CommandsSend)?;
        mode_rx
            .await
            .unwrap_or_else(|e| Err(RockUsbError::CommandsRecv(e)))
    }
}

#[async_trait::async_trait]
impl Volume for Rockusb {
    fn targets(&self) -> &[VolumeTargetInfo] {
        &self.targets
    }

    async fn open(
        &self,
        target: &str,
        _length: Option<u64>,
    ) -> Result<Box<dyn VolumeTarget>, VolumeError> {
        match &self.mode {
            RockUsbMode::MaskRom => {
                let area = match target {
                    "471" => 0x471,
                    "472" => 0x472,
                    _ => Err(VolumeError::UnknownTargetRequested)?,
                };
                Ok(Box::new(RockUsbMaskromTarget::new(
                    area,
                    self.commands.clone(),
                )))
            }
            RockUsbMode::Loader((name, _size)) if name == target => {
                Ok(Box::new(RockUsbTarget::new(self.commands.clone()).await?))
            }
            _ => Err(VolumeError::UnknownTargetRequested),
        }
    }

    async fn commit(&self) -> Result<(), VolumeError> {
        Ok(())
    }
}

struct RockUsbMaskromTarget {
    area: u16,
    commands: mpsc::Sender<RockUsbCommand>,
    data: BytesMut,
}

impl RockUsbMaskromTarget {
    fn new(area: u16, commands: mpsc::Sender<RockUsbCommand>) -> Self {
        // Expect 1MB as the maximum size for the blobs loaded in maskrom mode
        let data = BytesMut::with_capacity(1024 * 1024);
        Self {
            area,
            commands,
            data,
        }
    }

    async fn flush(&mut self) -> Result<(), RockUsbError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(RockUsbCommand::WriteMaskromArea((
                tx,
                self.area,
                self.data.split().into(),
            )))
            .await
            .map_err(|_e| RockUsbError::CommandsSend)?;
        self.data = BytesMut::new();
        rx.await
            .unwrap_or_else(|e| Err(RockUsbError::CommandsRecv(e)))
    }
}

#[async_trait::async_trait]
impl VolumeTarget for RockUsbMaskromTarget {
    async fn write(&mut self, data: Bytes, offset: u64, completion: crate::WriteCompletion) {
        if offset as usize != self.data.len() {
            completion.complete(Err(tonic::Status::out_of_range("Invalid offset")));
        } else if data.len() + self.data.len() > self.data.capacity() {
            completion.complete(Err(tonic::Status::out_of_range("No more capacity")));
        } else {
            self.data.extend_from_slice(&data);
            completion.complete(Ok(data.len() as u64));
        }
    }

    async fn flush(&mut self, completion: crate::FlushCompletion) {
        let r = self.flush().await;
        completion.complete(r.map_err(Into::into));
    }
}

struct RockUsbTarget {
    operations: mpsc::Sender<RockUsbIO>,
}

impl RockUsbTarget {
    async fn new(commands: mpsc::Sender<RockUsbCommand>) -> Result<Self, VolumeError> {
        let (tx, rx) = oneshot::channel();
        commands
            .send(RockUsbCommand::Open(tx))
            .await
            .map_err(|_e| RockUsbError::CommandsSend)?;
        let operations = rx
            .await
            .unwrap_or_else(|e| Err(RockUsbError::CommandsRecv(e)))?;

        Ok(Self { operations })
    }

    async fn do_read(&mut self, length: u64, offset: u64) -> Result<Bytes, tonic::Status> {
        let (tx, rx) = oneshot::channel();
        self.operations
            .send(RockUsbIO::Read(length, offset, tx))
            .await
            .map_err(|_e| RockUsbError::CommandsSend)?;
        rx.await
            .map_err(RockUsbError::CommandsRecv)?
            .map_err(|e| tonic::Status::aborted(format!("{:?}", e)))
    }

    async fn do_write(&mut self, data: Bytes, offset: u64) -> Result<u64, tonic::Status> {
        let len = data.len() as u64;
        let (tx, rx) = oneshot::channel();
        self.operations
            .send(RockUsbIO::Write(data, offset, tx))
            .await
            .map_err(|_e| RockUsbError::CommandsSend)?;
        rx.await
            .map_err(RockUsbError::CommandsRecv)?
            .map_err(|e| tonic::Status::aborted(format!("{:?}", e)))?;
        Ok(len)
    }

    async fn do_flush(&mut self) -> Result<(), tonic::Status> {
        let (tx, rx) = oneshot::channel();
        self.operations
            .send(RockUsbIO::Flush(tx))
            .await
            .map_err(|_e| RockUsbError::CommandsSend)?;
        rx.await
            .map_err(RockUsbError::CommandsRecv)?
            .map_err(|e| tonic::Status::aborted(format!("{:?}", e)))
    }
}

#[async_trait::async_trait]
impl VolumeTarget for RockUsbTarget {
    async fn read(&mut self, length: u64, offset: u64, completion: crate::ReadCompletion) {
        completion.complete(self.do_read(length, offset).await);
    }

    async fn write(&mut self, data: Bytes, offset: u64, completion: crate::WriteCompletion) {
        completion.complete(self.do_write(data, offset).await);
    }

    async fn flush(&mut self, completion: crate::FlushCompletion) {
        completion.complete(self.do_flush().await);
    }
}

#[derive(Debug)]
enum RockUsbCommand {
    Mode(oneshot::Sender<Result<RockUsbMode, RockUsbError>>),
    WriteMaskromArea((oneshot::Sender<Result<(), RockUsbError>>, u16, Bytes)),
    Open(oneshot::Sender<Result<mpsc::Sender<RockUsbIO>, RockUsbError>>),
}

#[derive(Debug)]
enum RockUsbIO {
    Write(Bytes, u64, oneshot::Sender<Result<(), std::io::Error>>),
    Read(u64, u64, oneshot::Sender<Result<Bytes, std::io::Error>>),
    Flush(oneshot::Sender<Result<(), std::io::Error>>),
}

fn open_device(device: &rusb::Device<GlobalContext>) -> Result<Transport, RockUsbError> {
    let handle = device.open()?;
    Ok(Transport::from_usb_device(handle)?)
}

fn handle_io(
    device: &rusb::Device<GlobalContext>,
    tx: oneshot::Sender<Result<mpsc::Sender<RockUsbIO>, RockUsbError>>,
) {
    let transport = match open_device(device) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let mut io = match transport.into_io() {
        Ok(io) => io,
        Err(e) => {
            let _ = tx.send(Err(e.into()));
            return;
        }
    };

    let (operations_tx, mut operations) = mpsc::channel(1);
    let _ = tx.send(Ok(operations_tx));

    while let Some(op) = operations.blocking_recv() {
        match op {
            RockUsbIO::Write(data, offset, tx) => {
                let r = io
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| io.write_all(&data));
                let _ = tx.send(r);
            }
            RockUsbIO::Read(length, offset, tx) => {
                let mut data = BytesMut::zeroed(length as usize);
                let r = io
                    .seek(SeekFrom::Start(offset))
                    .and_then(|_| io.read_exact(&mut data))
                    .map(|()| data.into());
                let _ = tx.send(r);
            }
            RockUsbIO::Flush(tx) => {
                let r = io.flush();
                let _ = tx.send(r);
            }
        }
    }
}

fn determine_mode(device: &rusb::Device<GlobalContext>) -> Result<RockUsbMode, RockUsbError> {
    let mut transport = open_device(device)?;
    match transport.flash_info() {
        Ok(info) => {
            let name = transport
                .flash_id()
                .map(|f| f.to_str().trim_end().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            Ok(RockUsbMode::Loader((name, info.size())))
        }
        Err(_) => Ok(RockUsbMode::MaskRom),
    }
}

fn write_maskrom_area(
    device: &rusb::Device<GlobalContext>,
    area: u16,
    data: Bytes,
) -> Result<(), RockUsbError> {
    let mut transport = open_device(device)?;
    transport.write_maskrom_area(area, &data)?;
    Ok(())
}

fn rockusb_thread(bus: u8, dev: u8, mut commands: mpsc::Receiver<RockUsbCommand>) {
    let Some(device) = crate::utils::usb_device_from_bus_dev(bus, dev) else  {
        warn!("Failed to start thread for usb device {}/{}, not found", bus, dev);
        return
    };

    while let Some(command) = commands.blocking_recv() {
        match command {
            RockUsbCommand::Mode(tx) => {
                let _ = tx.send(determine_mode(&device));
            }
            RockUsbCommand::WriteMaskromArea((tx, area, data)) => {
                let _ = tx.send(write_maskrom_area(&device, area, data));
            }
            RockUsbCommand::Open(tx) => handle_io(&device, tx),
        }
    }
}
