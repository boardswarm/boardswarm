use crate::{Media, MediaError, MediaSignalMsg, MediaSignallingRx};

#[derive(Debug)]
pub struct V4l2Device {}

impl V4l2Device {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait::async_trait]
impl Media for V4l2Device {
    async fn open(
        &self,
    ) -> Result<
        (
            Box<dyn MediaSignallingRx>,
            futures::stream::BoxStream<'static, MediaSignalMsg>,
        ),
        MediaError,
    > {
        todo!()
    }
}
