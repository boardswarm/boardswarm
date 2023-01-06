use boardswarm_protocol::{
    console_output, device_client::DeviceClient, device_input_request, DeviceInputRequest,
    DeviceTarget,
};
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};

#[derive(Clone, Debug)]
pub struct Device {
    client: DeviceClient<tonic::transport::Channel>,
}

impl Device {
    pub async fn connect(url: String) -> Result<Self, tonic::transport::Error> {
        let client = DeviceClient::connect(url).await?;
        Ok(Device { client })
    }

    pub async fn list_devices(&mut self) -> Result<Vec<String>, tonic::Status> {
        let devices = self.client.list_devices(()).await?;
        Ok(devices.into_inner().device)
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
}
