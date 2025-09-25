use std::path::PathBuf;

use bytes::{Bytes, BytesMut};
use mediatek_brom::{io::BromExecuteAsync, Brom};
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, instrument, warn};

use crate::{
    registry::{self, Properties},
    serial::SerialProvider,
    udev::{Device, DeviceRegistrations, PreRegistration},
    Server, Volume, VolumeError, VolumeTarget, VolumeTargetInfo,
};

pub const PROVIDER: &str = "mediatek-brom";
pub const TARGET: &str = "brom";

#[derive(Deserialize, Clone, Debug, Default)]
struct MediatekBromParameters {
    address: Option<u32>,
}

pub struct MediatekBromProvider {
    name: String,
    parameters: MediatekBromParameters,
    registrations: DeviceRegistrations,
}

impl MediatekBromProvider {
    pub fn new(name: String, parameters: serde_yaml::Value, server: Server) -> Self {
        let parameters: MediatekBromParameters = serde_yaml::from_value(parameters).unwrap();
        Self {
            name,
            parameters,
            registrations: DeviceRegistrations::new(server),
        }
    }
}

impl SerialProvider for MediatekBromProvider {
    fn handle(&mut self, device: &crate::udev::Device, seqnum: u64) -> bool {
        let provider_properties = &[
            (registry::PROVIDER_NAME, self.name.as_str()),
            (registry::PROVIDER, PROVIDER),
        ];
        if device.property_u64("ID_VENDOR_ID", 16) != Some(0x0e8d) {
            return false;
        };
        if device.property_u64("ID_MODEL_ID", 16) != Some(0x0003) {
            return false;
        };

        if let Some(node) = device.devnode() {
            if let Some(name) = node.file_name() {
                let prereg = self.registrations.pre_register(device, seqnum);

                let mut properties = device.properties(name.to_string_lossy());
                properties.extend(provider_properties);
                tokio::spawn(setup_volume(
                    prereg,
                    node.to_path_buf(),
                    self.parameters.address,
                    properties,
                ));

                return true;
            }
        }
        false
    }

    fn remove(&mut self, device: &Device) {
        self.registrations.remove(device);
    }
}

#[instrument(skip(r, properties))]
async fn setup_volume(
    r: PreRegistration,
    node: PathBuf,
    address: Option<u32>,
    mut properties: Properties,
) {
    info!("Setting up brom volume for {}", node.display());
    let mut port = match tokio_serial::new(node.to_string_lossy(), 115200).open_native_async() {
        Ok(port) => port,
        Err(e) => {
            warn!("Failed to open serial port: {e}");
            return;
        }
    };
    let brom = match port.execute(Brom::handshake()).await {
        Ok(brom) => brom,
        Err(e) => {
            warn!("Failed to perform brom handshake: {e}");
            return;
        }
    };

    let hwcode = match port.execute(brom.hwcode()).await {
        Ok(hwcode) => hwcode,
        Err(e) => {
            warn!("Failed to retrieve brom hardware code: {e}");
            return;
        }
    };

    info!("Hardware: {:#x}", hwcode.code);

    // If unset, DA address is determined from hardware code information
    let address = match address.or(hwcode.da_address()) {
        None => {
            warn!("Failed to determine DA address");
            return;
        }
        Some(address) => address,
    };

    properties.insert(
        format!("{PROVIDER}.hw_code"),
        format!("{:04x}", hwcode.code),
    );
    properties.insert(
        format!("{PROVIDER}.hw_version"),
        format!("{}", hwcode.version),
    );

    r.register_volume(properties, MediatekBromVolume::new(port, brom, address));
}

enum BromCommand {
    SendDa(
        bytes::Bytes,
        tokio::sync::oneshot::Sender<Result<(), mediatek_brom::io::IOError>>,
    ),
    Execute(tokio::sync::oneshot::Sender<Result<(), mediatek_brom::io::IOError>>),
}

async fn process(
    mut port: tokio_serial::SerialStream,
    brom: Brom,
    address: u32,
    mut exec: tokio::sync::mpsc::Receiver<BromCommand>,
) {
    while let Some(command) = exec.recv().await {
        match command {
            BromCommand::SendDa(bytes, sender) => {
                info!("Sending DA");
                let r = port.execute(brom.send_da(address, &bytes)).await;
                let _ = sender.send(r);
            }
            BromCommand::Execute(sender) => {
                info!("Executing DA");
                let r = port.execute(brom.jump_da64(address)).await;
                let _ = sender.send(r);
            }
        }
    }
}

#[derive(Debug)]
struct MediatekBromVolume {
    device: tokio::sync::mpsc::Sender<BromCommand>,
    targets: [VolumeTargetInfo; 1],
}

impl MediatekBromVolume {
    fn new(port: tokio_serial::SerialStream, brom: Brom, address: u32) -> Self {
        let (device, exec) = tokio::sync::mpsc::channel(16);
        tokio::spawn(process(port, brom, address, exec));
        Self {
            device,
            targets: [VolumeTargetInfo {
                name: String::from("brom"),
                readable: false,
                writable: true,
                seekable: false,
                size: None,
                blocksize: None,
            }],
        }
    }
}

#[async_trait::async_trait]
impl Volume for MediatekBromVolume {
    fn targets(&self) -> (&[VolumeTargetInfo], bool) {
        (&self.targets, true)
    }

    async fn open(
        &self,
        target: &str,
        length: Option<u64>,
    ) -> Result<(VolumeTargetInfo, Box<dyn VolumeTarget>), VolumeError> {
        if target == TARGET {
            Ok((
                self.targets[0].clone(),
                Box::new(BromVolumeTarget::new(self.device.clone(), length)),
            ))
        } else {
            Err(VolumeError::UnknownTargetRequested)
        }
    }

    async fn commit(&self) -> Result<(), VolumeError> {
        let (tx, rx) = oneshot::channel();
        let execute = BromCommand::Execute(tx);

        self.device
            .send(execute)
            .await
            .map_err(|e| VolumeError::Internal(e.to_string()))?;
        rx.await
            .map_err(|e| VolumeError::Internal(e.to_string()))?
            .map_err(|e| VolumeError::Failure(e.to_string()))?;

        Ok(())
    }
}

struct BromVolumeTarget {
    device: tokio::sync::mpsc::Sender<BromCommand>,
    data: BytesMut,
}

impl BromVolumeTarget {
    fn new(device: tokio::sync::mpsc::Sender<BromCommand>, size_hint: Option<u64>) -> Self {
        let data =
            BytesMut::with_capacity(size_hint.unwrap_or(1024 * 1024).min(8 * 1024 * 1024) as usize);
        Self { device, data }
    }
}

#[async_trait::async_trait]
impl VolumeTarget for BromVolumeTarget {
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

    async fn shutdown(&mut self, completion: crate::ShutdownCompletion) {
        let (tx, rx) = oneshot::channel();
        let send_da = BromCommand::SendDa(self.data.split().into(), tx);
        if let Err(e) = self.device.send(send_da).await {
            completion.complete(Err(tonic::Status::internal(e.to_string())));
            return;
        };
        match rx.await {
            Ok(Ok(())) => completion.complete(Ok(())),
            Ok(Err(e)) => {
                completion.complete(Err(tonic::Status::failed_precondition(e.to_string())))
            }
            Err(e) => completion.complete(Err(tonic::Status::internal(e.to_string()))),
        }
    }
}
