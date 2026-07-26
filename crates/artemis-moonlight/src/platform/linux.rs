//! Audited Linux FFI implementation.
//!
//! Unsafe code is confined to this module and each operation documents its safety invariant.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Sender, bounded, unbounded};

use crate::{Error, EventReceiver, Result, StreamConfig, StreamEvent};

static ACTIVE: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct NativeStartConfig {
    address: *const c_char,
    app_version: *const c_char,
    gfe_version: *const c_char,
    rtsp_session_url: *const c_char,
    server_codec_mode_support: i32,
    width: i32,
    height: i32,
    fps: i32,
    bitrate_kbps: i32,
    packet_size: i32,
    audio_configuration: i32,
    client_refresh_rate_x100: i32,
    remote_input_key: [u8; 16],
    remote_input_iv: [u8; 16],
}

type StageCallback = extern "C" fn(*mut c_void, *const c_char, i32, i32);
type ConnectedCallback = extern "C" fn(*mut c_void);
type TerminatedCallback = extern "C" fn(*mut c_void, i32);
type VideoSetupCallback = extern "C" fn(*mut c_void, i32, i32, i32, i32) -> i32;
type VideoFrameCallback = extern "C" fn(*mut c_void, *const u8, usize, i32, u64);
type AudioSetupCallback =
    extern "C" fn(*mut c_void, i32, i32, i32, i32, i32, *const u8, usize) -> i32;
type AudioPacketCallback = extern "C" fn(*mut c_void, *const u8, usize);

#[repr(C)]
struct NativeCallbacks {
    userdata: *mut c_void,
    stage: Option<StageCallback>,
    connected: Option<ConnectedCallback>,
    terminated: Option<TerminatedCallback>,
    video_setup: Option<VideoSetupCallback>,
    video_frame: Option<VideoFrameCallback>,
    audio_setup: Option<AudioSetupCallback>,
    audio_packet: Option<AudioPacketCallback>,
}

#[repr(C)]
struct NativeSession {
    _private: [u8; 0],
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
    fn aml_mouse_move(session: *mut NativeSession, x: i16, y: i16) -> i32;
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
    media: Sender<StreamEvent>,
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
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_kbps: config.bitrate_kbps,
            packet_size: config.packet_size,
            audio_configuration: config.audio_configuration,
            client_refresh_rate_x100: config.client_refresh_rate_x100,
            remote_input_key: config.remote_input_key,
            remote_input_iv: config.remote_input_iv,
        };

        let (control_sender, control_receiver) = unbounded();
        let (media_sender, media_receiver) = bounded(128);
        let callback_context = Box::new(CallbackContext {
            control: control_sender,
            media: media_sender,
        });
        let callback_context =
            NonNull::new(Box::into_raw(callback_context)).ok_or(Error::Allocation)?;
        let callbacks = NativeCallbacks {
            userdata: callback_context.as_ptr().cast(),
            stage: Some(on_stage),
            connected: Some(on_connected),
            terminated: Some(on_terminated),
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
                media: media_receiver,
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
            .control
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
        let _ = context.media.try_send(StreamEvent::VideoFrame {
            bytes,
            key_frame: frame_type == 1,
            presentation_time_us,
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
            .control
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
        if data.is_null() || length == 0 {
            return;
        }
        // SAFETY: The C shim guarantees this borrowed packet is valid for the callback. It is
        // copied into owned Rust memory before returning.
        let packet = unsafe { slice::from_raw_parts(data, length) }.to_vec();
        let _ = context.media.try_send(StreamEvent::AudioPacket(packet));
    }));
}
