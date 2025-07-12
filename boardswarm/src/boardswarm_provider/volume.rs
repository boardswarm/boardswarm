use boardswarm_client::client::{Boardswarm, VolumeIo};
use bytes::Bytes;

use crate::{Volume, VolumeError, VolumeTarget, VolumeTargetInfo};

#[derive(Debug)]
pub struct BoardswarmVolume {
    id: u64,
    remote: Boardswarm,
    targets: Vec<VolumeTargetInfo>,
    exhaustive: bool,
}

impl BoardswarmVolume {
    pub async fn new(id: u64, mut remote: Boardswarm) -> Result<Self, tonic::Status> {
        let info = remote.volume_info(id).await?;

        Ok(Self {
            id,
            remote,
            targets: info.target,
            exhaustive: info.exhaustive,
        })
    }
}

#[async_trait::async_trait]
impl Volume for BoardswarmVolume {
    fn targets(&self) -> (&[VolumeTargetInfo], bool) {
        (&self.targets, self.exhaustive)
    }

    async fn open(
        &self,
        target: &str,
        length: Option<u64>,
    ) -> Result<(VolumeTargetInfo, Box<dyn VolumeTarget>), VolumeError> {
        let (info, io) = self
            .remote
            .clone()
            .volume_io(self.id, target, length)
            .await
            .map_err(|e| VolumeError::Internal(e.to_string()))?;

        Ok((info.clone(), Box::new(BoardswarmVolumeTarget { io })))
    }

    async fn commit(&self) -> Result<(), VolumeError> {
        self.remote
            .clone()
            .volume_commit(self.id)
            .await
            .map_err(|e| match e.code() {
                tonic::Code::NotFound => VolumeError::UnknownTargetRequested,
                tonic::Code::Aborted => VolumeError::Failure(e.to_string()),
                tonic::Code::Internal => VolumeError::Internal(e.to_string()),
                _ => VolumeError::Internal(e.to_string()),
            })
    }

    async fn erase(&self, target: &str) -> Result<(), VolumeError> {
        self.remote
            .clone()
            .volume_erase(self.id, target)
            .await
            .map_err(|e| match e.code() {
                tonic::Code::NotFound => VolumeError::UnknownTargetRequested,
                tonic::Code::Aborted => VolumeError::Failure(e.to_string()),
                tonic::Code::Internal => VolumeError::Internal(e.to_string()),
                _ => VolumeError::Internal(e.to_string()),
            })?;
        Ok(())
    }
}

struct BoardswarmVolumeTarget {
    io: VolumeIo,
}

#[async_trait::async_trait]
impl VolumeTarget for BoardswarmVolumeTarget {
    async fn read(&mut self, length: u64, offset: u64, completion: crate::ReadCompletion) {
        match self.io.request_read(length, offset).await {
            Ok(request) => {
                tokio::spawn(async move { completion.complete(request.await) });
            }
            Err(e) => completion.complete(Err(tonic::Status::unavailable(format!(
                "Target no longer available: {e}"
            )))),
        }
    }

    async fn write(&mut self, data: Bytes, offset: u64, completion: crate::WriteCompletion) {
        match self.io.request_write(data, offset).await {
            Ok(request) => {
                tokio::spawn(async move { completion.complete(request.await) });
            }
            Err(e) => completion.complete(Err(tonic::Status::unavailable(format!(
                "Target no longer available: {e}"
            )))),
        }
    }

    async fn flush(&mut self, completion: crate::FlushCompletion) {
        match self.io.request_flush().await {
            Ok(request) => {
                tokio::spawn(async move { completion.complete(request.await) });
            }
            Err(e) => completion.complete(Err(tonic::Status::unavailable(format!(
                "Target no longer available: {e}"
            )))),
        }
    }

    async fn shutdown(&mut self, completion: crate::ShutdownCompletion) {
        match self.io.request_shutdown().await {
            Ok(request) => {
                tokio::spawn(async move { completion.complete(request.await) });
            }
            Err(e) => completion.complete(Err(tonic::Status::unavailable(format!(
                "Target no longer available: {e}"
            )))),
        }
    }
}
