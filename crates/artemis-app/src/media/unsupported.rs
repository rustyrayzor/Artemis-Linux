use eframe::egui;

use artemis_moonlight::{AudioEventReceiver, StreamEvent};

pub struct DecodedFrame {
    pub image: egui::ColorImage,
}

pub struct MediaRuntime;

#[allow(clippy::unused_self)]
impl MediaRuntime {
    pub fn new(_audio_events: AudioEventReceiver) -> Result<Self, String> {
        Err("streaming media is supported only on Linux".to_owned())
    }

    pub fn handle(&mut self, _event: StreamEvent) -> Result<(), String> {
        Err("streaming media is supported only on Linux".to_owned())
    }

    pub fn try_frame(&self) -> Option<DecodedFrame> {
        None
    }

    pub fn record_presented(&self) {}

    pub fn report_video_stats(&mut self) {}

    pub fn poll_error(&self) -> Option<String> {
        None
    }

    pub fn shutdown(&mut self) {}
}
