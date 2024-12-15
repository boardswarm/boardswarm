use std::collections::HashMap;

use android_sparse_image::{ChunkHeader, CHUNK_HEADER_BYTES_LEN};
use anyhow::Context;
use bytes::{Bytes, BytesMut};
use fastboot_protocol::nusb::NusbFastBoot;
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tracing::{debug, warn};
use tracing::{info, instrument};

use crate::{
    registry::{self, Properties},
    udev::{DeviceEvent, DeviceRegistrations, PreRegistration, UsbInterface},
    Server, Volume, VolumeError, VolumeTarget, VolumeTargetInfo,
};

pub const PROVIDER: &str = "fastboot";

#[derive(Deserialize, Debug, Default)]
struct FastbootParameters {
    #[serde(rename = "match")]
    match_: HashMap<String, String>,
    #[serde(default)]
    targets: Vec<String>,
}

#[instrument(skip(server, parameters))]
pub async fn start_provider(name: String, parameters: Option<serde_yaml::Value>, server: Server) {
    let registrations = DeviceRegistrations::new(server);
    let provider_properties = &[
        (registry::PROVIDER_NAME, name.as_str()),
        (registry::PROVIDER, PROVIDER),
    ];
    let mut devices = crate::udev::DeviceStream::new("usb").unwrap();
    let parameters: FastbootParameters = if let Some(parameters) = parameters {
        serde_yaml::from_value(parameters).unwrap()
    } else {
        Default::default()
    };
    while let Some(d) = devices.next().await {
        match d {
            DeviceEvent::Add { device, seqnum } => {
                let Some(interfaces) = device.usb_interfaces() else {
                    continue;
                };

                if !interfaces.iter().any(|i| {
                    i == &UsbInterface {
                        class: 0xff,
                        subclass: 0x42,
                        protocol: 0x3,
                    }
                }) {
                    continue;
                }

                let Some(busnum): Option<u8> = device
                    .property_u64("BUSNUM", 10)
                    .and_then(|v| v.try_into().ok())
                else {
                    continue;
                };
                let Some(devnum): Option<u8> = device
                    .property_u64("DEVNUM", 10)
                    .and_then(|v| v.try_into().ok())
                else {
                    continue;
                };

                let name = format!("fastboot {}/{}", busnum, devnum);
                let mut properties = device.properties(&name);
                if !properties.matches(&parameters.match_) {
                    debug!(
                        "Ignoring fastboot device {} - {:?}",
                        device.syspath().display(),
                        properties,
                    );

                    continue;
                }
                info!("New fastboot volume on: {name}");
                properties.extend(provider_properties);

                let prereg = registrations.pre_register(&device, seqnum);
                let known_targets = parameters.targets.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        setup_volume(prereg, busnum, devnum, known_targets, properties).await
                    {
                        warn!("Failed to setup fastboot volume: {e}");
                    }
                });
            }
            DeviceEvent::Remove(device) => registrations.remove(&device),
        }
    }
}

const FASTBOOT_BLOCKSIZE: usize = 4096;

#[instrument(skip(r, properties))]
async fn setup_volume(
    r: PreRegistration<FastbootVolume>,
    busnum: u8,
    devnum: u8,
    known_targets: Vec<String>,
    mut properties: Properties,
) -> anyhow::Result<()> {
    let info = crate::utils::nusb_info_from_bus_dev(busnum, devnum)
        .ok_or_else(|| anyhow::anyhow!("Failed to find fastboot device"))?;
    let mut fb = fastboot_protocol::nusb::NusbFastBoot::from_info(&info)
        .context("Failed to setup fastboot device")?;

    let vars = fb.get_all_vars().await.unwrap_or_else(|e| {
        info!("Failed to get all var: {e}, no discoverable targets");
        Default::default()
    });

    let mut targets: Vec<_> = vars
        .iter()
        .filter_map(|(k, v)| {
            if let Some(name) = k.strip_prefix("partition-size:") {
                let size = fastboot_protocol::protocol::parse_u64_hex(v).ok();
                debug!("Fastboot Partition: {name} - size: {size:?}");
                Some(VolumeTargetInfo {
                    name: name.to_string(),
                    readable: false,
                    writable: true,
                    seekable: true,
                    size,
                    blocksize: Some(FASTBOOT_BLOCKSIZE as u32),
                })
            } else {
                None
            }
        })
        .collect();

    for k in known_targets {
        if !targets.iter().any(|t| t.name == k) {
            targets.push(VolumeTargetInfo {
                name: k,
                readable: false,
                writable: true,
                seekable: true,
                size: None,
                blocksize: Some(FASTBOOT_BLOCKSIZE as u32),
            });
        }
    }

    let max_download = fastboot_protocol::protocol::parse_u32_hex(
        &fb.get_var("max-download-size")
            .await
            .context("Missing max download size")?,
    )
    .context("Failed to parse max-download-size")?;

    let version = fb.get_var("version").await.context("Missing version")?;

    debug!("Fastboot version: {version}");
    properties.insert(format!("{PROVIDER}.version"), version);
    let volume = FastbootVolume::new(fb, max_download, targets);
    r.register(properties, volume);
    Ok(())
}

#[derive(Debug)]
struct FastbootChunk {
    // offset in blocks
    offset: u32,
    data: Vec<Bytes>,
}

impl FastbootChunk {
    // Start offset in bytes
    fn offset_bytes(&self) -> usize {
        self.offset as usize * FASTBOOT_BLOCKSIZE
    }

    // Data amount in bytes
    fn data_bytes(&self) -> usize {
        self.data.iter().map(Bytes::len).sum()
    }

    // End in bytes
    fn end_bytes(&self) -> usize {
        self.offset_bytes() + self.data_bytes()
    }

    // Data in blocks padded up to whole blocksizes
    fn blocks(&self) -> u32 {
        self.data_bytes().div_ceil(FASTBOOT_BLOCKSIZE) as u32
    }

    // End in blocks padded up to whole blocksizes
    fn end_blocks(&self) -> u32 {
        self.offset + self.blocks()
    }

    // Padding required at the end in bytes
    fn end_padding(&self) -> usize {
        let rem = self.data_bytes() % FASTBOOT_BLOCKSIZE;
        if rem > 0 {
            FASTBOOT_BLOCKSIZE - rem
        } else {
            0
        }
    }
}

#[derive(Debug, Default)]
struct FastbootData {
    chunks: Vec<FastbootChunk>,
}

impl FastbootData {
    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    // Number of data chunks
    fn n_chunks(&self) -> usize {
        self.chunks.len()
    }

    // Expanded data in blocks
    fn expanded_blocks(&self) -> u32 {
        if let Some(last) = self.chunks.last() {
            last.end_blocks()
        } else {
            0
        }
    }

    // Calculate the size required for flashing this chunks
    fn flash_size(&self) -> usize {
        android_sparse_image::FILE_HEADER_BYTES_LEN
            + self.chunks
            .iter()
            .map(|chunk|
                if chunk.offset > 0 {
                    2 * CHUNK_HEADER_BYTES_LEN
                } else {
                    CHUNK_HEADER_BYTES_LEN
                }
                + chunk.data.iter().map(|d| d.len().next_multiple_of(FASTBOOT_BLOCKSIZE )).sum::<usize>()
             ).sum::<usize>()
    }

    // Calculate the size required chunks for flashing
    fn flash_chunks(&self) -> u32 {
        (match self.chunks.first() {
            Some(first) if first.offset == 0 => self.n_chunks() * 2 - 1,
            _ => self.n_chunks() * 2,
        }) as u32
    }

    // Try to append as much data as possible  at a given offset;
    fn append(&mut self, offset: usize, data: &mut Bytes, max_size: usize) -> bool {
        let remaining = max_size - self.flash_size();
        if let Some(last) = self.chunks.last_mut() {
            let end = last.end_bytes();
            if end == offset {
                let remaining = (remaining / FASTBOOT_BLOCKSIZE) * FASTBOOT_BLOCKSIZE;
                last.data.push(data.split_to(data.len().min(remaining)));
                return true;
            }

            // Will only append at the end
            if last.end_bytes() > offset {
                return false;
            }
        }

        // Starting a new chunk; assuming one don't care aka seek chunk is needed followed by the
        // raw chunk
        let remaining = remaining.saturating_sub(2 * CHUNK_HEADER_BYTES_LEN);
        // If we can't add a block..
        if remaining < FASTBOOT_BLOCKSIZE {
            return false;
        }

        let block_offset = offset / FASTBOOT_BLOCKSIZE;
        let remaining = (remaining / FASTBOOT_BLOCKSIZE) * FASTBOOT_BLOCKSIZE;
        let rem = offset % FASTBOOT_BLOCKSIZE;
        let data = if rem > 0 {
            let padding = BytesMut::zeroed(rem).into();
            vec![padding, data.split_to(data.len().min(remaining - rem))]
        } else {
            vec![data.split_to(data.len().min(remaining))]
        };

        self.chunks.push(FastbootChunk {
            offset: block_offset as u32,
            data,
        });
        true
    }
}

#[derive(Debug)]
enum FastbootCommand {
    Flash {
        target: String,
        data: FastbootData,
        result: oneshot::Sender<Result<(), fastboot_protocol::nusb::DownloadError>>,
    },
    Ping {
        sender: oneshot::Sender<()>,
    },
}

async fn process(
    mut fastboot: NusbFastBoot,
    mut exec: tokio::sync::mpsc::Receiver<FastbootCommand>,
) {
    while let Some(command) = exec.recv().await {
        match command {
            FastbootCommand::Flash {
                target,
                data,
                result,
            } => {
                async fn do_download(
                    fastboot: &mut NusbFastBoot,
                    target: &str,
                    data: FastbootData,
                ) -> Result<(), fastboot_protocol::nusb::DownloadError> {
                    let size = data.flash_size() as u32;
                    let mut download = fastboot.download(size).await?;
                    let header = android_sparse_image::FileHeader {
                        block_size: FASTBOOT_BLOCKSIZE as u32,
                        blocks: data.expanded_blocks(),
                        chunks: data.flash_chunks(),
                        checksum: 0,
                    };
                    download.extend_from_slice(&header.to_bytes()).await?;
                    // offset in chunks
                    let mut offset = 0;
                    for chunk in data.chunks {
                        if chunk.offset != offset {
                            let dontcare = ChunkHeader::new_dontcare(chunk.offset - offset);
                            download.extend_from_slice(&dontcare.to_bytes()).await?;
                        }
                        let header =
                            ChunkHeader::new_raw(chunk.blocks(), FASTBOOT_BLOCKSIZE as u32);
                        download.extend_from_slice(&header.to_bytes()).await?;
                        for d in &chunk.data {
                            download.extend_from_slice(d).await?;
                        }
                        match chunk.end_padding() {
                            0 => (),
                            v => {
                                let d = BytesMut::zeroed(v);
                                download.extend_from_slice(&d).await?;
                            }
                        }
                        offset = chunk.end_blocks();
                    }
                    download.finish().await?;
                    fastboot.flash(target).await?;
                    Ok(())
                }
                let r = if data.is_empty() {
                    Ok(())
                } else {
                    do_download(&mut fastboot, &target, data).await
                };
                let _ = result.send(r);
            }
            FastbootCommand::Ping { sender } => {
                let _ = sender.send(());
            }
        }
    }
}

struct FlashResult(
    tokio::sync::oneshot::Receiver<Result<(), fastboot_protocol::nusb::DownloadError>>,
);

impl FlashResult {
    async fn result(self) -> Result<(), tonic::Status> {
        self.0
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))?
            .map_err(|e| tonic::Status::failed_precondition(e.to_string()))
    }
}

#[derive(Debug, Clone)]
struct FastbootDevice(tokio::sync::mpsc::Sender<FastbootCommand>);
impl FastbootDevice {
    async fn flash<S: Into<String>>(
        &self,
        target: S,
        data: FastbootData,
    ) -> Result<FlashResult, tonic::Status> {
        let (tx, result) = oneshot::channel();
        let cmd = FastbootCommand::Flash {
            target: target.into(),
            data,
            result: tx,
        };
        self.0
            .send(cmd)
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok(FlashResult(result))
    }

    async fn ping(&self) -> Result<(), tonic::Status> {
        let (sender, rx) = oneshot::channel();
        let cmd = FastbootCommand::Ping { sender };
        self.0
            .send(cmd)
            .await
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
        rx.await.map_err(|e| tonic::Status::internal(e.to_string()))
    }
}

#[derive(Debug)]
struct FastbootVolume {
    targets: Vec<VolumeTargetInfo>,
    device: FastbootDevice,
    max_download: u32,
}

impl FastbootVolume {
    fn new(fastboot: NusbFastBoot, max_download: u32, targets: Vec<VolumeTargetInfo>) -> Self {
        let (device, exec) = tokio::sync::mpsc::channel(1);
        tokio::spawn(process(fastboot, exec));
        Self {
            targets,
            device: FastbootDevice(device),
            max_download,
        }
    }
}

#[async_trait::async_trait]
impl Volume for FastbootVolume {
    fn targets(&self) -> &[VolumeTargetInfo] {
        &self.targets
    }

    async fn open(
        &self,
        target: &str,
        _length: Option<u64>,
    ) -> Result<(VolumeTargetInfo, Box<dyn VolumeTarget>), VolumeError> {
        let Some(info) = self.targets.iter().find(|t| t.name == target) else {
            return Err(VolumeError::UnknownTargetRequested);
        };
        Ok((
            info.clone(),
            Box::new(FastbootVolumeTarget::new(
                self.device.clone(),
                target.to_string(),
                self.max_download,
            )),
        ))
    }

    async fn commit(&self) -> Result<(), VolumeError> {
        // TODO maybe make it reboot?
        Ok(())
    }
}

struct FastbootVolumeTarget {
    device: FastbootDevice,
    target: String,
    max_download: u32,
    buffered: FastbootData,
}

impl FastbootVolumeTarget {
    fn new(device: FastbootDevice, target: String, max_download: u32) -> Self {
        Self {
            device,
            target,
            max_download,
            buffered: FastbootData::default(),
        }
    }
}

// Flash out when buffering 16Mb
const FLASH_THRESHOLD: usize = 16 * 1024 * 1024;

#[async_trait::async_trait]
impl VolumeTarget for FastbootVolumeTarget {
    async fn write(&mut self, mut data: Bytes, offset: u64, completion: crate::WriteCompletion) {
        let len = data.len();
        let threshold = FLASH_THRESHOLD.min(self.max_download as usize - 2 * FASTBOOT_BLOCKSIZE);
        let mut flashes = vec![];

        while !data.is_empty() {
            let appended =
                self.buffered
                    .append(offset as usize, &mut data, self.max_download as usize);
            if !appended || self.buffered.flash_size() > threshold {
                let buffer = std::mem::take(&mut self.buffered);
                match self.device.flash(&self.target, buffer).await {
                    Ok(flash) => flashes.push(flash),
                    Err(e) => {
                        completion.complete(Err(e));
                        return;
                    }
                }
            }
        }

        // Asynchronously wait for flashing to complete, so the buffer can be re-filled while
        // flashing continues
        if flashes.is_empty() {
            completion.complete(Ok(len as u64));
        } else {
            tokio::spawn(async move {
                for f in flashes {
                    if let Err(e) = f.result().await {
                        completion.complete(Err(e));
                        return;
                    }
                }
                completion.complete(Ok(len as u64));
            });
        }
    }

    async fn flush(&mut self, completion: crate::FlushCompletion) {
        async fn do_flush(me: &mut FastbootVolumeTarget) -> Result<(), tonic::Status> {
            if !me.buffered.is_empty() {
                me.device
                    .flash(&me.target, std::mem::take(&mut me.buffered))
                    .await?
                    .result()
                    .await?;
            } else {
                me.device.ping().await?;
            };
            Ok(())
        }
        completion.complete(do_flush(self).await);
    }
}
