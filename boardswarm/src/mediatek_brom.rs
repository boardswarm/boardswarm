use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use bytes::{Bytes, BytesMut};
use mediatek_brom::{io::BromExecuteAsync, Brom};
use tokio::sync::oneshot;
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, instrument, warn};

use crate::{
    registry::{self, Properties},
    serial::SerialProvider,
    Server, Volume, VolumeError, VolumeTarget, VolumeTargetInfo,
};

pub const PROVIDER: &str = "mediatek-brom";
pub const TARGET: &str = "brom";

#[derive(Debug)]
enum RegistrationState {
    /// Pending registration for a given udev sequence number
    Pending(u64),
    /// Registered with volume id
    Registered(u64),
}

#[derive(Clone)]
struct Registrations {
    server: Server,
    registrations: Arc<Mutex<HashMap<PathBuf, RegistrationState>>>,
}

impl Registrations {
    fn pre_register(&self, syspath: &Path, seqnum: u64) {
        let mut registrations = self.registrations.lock().unwrap();
        if let Some(existing) =
            registrations.insert(syspath.to_path_buf(), RegistrationState::Pending(seqnum))
        {
            warn!("Pre-registering with known previous item: {:?}", existing);
            if let RegistrationState::Registered(id) = existing {
                self.server.unregister_volume(id)
            }
        }
    }
    fn failed(&self, syspath: &Path, seqnum: u64) {
        let mut registrations = self.registrations.lock().unwrap();
        match registrations.get(syspath) {
            Some(RegistrationState::Pending(s)) if *s == seqnum => {
                registrations.remove(syspath);
            }
            _ => {
                info!("Ignoring outdated failure (seqnum: {seqnum})")
            }
        }
    }

    fn register(
        &self,
        syspath: &Path,
        seqnum: u64,
        volume: MediatekBromVolume,
        properties: Properties,
    ) {
        let mut registrations = self.registrations.lock().unwrap();
        match registrations.get(syspath) {
            Some(RegistrationState::Pending(s)) if *s == seqnum => {
                let id = self.server.register_volume(properties, volume);
                registrations.insert(syspath.to_path_buf(), RegistrationState::Registered(id));
            }
            _ => {
                info!("Ignoring outdated registration (seqnum: {seqnum})")
            }
        }
    }

    fn remove(&self, syspath: &Path) {
        let mut registrations = self.registrations.lock().unwrap();
        if let Some(RegistrationState::Registered(id)) = registrations.remove(syspath) {
            self.server.unregister_volume(id);
        }
    }
}

pub struct MediatekBromProvider {
    name: String,
    registrations: Registrations,
}

impl MediatekBromProvider {
    pub fn new(name: String, server: Server) -> Self {
        Self {
            name,
            registrations: Registrations {
                server,
                registrations: Default::default(),
            },
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
        if device.property_u64("ID_MODEL", 16) != Some(0x0003) {
            return false;
        };

        if let Some(node) = device.devnode() {
            if let Some(name) = node.file_name() {
                self.registrations.pre_register(device.syspath(), seqnum);

                let mut properties = device.properties(name.to_string_lossy());
                properties.extend(provider_properties);
                tokio::spawn(setup_volume(
                    self.registrations.clone(),
                    device.syspath().to_path_buf(),
                    node.to_path_buf(),
                    properties,
                    seqnum,
                ));

                return true;
            }
        }
        false
    }

    fn remove(&mut self, path: &Path) {
        self.registrations.remove(path);
    }
}

#[instrument(skip(r, properties))]
async fn setup_volume(
    r: Registrations,
    syspath: PathBuf,
    node: PathBuf,
    mut properties: Properties,
    seqnum: u64,
) {
    info!("Setting up brom volume for {}", node.display());
    let mut port = match tokio_serial::new(node.to_string_lossy(), 115200).open_native_async() {
        Ok(port) => port,
        Err(e) => {
            warn!("Failed to open serial port: {e}");
            r.failed(&syspath, seqnum);
            return;
        }
    };
    let brom = port.execute(Brom::handshake(0x201000)).await.unwrap();
    let hwcode = port.execute(brom.hwcode()).await.unwrap();

    info!("Hardware: {}", hwcode.code);
    properties.insert(
        format!("{PROVIDER}.hw_code"),
        format!("{:04x}", hwcode.code),
    );
    properties.insert(
        format!("{PROVIDER}.hw_version"),
        format!("{}", hwcode.version),
    );

    let volume = MediatekBromVolume::new(port, brom);

    r.register(&syspath, seqnum, volume, properties);
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
    mut exec: tokio::sync::mpsc::Receiver<BromCommand>,
) {
    while let Some(command) = exec.recv().await {
        match command {
            BromCommand::SendDa(bytes, sender) => {
                info!("Sending DA");
                let r = port.execute(brom.send_da(&bytes)).await;
                let _ = sender.send(r);
            }
            BromCommand::Execute(sender) => {
                info!("Executing DA");
                let r = port.execute(brom.jump_da64()).await;
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
    fn new(port: tokio_serial::SerialStream, brom: Brom) -> Self {
        let (device, exec) = tokio::sync::mpsc::channel(16);
        tokio::spawn(process(port, brom, exec));
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
    fn targets(&self) -> &[VolumeTargetInfo] {
        &self.targets
    }

    async fn open(
        &self,
        target: &str,
        length: Option<u64>,
    ) -> Result<Box<dyn VolumeTarget>, VolumeError> {
        if target == TARGET {
            Ok(Box::new(BromVolumeTarget::new(self.device.clone(), length)))
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
