use boardswarm_protocol::{
    console_input_request, console_output, consoles_client::ConsolesClient, device_input_request,
    devices_client::DevicesClient, ConsoleConfigureRequest, ConsoleInputRequest,
    ConsoleOutputRequest, DeviceInputRequest, DeviceModeRequest, DeviceTarget,
};
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};

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
