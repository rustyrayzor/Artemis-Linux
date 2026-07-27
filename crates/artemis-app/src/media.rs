#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

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
}

#[cfg(target_os = "linux")]
pub use linux::{DecodedFrame, GlInteropContext, MediaRuntime};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{DecodedFrame, GlInteropContext, MediaRuntime};
