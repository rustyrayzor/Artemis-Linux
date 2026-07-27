use artemis_moonlight::{AudioEventReceiver, Session, VideoEventReceiver};

use super::StreamDiagnostics;

#[derive(Clone)]
pub struct GlInteropContext;

impl GlInteropContext {
    pub fn new(_context: &eframe::CreationContext<'_>) -> Result<Option<Self>, String> {
        Ok(None)
    }
}

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub nv12: Vec<u8>,
    pub presentation_time_us: u64,
}

pub struct MediaRuntime;

#[allow(clippy::unused_self)]
impl MediaRuntime {
    pub fn new(
        _audio_events: AudioEventReceiver,
        _video_events: VideoEventReceiver,
        _gl_interop: Option<GlInteropContext>,
    ) -> Result<Self, String> {
        Err("streaming media is supported only on Linux".to_owned())
    }

    pub fn try_frame(&self) -> Option<DecodedFrame> {
        None
    }

    pub fn record_presented(&mut self, _frame: &DecodedFrame) {}

    pub fn report_stream_stats(&mut self, _session: &Session) {}

    pub fn diagnostics(&self) -> StreamDiagnostics {
        StreamDiagnostics::default()
    }

    pub fn poll_error(&self) -> Option<String> {
        None
    }

    pub fn shutdown(&mut self) {}
}
