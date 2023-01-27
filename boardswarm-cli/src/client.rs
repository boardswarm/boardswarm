use boardswarm_protocol::{
    actuators_client::ActuatorsClient, console_input_request, console_output,
    consoles_client::ConsolesClient, device_input_request, device_upload_request,
    devices_client::DevicesClient, upload_request, uploaders_client::UploadersClient,
    ActuatorModeRequest, ConsoleConfigureRequest, ConsoleInputRequest, ConsoleOutputRequest,
    DeviceInputRequest, DeviceModeRequest, DeviceTarget, DeviceUploadRequest, DeviceUploadTarget,
    DeviceUploaderRequest, UploadRequest, UploadTarget, UploaderRequest,
};
use bytes::Bytes;
use futures::{stream, Stream, StreamExt, TryStreamExt};

#[derive(Clone, Debug)]
pub struct Devices {
    client: DevicesClient<tonic::transport::Channel>,
}

impl Devices {
    pub async fn connect(url: String) -> Result<Self, tonic::transport::Error> {
        let client = DevicesClient::connect(url).await?;
        Ok(Self { client })
    }

    pub async fn list(&mut self) -> Result<Vec<String>, tonic::Status> {
        let devices = self.client.list(()).await?;
        Ok(devices.into_inner().devices)
    }

    pub async fn stream_input<I>(
        &mut self,
        device: String,
        console: Option<String>,
        input: I,
    ) -> Result<(), tonic::Status>
    where
        I: Stream<Item = Bytes> + Send + 'static,
    {
        self.client
            .stream_input(
                stream::once(async move {
                    DeviceInputRequest {
                        target_or_data: Some(device_input_request::TargetOrData::Target(
                            DeviceTarget { device, console },
                        )),
                    }
                })
                .chain(input.map(|i| DeviceInputRequest {
                    target_or_data: Some(device_input_request::TargetOrData::Data(i)),
                })),
            )
            .await?;
        Ok(())
    }

    pub async fn stream_output(
        &mut self,
        device: String,
        console: Option<String>,
    ) -> Result<impl Stream<Item = Bytes>, tonic::Status> {
        let request = tonic::Request::new(DeviceTarget {
            device: device.clone(),
            console: console.clone(),
        });
        let response = self.client.stream_output(request).await?;
        let stream = response.into_inner();
        Ok(stream.filter_map(|output| async {
            let output = output.ok()?;
            match output.data_or_state? {
                console_output::DataOrState::Data(data) => Some(data),
                _ => None,
            }
        }))
    }

    pub async fn change_mode(&mut self, device: String, mode: String) -> Result<(), tonic::Status> {
        self.client
            .change_mode(DeviceModeRequest { device, mode })
            .await?;
        Ok(())
    }

    pub async fn upload<S>(
        &mut self,
        device: String,
        uploader: String,
        target: String,
        data: S,
        length: u64,
    ) -> Result<(), tonic::Status>
    where
        S: Stream<Item = Bytes> + Send + 'static,
    {
        let progress = self
            .client
            .upload(
                stream::once(async move {
                    DeviceUploadRequest {
                        target_or_data: Some(device_upload_request::TargetOrData::Target(
                            DeviceUploadTarget {
                                device,
                                uploader,
                                target,
                                length,
                            },
                        )),
                    }
                })
                .chain(data.map(|d| DeviceUploadRequest {
                    target_or_data: Some(device_upload_request::TargetOrData::Data(d)),
                })),
            )
            .await?;
        let mut progress = progress.into_inner();
        while let Some(p) = progress.try_next().await? {
            dbg!(p);
        }
        Ok(())
    }

    pub async fn commit(&mut self, device: String, uploader: String) -> Result<(), tonic::Status> {
        let request = tonic::Request::new(DeviceUploaderRequest { device, uploader });
        self.client.upload_commit(request).await?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Consoles {
    client: ConsolesClient<tonic::transport::Channel>,
}

impl Consoles {
    pub async fn connect(url: String) -> Result<Self, tonic::transport::Error> {
        let client = ConsolesClient::connect(url).await?;
        Ok(Self { client })
    }

    pub async fn list(&mut self) -> Result<Vec<String>, tonic::Status> {
        let consoles = self.client.list(()).await?;
        Ok(consoles.into_inner().consoles)
    }

    pub async fn stream_input<I>(&mut self, console: String, input: I) -> Result<(), tonic::Status>
    where
        I: Stream<Item = Bytes> + Send + 'static,
    {
        self.client
            .stream_input(
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

    pub async fn stream_output(
        &mut self,
        console: String,
    ) -> Result<impl Stream<Item = Bytes>, tonic::Status> {
        let request = tonic::Request::new(ConsoleOutputRequest { console });
        let response = self.client.stream_output(request).await?;
        let stream = response.into_inner();
        Ok(stream.filter_map(|output| async {
            let output = output.ok()?;
            match output.data_or_state? {
                console_output::DataOrState::Data(data) => Some(data),
                _ => None,
            }
        }))
    }

    pub async fn configure(
        &mut self,
        console: String,
        parameters: boardswarm_protocol::Parameters,
    ) -> Result<(), tonic::Status> {
        let configure = ConsoleConfigureRequest {
            console,
            parameters: Some(parameters),
        };
        self.client.configure(configure).await?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Actuators {
    client: ActuatorsClient<tonic::transport::Channel>,
}

impl Actuators {
    pub async fn connect(url: String) -> Result<Self, tonic::transport::Error> {
        let client = ActuatorsClient::connect(url).await?;
        Ok(Self { client })
    }

    pub async fn list(&mut self) -> Result<Vec<String>, tonic::Status> {
        let actuators = self.client.list(()).await?;
        Ok(actuators.into_inner().actuators)
    }

    pub async fn change_mode(
        &mut self,
        actuator: String,
        parameters: boardswarm_protocol::Parameters,
    ) -> Result<(), tonic::Status> {
        let mode = ActuatorModeRequest {
            actuator,
            parameters: Some(parameters),
        };
        self.client.change_mode(mode).await?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Uploaders {
    client: UploadersClient<tonic::transport::Channel>,
}

impl Uploaders {
    pub async fn connect(url: String) -> Result<Self, tonic::transport::Error> {
        let client = UploadersClient::connect(url).await?;
        Ok(Self { client })
    }

    pub async fn list(&mut self) -> Result<Vec<String>, tonic::Status> {
        let uploaders = self.client.list(()).await?;
        Ok(uploaders.into_inner().uploaders)
    }

    pub async fn upload<S>(
        &mut self,
        uploader: String,
        target: String,
        data: S,
        length: u64,
    ) -> Result<(), tonic::Status>
    where
        S: Stream<Item = Bytes> + Send + 'static,
    {
        let progress = self
            .client
            .upload(
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
        let mut progress = progress.into_inner();
        while let Some(p) = progress.try_next().await? {
            dbg!(p);
        }
        Ok(())
    }

    pub async fn commit(&mut self, uploader: String) -> Result<(), tonic::Status> {
        let request = tonic::Request::new(UploaderRequest { uploader });
        self.client.commit(request).await?;
        Ok(())
    }
}
