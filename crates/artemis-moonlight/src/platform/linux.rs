//! Audited Linux FFI implementation.
//!
//! Unsafe code is confined to this module and each operation documents its safety invariant.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Sender, TrySendError, bounded, unbounded};

use crate::{
    ConnectionQuality, Error, EventReceiver, HdrMetadata, MediaIngressStats, NetworkStats, Result,
    StreamConfig, StreamEvent, VideoColorInfo, VideoColorSpace,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct NativeStartConfig {
    address: *const c_char,
    app_version: *const c_char,
    gfe_version: *const c_char,
    rtsp_session_url: *const c_char,
    server_codec_mode_support: i32,
    supported_video_formats: i32,
    width: i32,
    height: i32,
    fps: i32,
    bitrate_kbps: i32,
    packet_size: i32,
    audio_configuration: i32,
    client_refresh_rate_x100: i32,
    hdr_enabled: i32,
    remote_input_key: [u8; 16],
    remote_input_iv: [u8; 16],
}

type StageCallback = extern "C" fn(*mut c_void, *const c_char, i32, i32);
type ConnectedCallback = extern "C" fn(*mut c_void);
type TerminatedCallback = extern "C" fn(*mut c_void, i32);
type ConnectionStatusCallback = extern "C" fn(*mut c_void, i32);
type VideoSetupCallback = extern "C" fn(*mut c_void, i32, i32, i32, i32) -> i32;
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeHdrMetadata {
    display_primaries_x: [u16; 3],
    display_primaries_y: [u16; 3],
    white_point_x: u16,
    white_point_y: u16,
    max_display_luminance: u16,
    min_display_luminance: u16,
    max_content_light_level: u16,
    max_frame_average_light_level: u16,
    max_full_frame_luminance: u16,
}

type HdrModeCallback = extern "C" fn(*mut c_void, i32, *const NativeHdrMetadata);
type VideoFrameCallback =
    extern "C" fn(*mut c_void, *const u8, usize, i32, u64, i32, i32, *const NativeHdrMetadata);
type AudioSetupCallback =
    extern "C" fn(*mut c_void, i32, i32, i32, i32, i32, *const u8, usize) -> i32;
type AudioPacketCallback = extern "C" fn(*mut c_void, *const u8, usize);

#[repr(C)]
struct NativeCallbacks {
    userdata: *mut c_void,
    stage: Option<StageCallback>,
    connected: Option<ConnectedCallback>,
    terminated: Option<TerminatedCallback>,
    connection_status: Option<ConnectionStatusCallback>,
    hdr_mode: Option<HdrModeCallback>,
    video_setup: Option<VideoSetupCallback>,
    video_frame: Option<VideoFrameCallback>,
    audio_setup: Option<AudioSetupCallback>,
    audio_packet: Option<AudioPacketCallback>,
}

#[repr(C)]
struct NativeSession {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Default)]
struct NativeNetworkStats {
    audio_packets: u32,
    audio_fec_recovered: u32,
    audio_fec_failed: u32,
    audio_out_of_sequence: u32,
    audio_invalid: u32,
    video_packets: u32,
    video_fec_recovered: u32,
    video_fec_failed: u32,
    video_out_of_sequence: u32,
    video_invalid: u32,
}

unsafe extern "C" {
    fn aml_session_create(
        config: *const NativeStartConfig,
        callbacks: *const NativeCallbacks,
    ) -> *mut NativeSession;
    fn aml_session_start(session: *mut NativeSession) -> i32;
    fn aml_session_interrupt(session: *mut NativeSession);
    fn aml_session_stop(session: *mut NativeSession);
    fn aml_session_destroy(session: *mut NativeSession);
    fn aml_session_network_stats(
        session: *mut NativeSession,
        stats: *mut NativeNetworkStats,
    ) -> i32;
    fn aml_mouse_move(session: *mut NativeSession, x: i16, y: i16) -> i32;
    fn aml_mouse_move_as_position(
        session: *mut NativeSession,
        x: i16,
        y: i16,
        reference_width: i16,
        reference_height: i16,
    ) -> i32;
    fn aml_mouse_button(session: *mut NativeSession, action: u8, button: i32) -> i32;
    fn aml_scroll(session: *mut NativeSession, vertical: i16, horizontal: i16) -> i32;
    fn aml_keyboard(
        session: *mut NativeSession,
        virtual_key: i16,
        action: u8,
        modifiers: u8,
    ) -> i32;
    fn aml_controller_arrival(session: *mut NativeSession) -> i32;
    fn aml_controller_state(
        session: *mut NativeSession,
        buttons: i32,
        left_trigger: u8,
        right_trigger: u8,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
    ) -> i32;
    fn aml_controller_departure(session: *mut NativeSession) -> i32;
    fn aml_request_idr(session: *mut NativeSession);
}

struct CallbackContext {
    control: Sender<StreamEvent>,
    audio: Sender<StreamEvent>,
    video: Sender<StreamEvent>,
    audio_packets: AtomicU64,
    audio_bytes: AtomicU64,
    video_frames: AtomicU64,
    video_bytes: AtomicU64,
    video_queue_dropped: AtomicU64,
}

impl CallbackContext {
    fn media_ingress_stats(&self) -> MediaIngressStats {
        MediaIngressStats {
            audio_packets: self.audio_packets.load(Ordering::Relaxed),
            audio_bytes: self.audio_bytes.load(Ordering::Relaxed),
            video_frames: self.video_frames.load(Ordering::Relaxed),
            video_bytes: self.video_bytes.load(Ordering::Relaxed),
            video_queue_dropped: self.video_queue_dropped.load(Ordering::Relaxed),
        }
    }

    fn send_video(&self, event: StreamEvent) {
        self.video_frames.fetch_add(1, Ordering::Relaxed);
        if let StreamEvent::VideoFrame { bytes, .. } = &event {
            self.video_bytes.fetch_add(
                u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        if matches!(self.video.try_send(event), Err(TrySendError::Full(_))) {
            self.video_queue_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn send_audio(&self, event: StreamEvent) {
        self.audio_packets.fetch_add(1, Ordering::Relaxed);
        if let StreamEvent::AudioPacket(packet) = &event {
            self.audio_bytes.fetch_add(
                u64::try_from(packet.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        let _ = self.audio.send(event);
    }
}

/// Sole owner of a process-global moonlight-common-c connection.
pub struct Session {
    native: NonNull<NativeSession>,
    callback_context: NonNull<CallbackContext>,
    stopped: bool,
}

// SAFETY: `Session` has exclusive ownership of the native handle and its public mutation
// methods require `&mut self`. The native library owns its callback threads, which access only
// the independently allocated, thread-safe channel senders in `CallbackContext`.
unsafe impl Send for Session {}

#[allow(clippy::missing_errors_doc)]
impl Session {
    // Ownership ensures the per-session input key is zeroized immediately after the native
    // shim copies it, whether connection setup succeeds or fails.
    #[allow(clippy::needless_pass_by_value)]
    pub fn connect(config: StreamConfig) -> Result<(Self, EventReceiver)> {
        if ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::AlreadyActive);
        }
        match Self::connect_inner(&config) {
            Ok(value) => Ok(value),
            Err(error) => {
                ACTIVE.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    fn connect_inner(config: &StreamConfig) -> Result<(Self, EventReceiver)> {
        let address = CString::new(config.address.as_str()).map_err(|_| Error::InvalidString)?;
        let app_version =
            CString::new(config.app_version.as_str()).map_err(|_| Error::InvalidString)?;
        let gfe_version = config
            .gfe_version
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|_| Error::InvalidString)?;
        let rtsp_session_url = config
            .rtsp_session_url
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|_| Error::InvalidString)?;
        let native_config = NativeStartConfig {
            address: address.as_ptr(),
            app_version: app_version.as_ptr(),
            gfe_version: gfe_version
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            rtsp_session_url: rtsp_session_url
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            server_codec_mode_support: config.server_codec_mode_support,
            supported_video_formats: config.supported_video_formats,
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            packet_size: config.packet_size,
            audio_configuration: config.audio_configuration,
            client_refresh_rate_x100: config.client_refresh_rate_x100,
            hdr_enabled: i32::from(config.hdr_enabled),
            remote_input_key: config.remote_input_key,
            remote_input_iv: config.remote_input_iv,
        };

        let (control_sender, control_receiver) = unbounded();
        let (audio_sender, audio_receiver) = unbounded();
        let (video_sender, video_receiver) = bounded(128);
        let callback_context = Box::new(CallbackContext {
            control: control_sender,
            audio: audio_sender,
            video: video_sender,
            audio_packets: AtomicU64::new(0),
            audio_bytes: AtomicU64::new(0),
            video_frames: AtomicU64::new(0),
            video_bytes: AtomicU64::new(0),
            video_queue_dropped: AtomicU64::new(0),
        });
        let callback_context =
            NonNull::new(Box::into_raw(callback_context)).ok_or(Error::Allocation)?;
        let callbacks = NativeCallbacks {
            userdata: callback_context.as_ptr().cast(),
            stage: Some(on_stage),
            connected: Some(on_connected),
            terminated: Some(on_terminated),
            connection_status: Some(on_connection_status),
            hdr_mode: Some(on_hdr_mode),
            video_setup: Some(on_video_setup),
            video_frame: Some(on_video_frame),
            audio_setup: Some(on_audio_setup),
            audio_packet: Some(on_audio_packet),
        };

        // SAFETY: Both repr(C) argument structs are fully initialized and all string pointers
        // remain alive for this call. The C shim copies every string and callback value it needs.
        let native = unsafe { aml_session_create(&native_config, &callbacks) };
        let Some(native) = NonNull::new(native) else {
            // SAFETY: The pointer came from Box::into_raw above and native creation failed, so
            // no C code retained the userdata pointer.
            unsafe { drop(Box::from_raw(callback_context.as_ptr())) };
            return Err(Error::Allocation);
        };

        // SAFETY: `native` is a live session returned by `aml_session_create`. This is the only
        // active start operation, guarded by `ACTIVE`.
        let result = unsafe { aml_session_start(native.as_ptr()) };
        if result != 0 {
            // SAFETY: Start returned synchronously and the shim cleared its global reference.
            // Destroy joins/cleans any partial native state before userdata is reclaimed.
            unsafe { aml_session_destroy(native.as_ptr()) };
            // SAFETY: Native teardown has completed, so no callback can access userdata.
            unsafe { drop(Box::from_raw(callback_context.as_ptr())) };
            return Err(Error::Native(result));
        }

        Ok((
            Self {
                native,
                callback_context,
                stopped: false,
            },
            EventReceiver {
                control: control_receiver,
                audio: audio_receiver,
                video: video_receiver,
            },
        ))
    }

    pub fn interrupt(&mut self) {
        if !self.stopped {
            // SAFETY: The native handle remains valid until Drop and interrupt is idempotent.
            unsafe { aml_session_interrupt(self.native.as_ptr()) };
        }
    }

    pub fn stop(&mut self) {
        if !self.stopped {
            // SAFETY: This session exclusively owns the valid native handle.
            unsafe { aml_session_stop(self.native.as_ptr()) };
            self.stopped = true;
            ACTIVE.store(false, Ordering::Release);
        }
    }

    pub fn mouse_move(&mut self, x: i16, y: i16) -> Result<()> {
        // SAFETY: The native handle is valid and the C shim validates active state.
        check(unsafe { aml_mouse_move(self.native.as_ptr(), x, y) })
    }

    pub fn mouse_move_as_position(
        &mut self,
        x: i16,
        y: i16,
        reference_width: i16,
        reference_height: i16,
    ) -> Result<()> {
        // SAFETY: The native handle is valid and all dimensions are copied protocol integers.
        check(unsafe {
            aml_mouse_move_as_position(
                self.native.as_ptr(),
                x,
                y,
                reference_width,
                reference_height,
            )
        })
    }

    pub fn mouse_button(&mut self, action: u8, button: i32) -> Result<()> {
        // SAFETY: The native handle is valid and values use public moonlight constants.
        check(unsafe { aml_mouse_button(self.native.as_ptr(), action, button) })
    }

    pub fn scroll(&mut self, vertical: i16, horizontal: i16) -> Result<()> {
        // SAFETY: The native handle is valid and the C shim validates active state.
        check(unsafe { aml_scroll(self.native.as_ptr(), vertical, horizontal) })
    }

    pub fn keyboard(&mut self, key: i16, action: u8, modifiers: u8) -> Result<()> {
        // SAFETY: The native handle is valid and the arguments are plain protocol integers.
        check(unsafe { aml_keyboard(self.native.as_ptr(), key, action, modifiers) })
    }

    pub fn controller_arrival(&mut self) -> Result<()> {
        // SAFETY: The native handle is valid and the C shim emits controller zero only.
        check(unsafe { aml_controller_arrival(self.native.as_ptr()) })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn controller_state(
        &mut self,
        buttons: i32,
        left_trigger: u8,
        right_trigger: u8,
        left_x: i16,
        left_y: i16,
        right_x: i16,
        right_y: i16,
    ) -> Result<()> {
        // SAFETY: The native handle is valid and all fields match the C ABI exactly.
        check(unsafe {
            aml_controller_state(
                self.native.as_ptr(),
                buttons,
                left_trigger,
                right_trigger,
                left_x,
                left_y,
                right_x,
                right_y,
            )
        })
    }

    pub fn controller_departure(&mut self) -> Result<()> {
        // SAFETY: The native handle is valid and the operation is safe when already absent.
        check(unsafe { aml_controller_departure(self.native.as_ptr()) })
    }

    pub fn request_idr(&mut self) {
        // SAFETY: The native handle is valid and the shim ignores inactive sessions.
        unsafe { aml_request_idr(self.native.as_ptr()) };
    }

    pub fn network_stats(&self) -> Result<NetworkStats> {
        let mut stats = NativeNetworkStats::default();
        // SAFETY: The native handle is live and `stats` points to writable storage matching
        // the C ABI for the duration of this synchronous call.
        check(unsafe { aml_session_network_stats(self.native.as_ptr(), &mut stats) })?;
        Ok(NetworkStats {
            audio_packets: stats.audio_packets,
            audio_fec_recovered: stats.audio_fec_recovered,
            audio_fec_failed: stats.audio_fec_failed,
            audio_out_of_sequence: stats.audio_out_of_sequence,
            audio_invalid: stats.audio_invalid,
            video_packets: stats.video_packets,
            video_fec_recovered: stats.video_fec_recovered,
            video_fec_failed: stats.video_fec_failed,
            video_out_of_sequence: stats.video_out_of_sequence,
            video_invalid: stats.video_invalid,
        })
    }

    #[must_use]
    pub fn media_ingress_stats(&self) -> MediaIngressStats {
        // SAFETY: `Session` uniquely owns this allocation until native teardown completes in
        // `Drop`. Callback threads update only atomic counters and channel senders.
        unsafe { self.callback_context.as_ref() }.media_ingress_stats()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
        // SAFETY: Stop has completed and joins all native transport workers. Destroy releases
        // the uniquely owned native allocation and cannot invoke further callbacks afterward.
        unsafe { aml_session_destroy(self.native.as_ptr()) };
        // SAFETY: Native destruction has completed, so callback userdata is no longer reachable
        // from C and can be reconstructed exactly once.
        unsafe { drop(Box::from_raw(self.callback_context.as_ptr())) };
    }
}

fn check(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Native(code))
    }
}

fn callback_context(userdata: *mut c_void) -> Option<&'static CallbackContext> {
    let pointer = NonNull::new(userdata.cast::<CallbackContext>())?;
    // SAFETY: The userdata pointer is a live `CallbackContext` allocation for the entire
    // callback lifetime. It is reclaimed only after native session destruction.
    Some(unsafe { pointer.as_ref() })
}

fn callback_name(name: *const c_char) -> String {
    if name.is_null() {
        return "unknown stage".to_owned();
    }
    // SAFETY: moonlight-common-c supplies a NUL-terminated static stage name.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

extern "C" fn on_stage(userdata: *mut c_void, name: *const c_char, state: i32, error: i32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(context) = callback_context(userdata) else {
            return;
        };
        let name = callback_name(name);
        let event = match state {
            0 => StreamEvent::StageStarting(name),
            1 => StreamEvent::StageComplete(name),
            _ => StreamEvent::StageFailed { name, error },
        };
        let _ = context.control.send(event);
    }));
}

extern "C" fn on_connected(userdata: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(context) = callback_context(userdata) {
            let _ = context.control.send(StreamEvent::Connected);
        }
    }));
}

extern "C" fn on_terminated(userdata: *mut c_void, error: i32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(context) = callback_context(userdata) {
            let _ = context.control.send(StreamEvent::Terminated(error));
        }
    }));
}

extern "C" fn on_connection_status(userdata: *mut c_void, status: i32) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(context) = callback_context(userdata) else {
            return;
        };
        let quality = if status == 0 {
            ConnectionQuality::Okay
        } else {
            ConnectionQuality::Poor
        };
        let _ = context.control.send(StreamEvent::ConnectionStatus(quality));
    }));
}

fn hdr_metadata(pointer: *const NativeHdrMetadata) -> Option<HdrMetadata> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: Native callbacks pass either null or a pointer to an initialized metadata value
    // that remains valid for the duration of the callback. The plain fields are copied here.
    let metadata = unsafe { *pointer };
    Some(HdrMetadata {
        display_primaries_x: metadata.display_primaries_x,
        display_primaries_y: metadata.display_primaries_y,
        white_point_x: metadata.white_point_x,
        white_point_y: metadata.white_point_y,
        max_display_luminance: metadata.max_display_luminance,
        min_display_luminance: metadata.min_display_luminance,
        max_content_light_level: metadata.max_content_light_level,
        max_frame_average_light_level: metadata.max_frame_average_light_level,
        max_full_frame_luminance: metadata.max_full_frame_luminance,
    })
}

extern "C" fn on_hdr_mode(userdata: *mut c_void, active: i32, metadata: *const NativeHdrMetadata) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(context) = callback_context(userdata) {
            let color = VideoColorInfo {
                hdr_active: active != 0,
                color_space: if active != 0 {
                    VideoColorSpace::Rec2020
                } else {
                    VideoColorSpace::Rec709
                },
                hdr_metadata: hdr_metadata(metadata),
            };
            let _ = context.control.send(StreamEvent::HdrModeChanged(color));
        }
    }));
}

extern "C" fn on_video_setup(
    userdata: *mut c_void,
    format: i32,
    width: i32,
    height: i32,
    fps: i32,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(context) = callback_context(userdata) else {
            return -1;
        };
        context
            .video
            .send(StreamEvent::VideoSetup {
                format,
                width,
                height,
                fps,
            })
            .map_or(-1, |()| 0)
    }))
    .unwrap_or(-1)
}

extern "C" fn on_video_frame(
    userdata: *mut c_void,
    data: *const u8,
    length: usize,
    frame_type: i32,
    presentation_time_us: u64,
    hdr_active: i32,
    color_space: i32,
    metadata: *const NativeHdrMetadata,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(context) = callback_context(userdata) else {
            return;
        };
        if data.is_null() || length == 0 {
            return;
        }
        // SAFETY: The C shim guarantees this buffer is valid for exactly this callback. It is
        // copied into owned Rust memory before returning.
        let bytes = unsafe { slice::from_raw_parts(data, length) }.to_vec();
        context.send_video(StreamEvent::VideoFrame {
            bytes,
            key_frame: frame_type == 1,
            presentation_time_us,
            color: VideoColorInfo {
                hdr_active: hdr_active != 0,
                color_space: VideoColorSpace::from_native(color_space),
                hdr_metadata: hdr_metadata(metadata),
            },
        });
    }));
}

#[allow(clippy::too_many_arguments)]
extern "C" fn on_audio_setup(
    userdata: *mut c_void,
    sample_rate: i32,
    channels: i32,
    streams: i32,
    coupled_streams: i32,
    samples_per_frame: i32,
    mapping: *const u8,
    mapping_length: usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(context) = callback_context(userdata) else {
            return -1;
        };
        if mapping.is_null() || mapping_length > 8 {
            return -1;
        }
        // SAFETY: The native Opus mapping has `mapping_length` initialized bytes and remains
        // valid for this callback. The bytes are copied before returning.
        let mapping = unsafe { slice::from_raw_parts(mapping, mapping_length) }.to_vec();
        context
            .audio
            .send(StreamEvent::AudioSetup {
                sample_rate,
                channels,
                streams,
                coupled_streams,
                samples_per_frame,
                mapping,
            })
            .map_or(-1, |()| 0)
    }))
    .unwrap_or(-1)
}

extern "C" fn on_audio_packet(userdata: *mut c_void, data: *const u8, length: usize) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(context) = callback_context(userdata) else {
            return;
        };
        let packet = if length == 0 {
            Vec::new()
        } else {
            if data.is_null() {
                return;
            }
            // SAFETY: The C shim guarantees this borrowed packet is valid for the callback. It is
            // copied into owned Rust memory before returning.
            unsafe { slice::from_raw_parts(data, length) }.to_vec()
        };
        context.send_audio(StreamEvent::AudioPacket(packet));
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use crossbeam_channel::{bounded, unbounded};

    use super::CallbackContext;
    use crate::{StreamEvent, VideoColorInfo};

    #[test]
    fn media_ingress_counts_video_queue_drops() {
        let (control, _control_receiver) = unbounded();
        let (audio, _audio_receiver) = unbounded();
        let (video, _video_receiver) = bounded(1);
        let context = CallbackContext {
            control,
            audio,
            video,
            audio_packets: AtomicU64::new(0),
            audio_bytes: AtomicU64::new(0),
            video_frames: AtomicU64::new(0),
            video_bytes: AtomicU64::new(0),
            video_queue_dropped: AtomicU64::new(0),
        };

        context.send_video(StreamEvent::VideoFrame {
            bytes: vec![1, 2],
            key_frame: true,
            presentation_time_us: 1,
            color: VideoColorInfo::default(),
        });
        context.send_video(StreamEvent::VideoFrame {
            bytes: vec![3, 4, 5],
            key_frame: false,
            presentation_time_us: 2,
            color: VideoColorInfo::default(),
        });
        context.send_audio(StreamEvent::AudioPacket(vec![6, 7, 8, 9]));

        assert_eq!(
            context.media_ingress_stats(),
            crate::MediaIngressStats {
                audio_packets: 1,
                audio_bytes: 4,
                video_frames: 2,
                video_bytes: 5,
                video_queue_dropped: 1,
            }
        );
    }
}
