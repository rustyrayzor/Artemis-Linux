use artemis_moonlight::StreamEvent;

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub struct MediaRuntime;

#[allow(clippy::unused_self)]
impl MediaRuntime {
    pub fn new() -> Result<Self, String> {
        Err("streaming media is supported only on Linux".to_owned())
    }

    pub fn handle(&mut self, _event: StreamEvent) -> Result<(), String> {
        Err("streaming media is supported only on Linux".to_owned())
    }

    pub fn try_frame(&self) -> Option<DecodedFrame> {
        None
    }

    pub fn poll_error(&self) -> Option<String> {
        None
    }

    pub fn shutdown(&mut self) {}
}
