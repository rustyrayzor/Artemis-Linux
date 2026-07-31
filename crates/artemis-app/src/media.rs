#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

use artemis_moonlight::VideoCodec;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecoderSupport {
    pub available: bool,
    pub hardware: bool,
    pub main10: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecoderCapabilities {
    pub h264: DecoderSupport,
    pub hevc: DecoderSupport,
    pub av1: DecoderSupport,
    pub presentation_bit_depth: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HdrDisplayCapabilities {
    pub output_name: Option<String>,
    pub display_hdr10: bool,
    pub native_hdr_presentation: bool,
    pub presentation_reason: String,
}

impl DecoderCapabilities {
    #[must_use]
    pub const fn support(self, codec: VideoCodec) -> DecoderSupport {
        match codec {
            VideoCodec::H264 => self.h264,
            VideoCodec::Hevc => self.hevc,
            VideoCodec::Av1 => self.av1,
        }
    }

    #[must_use]
    pub const fn main10_ready(self, codec: VideoCodec) -> bool {
        self.presentation_bit_depth >= 10 && self.support(codec).main10
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StreamDiagnostics {
    pub video_ingress_fps: f64,
    pub decoded_fps: f64,
    pub presented_fps: f64,
    pub decoder_queue_dropped: u64,
    pub callback_queue_dropped: u64,
    pub video_mbps: f64,
    pub audio_kbps: f64,
    pub audio_ingress_pps: f64,
    pub video_network_pps: f64,
    pub audio_network_pps: f64,
    pub video_packet_issues: u64,
    pub audio_packet_issues: u64,
    pub video_fec_recovered: u64,
    pub audio_fec_recovered: u64,
    pub video_clock_drift_ms: Option<i64>,
    pub audio_clock_drift_ms: Option<i64>,
    pub decoder: &'static str,
    pub memory_path: &'static str,
    pub video_bit_depth: &'static str,
    pub hdr_source_active: bool,
    pub video_color_space: &'static str,
    pub hdr_metadata_available: bool,
    pub hdr_max_content_light_level: Option<u16>,
    pub hdr_presentation: &'static str,
    pub audio_layout: &'static str,
    pub audio_output: &'static str,
    pub frame_pacing_active: bool,
}

#[cfg(target_os = "linux")]
pub use linux::{
    DecodedFrame, GlInteropContext, MediaRuntime, decoder_capabilities, hdr_display_capabilities,
};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{
    DecodedFrame, GlInteropContext, MediaRuntime, decoder_capabilities, hdr_display_capabilities,
};
