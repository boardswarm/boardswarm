use std::collections::HashMap;

use boardswarm_protocol::{
    boardswarm_client::BoardswarmClient, console_input_request, upload_request,
    ActuatorModeRequest, ConsoleConfigureRequest, ConsoleInputRequest, ConsoleOutputRequest,
    DeviceModeRequest, DeviceRequest, Item, ItemPropertiesRequest, ItemType, ItemTypeRequest,
    UploadRequest, UploadTarget, UploaderInfoMsg, UploaderRequest,
};
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use tokio::sync::watch::Receiver;

pub enum ItemEvent {
    Added(Vec<Item>),
    Removed(u64),
}

#[derive(Clone, Debug)]
pub struct Boardswarm {
    client: BoardswarmClient<tonic::transport::Channel>,
}

impl Boardswarm {
    pub async fn connect<D>(url: D) -> Result<Self, tonic::transport::Error>
    where
        D: TryInto<tonic::transport::Endpoint>,
        D::Error: Into<tonic::codegen::StdError>,
    {
        let client = BoardswarmClient::connect(url).await?;
        Ok(Self { client })
    }

    pub async fn list(&mut self, type_: ItemType) -> Result<Vec<Item>, tonic::Status> {
        let items = self
            .client
            .list(ItemTypeRequest {
                r#type: type_.into(),
            })
            .await?;

        Ok(items.into_inner().item)
    }

    pub async fn properties(
        &mut self,
        type_: ItemType,
        item: u64,
    ) -> Result<HashMap<String, String>, tonic::Status> {
        let properties = self
            .client
            .item_properties(ItemPropertiesRequest {
                r#type: type_.into(),
                item,
            })
            .await?
            .into_inner();
        Ok(properties
            .property
            .into_iter()
            .map(|v| (v.key, v.value))
            .collect())
    }

    pub async fn monitor(
        &mut self,
        type_: ItemType,
    ) -> Result<impl Stream<Item = Result<ItemEvent, tonic::Status>>, tonic::Status> {
        let items = self
            .client
            .monitor(ItemTypeRequest {
                r#type: type_.into(),
            })
            .await?
            .into_inner();

        Ok(items.filter_map(|event| async {
            event
                .map(|event| {
                    event.event.map(|event| match event {
                        boardswarm_protocol::item_event::Event::Add(added) => {
                            ItemEvent::Added(added.item)
                        }
                        boardswarm_protocol::item_event::Event::Remove(removed) => {
                            ItemEvent::Removed(removed)
                        }
                    })
                })
                .transpose()
        }))
    }

    pub async fn device_info(
        &mut self,
        device: u64,
    ) -> Result<impl Stream<Item = Result<boardswarm_protocol::Device, tonic::Status>>, tonic::Status>
    {
        let r = self.client.device_info(DeviceRequest { device }).await?;
        Ok(r.into_inner())
    }

    pub async fn device_change_mode(
        &mut self,
        device: u64,
        mode: String,
    ) -> Result<(), tonic::Status> {
        self.client
            .device_change_mode(DeviceModeRequest { device, mode })
            .await?;
        Ok(())
    }

    pub async fn console_stream_input<I>(
        &mut self,
        console: u64,
        input: I,
    ) -> Result<(), tonic::Status>
    where
        I: Stream<Item = Bytes> + Send + 'static,
    {
        self.client
            .console_stream_input(
                stream::once(async move {
                    ConsoleInputRequest {
                        target_or_data: Some(console_input_request::TargetOrData::Console(console)),
                    }
                })
                .chain(input.map(|i| ConsoleInputRequest {
                    target_or_data: Some(console_input_request::TargetOrData::Data(i)),
                })),
            )
            .await?;
        Ok(())
    }

    pub async fn console_stream_output(
        &mut self,
        console: u64,
    ) -> Result<impl Stream<Item = Bytes>, tonic::Status> {
        let request = tonic::Request::new(ConsoleOutputRequest { console });
        let response = self.client.console_stream_output(request).await?;
        let stream = response.into_inner();
        Ok(stream.filter_map(|output| async {
            let output = output.ok()?;
            Some(output.data)
        }))
    }

    pub async fn console_configure(
        &mut self,
        console: u64,
        parameters: boardswarm_protocol::Parameters,
    ) -> Result<(), tonic::Status> {
        let configure = ConsoleConfigureRequest {
            console,
            parameters: Some(parameters),
        };
        self.client.console_configure(configure).await?;
        Ok(())
    }

    pub async fn actuator_change_mode(
        &mut self,
        actuator: String,
        parameters: boardswarm_protocol::Parameters,
    ) -> Result<(), tonic::Status> {
        let mode = ActuatorModeRequest {
            actuator,
            parameters: Some(parameters),
        };
        self.client.actuator_change_mode(mode).await?;
        Ok(())
    }

    pub async fn uploader_info(&mut self, uploader: u64) -> Result<UploaderInfoMsg, tonic::Status> {
        let request = tonic::Request::new(UploaderRequest { uploader });
        let r = self.client.uploader_info(request).await?;
        Ok(r.into_inner())
    }

    pub async fn uploader_upload<S>(
        &mut self,
        uploader: u64,
        target: String,
        data: S,
        length: u64,
    ) -> Result<UploadProgress, tonic::Status>
    where
        S: Stream<Item = Bytes> + Send + 'static,
    {
        let progress = self
            .client
            .uploader_upload(
                stream::once(async move {
                    UploadRequest {
                        target_or_data: Some(upload_request::TargetOrData::Target(UploadTarget {
                            uploader,
                            target,
                            length,
                        })),
                    }
                })
                .chain(data.map(|d| UploadRequest {
                    target_or_data: Some(upload_request::TargetOrData::Data(d)),
                })),
            )
            .await?;
        Ok(UploadProgress::new(progress.into_inner()))
    }

    pub async fn uploader_commit(&mut self, uploader: u64) -> Result<(), tonic::Status> {
        let request = tonic::Request::new(UploaderRequest { uploader });
        self.client.uploader_commit(request).await?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum UploadProgressState {
    Writing(u64),
    Succeeded,
    Failed(tonic::Status),
}

pub struct UploadProgress {
    watch: Receiver<UploadProgressState>,
}

impl UploadProgress {
    fn new<P>(progress: P) -> Self
    where
        P: Stream<Item = Result<boardswarm_protocol::UploadProgress, tonic::Status>>
            + Send
            + 'static,
    {
        let (notifier, watch) = tokio::sync::watch::channel(UploadProgressState::Writing(0));
        tokio::spawn(async move {
            futures::pin_mut!(progress);
            while let Some(progress) = progress.next().await {
                match progress {
                    Ok(progress) => {
                        notifier.send_replace(UploadProgressState::Writing(progress.written));
                    }
                    Err(e) => {
                        notifier.send_replace(UploadProgressState::Failed(e));
                        return;
                    }
                }
            }
            notifier.send_replace(UploadProgressState::Succeeded);
        });

        UploadProgress { watch }
    }

    pub fn finished(&self) -> Option<Result<(), tonic::Status>> {
        match &*self.watch.borrow() {
            UploadProgressState::Succeeded => Some(Ok(())),
            UploadProgressState::Failed(status) => Some(Err(status.to_owned())),
            UploadProgressState::Writing(_) => None,
        }
    }

    // Wait for upload to finish
    pub async fn finish(&mut self) -> Result<(), tonic::Status> {
        loop {
            if let Some(f) = self.finished() {
                return f;
            } else {
                let _ = self.watch.changed().await;
            }
        }
    }

    pub fn stream(&self) -> impl Stream<Item = UploadProgressState> {
        tokio_stream::wrappers::WatchStream::new(self.watch.clone())
    }
}
