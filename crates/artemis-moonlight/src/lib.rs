//! Safe Rust ownership boundary around the process-global moonlight-common-c API.

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError, unbounded};
use zeroize::{Zeroize, ZeroizeOnDrop};

mod platform;

pub use platform::Session;

pub const KEY_ACTION_DOWN: u8 = 0x03;
pub const KEY_ACTION_UP: u8 = 0x04;
pub const BUTTON_ACTION_PRESS: u8 = 0x07;
pub const BUTTON_ACTION_RELEASE: u8 = 0x08;

pub const MOUSE_LEFT: i32 = 0x01;
pub const MOUSE_MIDDLE: i32 = 0x02;
pub const MOUSE_RIGHT: i32 = 0x03;
pub const MOUSE_X1: i32 = 0x04;
pub const MOUSE_X2: i32 = 0x05;

pub const MODIFIER_SHIFT: u8 = 0x01;
pub const MODIFIER_CTRL: u8 = 0x02;
pub const MODIFIER_ALT: u8 = 0x04;
pub const MODIFIER_META: u8 = 0x08;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionQuality {
    Okay,
    Poor,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkStats {
    pub audio_packets: u32,
    pub audio_fec_recovered: u32,
    pub audio_fec_failed: u32,
    pub audio_out_of_sequence: u32,
    pub audio_invalid: u32,
    pub video_packets: u32,
    pub video_fec_recovered: u32,
    pub video_fec_failed: u32,
    pub video_out_of_sequence: u32,
    pub video_invalid: u32,
}

/// Native connection parameters copied into the C shim before connecting.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct StreamConfig {
    pub address: String,
    pub app_version: String,
    pub gfe_version: Option<String>,
    pub rtsp_session_url: Option<String>,
    pub server_codec_mode_support: i32,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate_kbps: i32,
    pub packet_size: i32,
    pub audio_configuration: i32,
    pub client_refresh_rate_x100: i32,
    pub remote_input_key: [u8; 16],
    pub remote_input_iv: [u8; 16],
}

/// Events copied out of short-lived native callback buffers.
#[derive(Debug)]
pub enum StreamEvent {
    StageStarting(String),
    StageComplete(String),
    StageFailed {
        name: String,
        error: i32,
    },
    Connected,
    Terminated(i32),
    ConnectionStatus(ConnectionQuality),
    VideoSetup {
        format: i32,
        width: i32,
        height: i32,
        fps: i32,
    },
    VideoFrame {
        bytes: Vec<u8>,
        key_frame: bool,
        presentation_time_us: u64,
    },
    AudioSetup {
        sample_rate: i32,
        channels: i32,
        streams: i32,
        coupled_streams: i32,
        samples_per_frame: i32,
        mapping: Vec<u8>,
    },
    /// One encoded Opus frame. An empty frame requests packet-loss concealment.
    AudioPacket(Vec<u8>),
}

/// Receives priority lifecycle events, lossless audio events, then droppable video events.
pub struct EventReceiver {
    pub(crate) control: Receiver<StreamEvent>,
    pub(crate) audio: Receiver<StreamEvent>,
    pub(crate) video: Receiver<StreamEvent>,
}

/// Receives lossless audio events on a dedicated playback thread.
pub struct AudioEventReceiver {
    receiver: Receiver<StreamEvent>,
}

/// Receives encoded video events on a dedicated decode thread.
pub struct VideoEventReceiver {
    receiver: Receiver<StreamEvent>,
}

impl EventReceiver {
    /// Detaches audio delivery from the lifecycle and video event pump.
    pub fn take_audio(&mut self) -> AudioEventReceiver {
        let (_sender, replacement) = unbounded();
        AudioEventReceiver {
            receiver: std::mem::replace(&mut self.audio, replacement),
        }
    }

    /// Detaches video delivery from the lifecycle event pump.
    pub fn take_video(&mut self) -> VideoEventReceiver {
        let (_sender, replacement) = unbounded();
        VideoEventReceiver {
            receiver: std::mem::replace(&mut self.video, replacement),
        }
    }

    /// Returns the next lifecycle or media event without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Empty` when no event is ready and `Disconnected` after all callback senders
    /// have been dropped.
    pub fn try_recv(&self) -> std::result::Result<StreamEvent, TryRecvError> {
        match self.control.try_recv() {
            Ok(event) => Ok(event),
            Err(TryRecvError::Disconnected | TryRecvError::Empty) => match self.audio.try_recv() {
                Ok(event) => Ok(event),
                Err(TryRecvError::Disconnected | TryRecvError::Empty) => self.video.try_recv(),
            },
        }
    }
}

impl AudioEventReceiver {
    /// Waits up to `timeout` for the next audio event.
    ///
    /// # Errors
    ///
    /// Returns `Timeout` when no event arrives before the deadline and `Disconnected` after the
    /// native callback sender has been dropped.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> std::result::Result<StreamEvent, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl VideoEventReceiver {
    /// Waits up to `timeout` for the next video event.
    ///
    /// # Errors
    ///
    /// Returns `Timeout` when no event arrives before the deadline and `Disconnected` after the
    /// native callback sender has been dropped.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> std::result::Result<StreamEvent, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossbeam_channel::{bounded, unbounded};

    use super::{ConnectionQuality, EventReceiver, StreamEvent};

    #[test]
    fn audio_is_received_before_droppable_video() {
        let (_control_sender, control) = unbounded();
        let (audio_sender, audio) = unbounded();
        let (video_sender, video) = bounded(1);
        let receiver = EventReceiver {
            control,
            audio,
            video,
        };

        video_sender
            .send(StreamEvent::VideoFrame {
                bytes: vec![1],
                key_frame: true,
                presentation_time_us: 0,
            })
            .expect("video receiver should remain connected");
        audio_sender
            .send(StreamEvent::AudioPacket(vec![2]))
            .expect("audio receiver should remain connected");

        assert!(matches!(
            receiver.try_recv(),
            Ok(StreamEvent::AudioPacket(packet)) if packet == [2]
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(StreamEvent::VideoFrame { bytes, .. }) if bytes == [1]
        ));
    }

    #[test]
    fn detached_audio_is_not_pumped_with_video() {
        let (_control_sender, control) = unbounded();
        let (audio_sender, audio) = unbounded();
        let (video_sender, video) = bounded(1);
        let mut receiver = EventReceiver {
            control,
            audio,
            video,
        };
        let audio_receiver = receiver.take_audio();

        audio_sender
            .send(StreamEvent::AudioPacket(vec![2]))
            .expect("audio receiver should remain connected");
        video_sender
            .send(StreamEvent::VideoFrame {
                bytes: vec![1],
                key_frame: true,
                presentation_time_us: 0,
            })
            .expect("video receiver should remain connected");

        assert!(matches!(
            receiver.try_recv(),
            Ok(StreamEvent::VideoFrame { bytes, .. }) if bytes == [1]
        ));
        assert!(matches!(
            audio_receiver.recv_timeout(Duration::from_millis(10)),
            Ok(StreamEvent::AudioPacket(packet)) if packet == [2]
        ));
    }

    #[test]
    fn detached_video_is_not_pumped_with_lifecycle_events() {
        let (_control_sender, control) = unbounded();
        let (_audio_sender, audio) = unbounded();
        let (video_sender, video) = bounded(1);
        let mut receiver = EventReceiver {
            control,
            audio,
            video,
        };
        let video_receiver = receiver.take_video();

        video_sender
            .send(StreamEvent::VideoFrame {
                bytes: vec![1],
                key_frame: true,
                presentation_time_us: 0,
            })
            .expect("video receiver should remain connected");

        assert!(receiver.try_recv().is_err());
        assert!(matches!(
            video_receiver.recv_timeout(Duration::from_millis(10)),
            Ok(StreamEvent::VideoFrame { bytes, .. }) if bytes == [1]
        ));
    }

    #[test]
    fn connection_quality_is_a_priority_control_event() {
        let (control_sender, control) = unbounded();
        let (_audio_sender, audio) = unbounded();
        let (video_sender, video) = bounded(1);
        let receiver = EventReceiver {
            control,
            audio,
            video,
        };

        video_sender
            .send(StreamEvent::VideoFrame {
                bytes: vec![1],
                key_frame: true,
                presentation_time_us: 0,
            })
            .expect("video receiver should remain connected");
        control_sender
            .send(StreamEvent::ConnectionStatus(ConnectionQuality::Poor))
            .expect("control receiver should remain connected");

        assert!(matches!(
            receiver.try_recv(),
            Ok(StreamEvent::ConnectionStatus(ConnectionQuality::Poor))
        ));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("native streaming is supported only on Linux")]
    UnsupportedPlatform,
    #[error("a stream is already active in this process")]
    AlreadyActive,
    #[error("native string contained an interior NUL byte")]
    InvalidString,
    #[error("failed to allocate the native streaming session")]
    Allocation,
    #[error("moonlight-common-c failed with code {0}")]
    Native(i32),
}

pub type Result<T> = std::result::Result<T, Error>;
