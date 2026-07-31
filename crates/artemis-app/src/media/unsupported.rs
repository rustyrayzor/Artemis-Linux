use artemis_moonlight::{AudioEventReceiver, Session, VideoEventReceiver};

use super::{DecoderCapabilities, HdrDisplayCapabilities, StreamDiagnostics};

#[must_use]
pub fn decoder_capabilities() -> DecoderCapabilities {
    DecoderCapabilities::default()
}

pub fn hdr_display_capabilities() -> HdrDisplayCapabilities {
    HdrDisplayCapabilities {
        presentation_reason: "Native HDR presentation is supported only on Linux".to_owned(),
        ..HdrDisplayCapabilities::default()
    }
}

#[derive(Clone)]
pub struct GlInteropContext;

impl GlInteropContext {
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(_context: &eframe::CreationContext<'_>) -> Result<Option<Self>, String> {
        Ok(None)
    }

    #[must_use]
    pub const fn presentation_bit_depth(&self) -> u8 {
        8
    }
}

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub nv12: Vec<u8>,
    #[allow(dead_code)]
    pub presentation_time_us: u64,
}

pub struct MediaRuntime;

#[allow(clippy::unused_self)]
impl MediaRuntime {
    pub fn new(
        _audio_events: AudioEventReceiver,
        _video_events: VideoEventReceiver,
        _gl_interop: Option<GlInteropContext>,
        _frame_pacing: bool,
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

    pub fn set_audio_muted(&self, _muted: bool) {}

    pub fn poll_error(&self) -> Option<String> {
        None
    }

    pub fn shutdown(&mut self) {}
}
