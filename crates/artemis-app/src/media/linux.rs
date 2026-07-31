use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex, OnceLock, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use eframe::glow::{self, HasContext};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_gl as gst_gl;
use gstreamer_gl::prelude::*;
use gstreamer_gl_egl as gst_gl_egl;
use gstreamer_video as gst_video;

use artemis_moonlight::{
    AudioEventReceiver, HdrMetadata, NetworkStats, Session, StreamEvent, VideoBitDepth, VideoCodec,
    VideoColorInfo, VideoEventReceiver,
};

use super::{DecoderCapabilities, DecoderSupport, HdrDisplayCapabilities, StreamDiagnostics};

type EglGetCurrentDisplay = unsafe extern "C" fn() -> *mut c_void;

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub(crate) sample: gst::Sample,
    pub(crate) gl_context: Option<gst_gl::GLContext>,
    pub presentation_time_us: u64,
    pub color: VideoColorInfo,
}

#[derive(Clone)]
pub struct GlInteropContext {
    display: gst_gl::GLDisplay,
    context: gst_gl::GLContext,
    presentation_bit_depth: u8,
}

impl GlInteropContext {
    #[allow(unsafe_code)]
    pub fn new(context: &eframe::CreationContext<'_>) -> Result<Option<Self>, String> {
        gst::init().map_err(|error| error.to_string())?;
        let Some(get_proc_address) = context.get_proc_address else {
            return Ok(None);
        };
        let egl_get_current_display = get_proc_address(c"eglGetCurrentDisplay");
        if egl_get_current_display.is_null() {
            return Ok(None);
        }
        // SAFETY: eframe's GL loader returned the address for the exact EGL function signature.
        let egl_get_current_display = unsafe {
            std::mem::transmute::<*const c_void, EglGetCurrentDisplay>(egl_get_current_display)
        };
        // SAFETY: eframe's OpenGL context is current while the app creator is running.
        let egl_display = unsafe { egl_get_current_display() } as usize;
        let egl_context = gst_gl::GLContext::current_gl_context(gst_gl::GLPlatform::EGL);
        if egl_display == 0 || egl_context == 0 {
            return Ok(None);
        }
        let api = current_gl_api(context)?;
        let presentation_bit_depth = current_framebuffer_bit_depth(context)?;
        // SAFETY: Both handles are queried from eframe's current EGL context. The display and
        // context remain owned by eframe; the GStreamer wrappers are explicitly foreign wrappers.
        let display = unsafe { gst_gl_egl::GLDisplayEGL::with_egl_display(egl_display) }
            .map_err(|error| error.to_string())?
            .upcast::<gst_gl::GLDisplay>();
        // SAFETY: The EGL context handle belongs to the wrapped EGL display and remains valid for
        // the whole eframe application lifetime.
        let wrapped = unsafe {
            gst_gl::GLContext::new_wrapped(&display, egl_context, gst_gl::GLPlatform::EGL, api)
        }
        .ok_or_else(|| "GStreamer could not wrap eframe's EGL context".to_owned())?;
        wrapped.activate(true).map_err(|error| error.to_string())?;
        wrapped.fill_info().map_err(|error| error.to_string())?;
        Ok(Some(Self {
            display,
            context: wrapped,
            presentation_bit_depth,
        }))
    }

    #[must_use]
    pub const fn presentation_bit_depth(&self) -> u8 {
        self.presentation_bit_depth
    }

    fn configure_pipeline(&self, pipeline: &gst::Pipeline) -> Result<(), String> {
        let display_context = gl_display_context(&self.display);
        let app_context = gl_app_context(&self.context)?;
        pipeline.set_context(&display_context);
        pipeline.set_context(&app_context);

        let display = self.display.clone();
        let context = self.context.clone();
        let bus = pipeline
            .bus()
            .ok_or_else(|| "video pipeline has no message bus".to_owned())?;
        bus.set_sync_handler(move |_, message| {
            provide_gl_context(message, &display, &context);
            gst::BusSyncReply::Pass
        });
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VideoDecoder {
    codec: VideoCodec,
    element: &'static str,
    label: &'static str,
    hardware: bool,
}

const VA_H264_DECODER: VideoDecoder = VideoDecoder {
    codec: VideoCodec::H264,
    element: "vah264dec",
    label: "VA-API H.264 (vah264dec)",
    hardware: true,
};
const SOFTWARE_H264_DECODER: VideoDecoder = VideoDecoder {
    codec: VideoCodec::H264,
    element: "avdec_h264",
    label: "Software H.264 (avdec_h264)",
    hardware: false,
};
const H264_DECODERS: [VideoDecoder; 2] = [VA_H264_DECODER, SOFTWARE_H264_DECODER];
const VA_HEVC_DECODER: VideoDecoder = VideoDecoder {
    codec: VideoCodec::Hevc,
    element: "vah265dec",
    label: "VA-API HEVC (vah265dec)",
    hardware: true,
};
const SOFTWARE_HEVC_DECODER: VideoDecoder = VideoDecoder {
    codec: VideoCodec::Hevc,
    element: "avdec_h265",
    label: "Software HEVC (avdec_h265)",
    hardware: false,
};
const HEVC_DECODERS: [VideoDecoder; 2] = [VA_HEVC_DECODER, SOFTWARE_HEVC_DECODER];
const VA_AV1_DECODER: VideoDecoder = VideoDecoder {
    codec: VideoCodec::Av1,
    element: "vaav1dec",
    label: "VA-API AV1 (vaav1dec)",
    hardware: true,
};
const AV1_DECODERS: [VideoDecoder; 4] = [
    VA_AV1_DECODER,
    VideoDecoder {
        codec: VideoCodec::Av1,
        element: "av1dec",
        label: "Software AV1 (av1dec)",
        hardware: false,
    },
    VideoDecoder {
        codec: VideoCodec::Av1,
        element: "dav1ddec",
        label: "Software AV1 (dav1ddec)",
        hardware: false,
    },
    VideoDecoder {
        codec: VideoCodec::Av1,
        element: "avdec_av1",
        label: "Software AV1 (avdec_av1)",
        hardware: false,
    },
];
// One 64x64 Main-profile 8-bit 4:2:0 key frame in an OBU temporal unit.
const AV1_HARDWARE_PROBE: [u8; 32] = [
    18, 0, 10, 11, 0, 0, 0, 2, 175, 255, 240, 54, 190, 64, 16, 50, 15, 16, 128, 128, 1, 0, 0, 0,
    75, 23, 198, 61, 252, 191, 255, 160,
];
static AV1_HARDWARE_USABLE: OnceLock<bool> = OnceLock::new();

fn video_decoder_candidates(codec: VideoCodec) -> &'static [VideoDecoder] {
    match codec {
        VideoCodec::H264 => &H264_DECODERS,
        VideoCodec::Hevc => &HEVC_DECODERS,
        VideoCodec::Av1 => &AV1_DECODERS,
    }
}

fn select_video_decoder(codec: VideoCodec, bit_depth: VideoBitDepth) -> Option<VideoDecoder> {
    video_decoder_candidates(codec)
        .iter()
        .copied()
        .find(|decoder| {
            decoder_is_usable(*decoder)
                && (bit_depth == VideoBitDepth::Eight || decoder_supports_main10(*decoder))
        })
}

fn decoder_support(codec: VideoCodec) -> DecoderSupport {
    let candidates = video_decoder_candidates(codec);
    DecoderSupport {
        available: candidates.iter().any(|decoder| decoder_is_usable(*decoder)),
        hardware: candidates
            .iter()
            .any(|decoder| decoder.hardware && decoder_is_usable(*decoder)),
        main10: matches!(codec, VideoCodec::Hevc | VideoCodec::Av1)
            && candidates.iter().any(|decoder| {
                decoder.hardware && decoder_is_usable(*decoder) && decoder_supports_main10(*decoder)
            }),
    }
}

fn decoder_supports_main10(decoder: VideoDecoder) -> bool {
    if !decoder.hardware || !matches!(decoder.codec, VideoCodec::Hevc | VideoCodec::Av1) {
        return false;
    }
    let Some(factory) = gst::ElementFactory::find(decoder.element) else {
        return false;
    };
    let p010 = gst::Caps::builder("video/x-raw")
        .field("format", "P010_10LE")
        .build();
    factory.static_pad_templates().iter().any(|template| {
        template.direction() == gst::PadDirection::Src && template.caps().can_intersect(&p010)
    })
}

fn decoder_is_usable(decoder: VideoDecoder) -> bool {
    if gst::ElementFactory::find(decoder.element).is_none() {
        return false;
    }
    decoder.codec != VideoCodec::Av1 || !decoder.hardware || av1_hardware_is_usable()
}

fn av1_hardware_is_usable() -> bool {
    *AV1_HARDWARE_USABLE.get_or_init(|| match probe_av1_hardware_decoder() {
        Ok(()) => {
            tracing::info!(
                target: "artemis::media",
                "VA-API AV1 decoder passed the startup bitstream probe"
            );
            true
        }
        Err(error) => {
            tracing::warn!(
                target: "artemis::media",
                %error,
                "VA-API AV1 decoder failed its startup probe; using software AV1"
            );
            false
        }
    })
}

fn probe_av1_hardware_decoder() -> Result<(), String> {
    if gst::ElementFactory::find(VA_AV1_DECODER.element).is_none() {
        return Err("vaav1dec is not installed".to_owned());
    }
    let pipeline = gst::parse::launch(
        "appsrc name=probe_src is-live=false format=time \
         caps=video/x-av1,stream-format=obu-stream,alignment=tu ! \
         av1parse ! video/x-av1,stream-format=obu-stream,alignment=frame ! \
         vaav1dec ! fakesink sync=false",
    )
    .map_err(|error| error.to_string())?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "GStreamer did not construct the AV1 probe pipeline".to_owned())?;
    let result = run_av1_hardware_probe(&pipeline);
    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn run_av1_hardware_probe(pipeline: &gst::Pipeline) -> Result<(), String> {
    let source = pipeline
        .by_name("probe_src")
        .ok_or_else(|| "AV1 probe source is missing".to_owned())?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "AV1 probe source has the wrong type".to_owned())?;
    let bus = pipeline
        .bus()
        .ok_or_else(|| "AV1 probe pipeline has no message bus".to_owned())?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| error.to_string())?;
    source
        .push_buffer(gst::Buffer::from_slice(AV1_HARDWARE_PROBE))
        .map_err(|error| error.to_string())?;
    source.end_of_stream().map_err(|error| error.to_string())?;
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(2),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .ok_or_else(|| "VA-API AV1 probe timed out".to_owned())?;
    match message.view() {
        gst::MessageView::Eos(_) => Ok(()),
        gst::MessageView::Error(error) => Err(format!(
            "{} ({:?})",
            error.error(),
            error.debug().map(|value| value.to_string())
        )),
        _ => Err("VA-API AV1 probe returned an unexpected message".to_owned()),
    }
}

#[must_use]
pub fn decoder_capabilities() -> DecoderCapabilities {
    if let Err(error) = gst::init() {
        tracing::warn!(target: "artemis::media", %error, "could not inspect video decoders");
        return DecoderCapabilities::default();
    }
    DecoderCapabilities {
        h264: decoder_support(VideoCodec::H264),
        hevc: decoder_support(VideoCodec::Hevc),
        av1: decoder_support(VideoCodec::Av1),
        presentation_bit_depth: 0,
    }
}

#[must_use]
pub fn hdr_display_capabilities() -> HdrDisplayCapabilities {
    let connected = fs::read_dir("/sys/class/drm").ok().and_then(|entries| {
        entries.flatten().find_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().into_string().ok()?;
            if !name.contains('-')
                || fs::read_to_string(path.join("status")).ok()?.trim() != "connected"
            {
                return None;
            }
            let edid = fs::read(path.join("edid")).ok()?;
            Some((name, edid_supports_hdr10(&edid)))
        })
    });
    let (output_name, display_hdr10) =
        connected.map_or((None, false), |(name, hdr)| (Some(name), hdr));
    let compositor = crate::hdr_surface::probe_compositor_support();
    let native_hdr_presentation = display_hdr10 && compositor.is_ok();
    let presentation_reason = if display_hdr10 {
        compositor.map_or_else(
            |error| {
                format!(
                    "The connected display supports HDR10, but {error}. Artemis will use the GPU SDR tone-map fallback."
                )
            },
            |()| {
                "The connected display and Wayland compositor support native 10-bit BT.2020/PQ HDR presentation."
                    .to_owned()
            },
        )
    } else {
        "The connected display does not advertise HDR10/PQ in its EDID; Artemis will use the GPU SDR tone-map fallback."
            .to_owned()
    };
    HdrDisplayCapabilities {
        output_name,
        display_hdr10,
        native_hdr_presentation,
        presentation_reason,
    }
}

fn edid_supports_hdr10(edid: &[u8]) -> bool {
    edid.chunks_exact(128).skip(1).any(|block| {
        if block.first() != Some(&0x02) {
            return false;
        }
        let end = usize::from(block[2]);
        if !(4..=127).contains(&end) {
            return false;
        }
        let mut offset = 4;
        while offset < end {
            let header = block[offset];
            let length = usize::from(header & 0x1f);
            let next = offset.saturating_add(1).saturating_add(length);
            if next > end {
                return false;
            }
            let payload = &block[offset + 1..next];
            let extended_tag = header >> 5 == 0x07 && payload.first() == Some(&0x06);
            let pq = payload.get(1).is_some_and(|eotf| eotf & 0x04 != 0);
            if extended_tag && pq {
                return true;
            }
            offset = next;
        }
        false
    })
}

#[derive(Clone, Copy)]
struct VideoPipelineDetails {
    decoder: &'static str,
    memory_path: &'static str,
    bit_depth: &'static str,
    hdr_source_active: bool,
    color_space: &'static str,
    hdr_metadata_available: bool,
    hdr_max_content_light_level: Option<u16>,
    hdr_presentation: &'static str,
}

impl Default for VideoPipelineDetails {
    fn default() -> Self {
        Self {
            decoder: "Waiting for negotiated video",
            memory_path: "Pending",
            bit_depth: "Pending",
            hdr_source_active: false,
            color_space: "Pending",
            hdr_metadata_available: false,
            hdr_max_content_light_level: None,
            hdr_presentation: "Pending",
        }
    }
}

#[derive(Default)]
struct VideoPipelineMetadata {
    details: RwLock<VideoPipelineDetails>,
}

#[derive(Clone, Copy)]
struct AudioPipelineDetails {
    layout: &'static str,
    output: &'static str,
}

impl Default for AudioPipelineDetails {
    fn default() -> Self {
        Self {
            layout: "Waiting for negotiated audio",
            output: "Pending",
        }
    }
}

#[derive(Default)]
struct AudioPipelineMetadata {
    details: RwLock<AudioPipelineDetails>,
}

impl AudioPipelineMetadata {
    fn update(&self, details: AudioPipelineDetails) {
        if let Ok(mut current) = self.details.write() {
            *current = details;
        }
    }

    fn snapshot(&self) -> AudioPipelineDetails {
        self.details
            .read()
            .map_or_else(|_| AudioPipelineDetails::default(), |details| *details)
    }
}

impl VideoPipelineMetadata {
    fn update(&self, details: VideoPipelineDetails) {
        if let Ok(mut current) = self.details.write() {
            *current = details;
        }
    }

    fn snapshot(&self) -> VideoPipelineDetails {
        self.details
            .read()
            .map_or_else(|_| VideoPipelineDetails::default(), |details| *details)
    }

    fn update_color(&self, color: VideoColorInfo) {
        if let Ok(mut current) = self.details.write() {
            let was_hdr_active = current.hdr_source_active;
            current.hdr_source_active = color.hdr_active;
            current.color_space = color.color_space.label();
            current.hdr_metadata_available = color.hdr_metadata.is_some();
            current.hdr_max_content_light_level = color
                .hdr_metadata
                .map(|metadata| metadata.max_content_light_level)
                .filter(|value| *value > 0);
            current.hdr_presentation = if color.hdr_active && !was_hdr_active {
                "Negotiating HDR presentation"
            } else if color.hdr_active {
                current.hdr_presentation
            } else {
                "SDR compositor output"
            };
        }
    }

    fn set_hdr_presentation(&self, hdr_active: bool, native: bool) {
        if let Ok(mut current) = self.details.write() {
            current.hdr_presentation = if !hdr_active {
                "SDR compositor output"
            } else if native {
                "Native HDR10 (BT.2020/PQ)"
            } else {
                "HDR source to SDR tone map"
            };
        }
    }
}

fn provide_gl_context(
    message: &gst::Message,
    display: &gst_gl::GLDisplay,
    context: &gst_gl::GLContext,
) {
    let gst::MessageView::NeedContext(needed) = message.view() else {
        return;
    };
    let Some(element) = message
        .src()
        .and_then(|source| source.downcast_ref::<gst::Element>())
    else {
        return;
    };
    match needed.context_type() {
        context_type if context_type == gst_gl::GL_DISPLAY_CONTEXT_TYPE.as_str() => {
            element.set_context(&gl_display_context(display));
        }
        "gst.gl.app_context" => {
            if let Ok(app_context) = gl_app_context(context) {
                element.set_context(&app_context);
            }
        }
        _ => {}
    }
}

#[allow(unsafe_code)]
fn current_gl_api(context: &eframe::CreationContext<'_>) -> Result<gst_gl::GLAPI, String> {
    let gl = context
        .gl
        .as_ref()
        .ok_or_else(|| "eframe did not expose its OpenGL context".to_owned())?;
    // SAFETY: eframe's OpenGL context is current while the app creator is running.
    let version = unsafe { gl.get_parameter_string(glow::VERSION) };
    if version.starts_with("OpenGL ES") {
        Ok(gst_gl::GLAPI::GLES2)
    } else {
        Ok(gst_gl::GLAPI::OPENGL | gst_gl::GLAPI::OPENGL3)
    }
}

#[allow(unsafe_code)]
fn current_framebuffer_bit_depth(context: &eframe::CreationContext<'_>) -> Result<u8, String> {
    let gl = context
        .gl
        .as_ref()
        .ok_or_else(|| "eframe did not expose its OpenGL context".to_owned())?;
    // SAFETY: eframe's OpenGL context is current while the app creator is running. Core OpenGL
    // requires attachment queries for channel sizes; the legacy GL_RED_BITS query is invalid.
    let channel_bits = unsafe {
        while gl.get_error() != glow::NO_ERROR {}
        let framebuffer = gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING);
        let attachment = if framebuffer == 0 {
            glow::BACK_LEFT
        } else {
            glow::COLOR_ATTACHMENT0
        };
        [
            gl.get_framebuffer_attachment_parameter_i32(
                glow::DRAW_FRAMEBUFFER,
                attachment,
                glow::FRAMEBUFFER_ATTACHMENT_RED_SIZE,
            ),
            gl.get_framebuffer_attachment_parameter_i32(
                glow::DRAW_FRAMEBUFFER,
                attachment,
                glow::FRAMEBUFFER_ATTACHMENT_GREEN_SIZE,
            ),
            gl.get_framebuffer_attachment_parameter_i32(
                glow::DRAW_FRAMEBUFFER,
                attachment,
                glow::FRAMEBUFFER_ATTACHMENT_BLUE_SIZE,
            ),
        ]
    };
    // SAFETY: This only reads and clears the current context's error flag.
    let error = unsafe { gl.get_error() };
    if error != glow::NO_ERROR {
        return Err(format!(
            "OpenGL framebuffer color-depth query failed with error 0x{error:x}"
        ));
    }
    let minimum = channel_bits.into_iter().min().unwrap_or_default();
    u8::try_from(minimum).map_err(|_| "OpenGL reported an invalid color depth".to_owned())
}

fn gl_display_context(display: &gst_gl::GLDisplay) -> gst::Context {
    let context = gst::Context::new(gst_gl::GL_DISPLAY_CONTEXT_TYPE.as_str(), true);
    context.set_gl_display(Some(display));
    context
}

fn gl_app_context(context: &gst_gl::GLContext) -> Result<gst::Context, String> {
    let mut app_context = gst::Context::new("gst.gl.app_context", true);
    app_context
        .get_mut()
        .ok_or_else(|| "GStreamer GL app context is unexpectedly shared".to_owned())?
        .structure_mut()
        .set("context", context.clone());
    Ok(app_context)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoStats {
    pub submitted: u64,
    pub decoded: u64,
    pub dropped: u64,
    pub presented: u64,
}

#[derive(Default)]
struct VideoCounters {
    submitted: AtomicU64,
    decoded: AtomicU64,
    dropped: AtomicU64,
    presented: AtomicU64,
}

impl VideoCounters {
    fn snapshot(&self) -> VideoStats {
        VideoStats {
            submitted: self.submitted.load(Ordering::Relaxed),
            decoded: self.decoded.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            presented: self.presented.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AudioStats {
    packets: u64,
    media_us: u64,
    frame_duration_us: u64,
    first_output_elapsed_us: u64,
    last_output_elapsed_us: u64,
}

#[derive(Default)]
struct AudioCounters {
    packets: AtomicU64,
    media_us: AtomicU64,
    frame_duration_us: AtomicU64,
    first_output_elapsed_us: AtomicU64,
    last_output_elapsed_us: AtomicU64,
}

impl AudioCounters {
    fn reset(&self, frame_duration_us: u64) {
        self.packets.store(0, Ordering::Relaxed);
        self.media_us.store(0, Ordering::Relaxed);
        self.frame_duration_us
            .store(frame_duration_us, Ordering::Relaxed);
        self.first_output_elapsed_us.store(0, Ordering::Relaxed);
        self.last_output_elapsed_us.store(0, Ordering::Relaxed);
    }

    fn record_output(&self, origin: Instant, duration_us: u64) {
        let elapsed_us = duration_micros(origin.elapsed()).saturating_add(1);
        let _ = self.first_output_elapsed_us.compare_exchange(
            0,
            elapsed_us,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        self.last_output_elapsed_us
            .store(elapsed_us, Ordering::Relaxed);
        self.media_us.fetch_add(duration_us, Ordering::Relaxed);
        self.packets.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> AudioStats {
        AudioStats {
            packets: self.packets.load(Ordering::Relaxed),
            media_us: self.media_us.load(Ordering::Relaxed),
            frame_duration_us: self.frame_duration_us.load(Ordering::Relaxed),
            first_output_elapsed_us: self.first_output_elapsed_us.load(Ordering::Relaxed),
            last_output_elapsed_us: self.last_output_elapsed_us.load(Ordering::Relaxed),
        }
    }
}

pub struct MediaRuntime {
    video: VideoWorker,
    audio: AudioWorker,
    frames: Receiver<DecodedFrame>,
    video_counters: Arc<VideoCounters>,
    video_metadata: Arc<VideoPipelineMetadata>,
    audio_counters: Arc<AudioCounters>,
    audio_metadata: Arc<AudioPipelineMetadata>,
    audio_muted: Arc<AtomicBool>,
    first_presented_at: Option<Instant>,
    last_presented_at: Option<Instant>,
    first_presented_pts_us: Option<u64>,
    last_presented_pts_us: Option<u64>,
    last_video_report_at: Instant,
    last_video_report: VideoStats,
    last_ingress_report: artemis_moonlight::MediaIngressStats,
    last_network_report: NetworkStats,
    diagnostics: StreamDiagnostics,
    frame_pacing_active: bool,
}

impl MediaRuntime {
    pub fn new(
        audio_events: AudioEventReceiver,
        video_events: VideoEventReceiver,
        gl_interop: Option<GlInteropContext>,
        frame_pacing: bool,
    ) -> Result<Self, String> {
        gst::init().map_err(|error| error.to_string())?;
        let clock_origin = Instant::now();
        let (frame_sender, frames) = bounded(2);
        let video_counters = Arc::new(VideoCounters::default());
        let video_metadata = Arc::new(VideoPipelineMetadata::default());
        let audio_counters = Arc::new(AudioCounters::default());
        let audio_metadata = Arc::new(AudioPipelineMetadata::default());
        let audio_muted = Arc::new(AtomicBool::new(false));
        let audio = AudioWorker::spawn(
            audio_events,
            Arc::clone(&audio_counters),
            Arc::clone(&audio_metadata),
            Arc::clone(&audio_muted),
            clock_origin,
        )?;
        let video = VideoWorker::spawn(
            video_events,
            frame_sender,
            Arc::clone(&video_counters),
            Arc::clone(&video_metadata),
            gl_interop,
            frame_pacing,
        )?;
        Ok(Self {
            video,
            audio,
            frames,
            video_counters,
            video_metadata,
            audio_counters,
            audio_metadata,
            audio_muted,
            first_presented_at: None,
            last_presented_at: None,
            first_presented_pts_us: None,
            last_presented_pts_us: None,
            last_video_report_at: Instant::now(),
            last_video_report: VideoStats::default(),
            last_ingress_report: artemis_moonlight::MediaIngressStats::default(),
            last_network_report: NetworkStats::default(),
            diagnostics: StreamDiagnostics {
                decoder: VideoPipelineDetails::default().decoder,
                memory_path: VideoPipelineDetails::default().memory_path,
                video_bit_depth: VideoPipelineDetails::default().bit_depth,
                video_color_space: VideoPipelineDetails::default().color_space,
                hdr_presentation: VideoPipelineDetails::default().hdr_presentation,
                ..StreamDiagnostics::default()
            },
            frame_pacing_active: frame_pacing,
        })
    }

    pub fn try_frame(&self) -> Option<DecodedFrame> {
        let mut latest = None;
        while let Ok(frame) = self.frames.try_recv() {
            if latest.replace(frame).is_some() {
                self.video_counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        latest
    }

    pub fn record_presented(&mut self, frame: &DecodedFrame) {
        self.video_counters
            .presented
            .fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        self.first_presented_at.get_or_insert(now);
        self.first_presented_pts_us
            .get_or_insert(frame.presentation_time_us);
        self.last_presented_at = Some(now);
        self.last_presented_pts_us = Some(frame.presentation_time_us);
    }

    pub fn report_stream_stats(&mut self, session: &Session) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_video_report_at);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let current = self.video_counters.snapshot();
        let seconds = elapsed.as_secs_f64();
        let submitted_fps =
            rate_per_second(current.submitted, self.last_video_report.submitted, seconds);
        let decoded_fps = rate_per_second(current.decoded, self.last_video_report.decoded, seconds);
        let presented_fps =
            rate_per_second(current.presented, self.last_video_report.presented, seconds);
        let dropped = current
            .dropped
            .saturating_sub(self.last_video_report.dropped);
        let ingress = session.media_ingress_stats();
        let network = session.network_stats().unwrap_or_default();
        let video_ingress_fps = rate_per_second(
            ingress.video_frames,
            self.last_ingress_report.video_frames,
            seconds,
        );
        let audio_ingress_pps = rate_per_second(
            ingress.audio_packets,
            self.last_ingress_report.audio_packets,
            seconds,
        );
        let callback_queue_dropped = ingress
            .video_queue_dropped
            .saturating_sub(self.last_ingress_report.video_queue_dropped);
        let video_network_pps = rate_per_second(
            u64::from(network.video_packets),
            u64::from(self.last_network_report.video_packets),
            seconds,
        );
        let audio_network_pps = rate_per_second(
            u64::from(network.audio_packets),
            u64::from(self.last_network_report.audio_packets),
            seconds,
        );
        let video_mbps = megabits_per_second(
            ingress.video_bytes,
            self.last_ingress_report.video_bytes,
            seconds,
        );
        let audio_kbps = kilobits_per_second(
            ingress.audio_bytes,
            self.last_ingress_report.audio_bytes,
            seconds,
        );
        let video_packet_issues = video_packet_issues(&network)
            .saturating_sub(video_packet_issues(&self.last_network_report));
        let audio_packet_issues = audio_packet_issues(&network)
            .saturating_sub(audio_packet_issues(&self.last_network_report));
        let video_fec_recovered = u64::from(network.video_fec_recovered)
            .saturating_sub(u64::from(self.last_network_report.video_fec_recovered));
        let audio_fec_recovered = u64::from(network.audio_fec_recovered)
            .saturating_sub(u64::from(self.last_network_report.audio_fec_recovered));
        let video_clock = self.video_clock();
        let audio_clock = audio_clock(self.audio_counters.snapshot());
        let video_details = self.video_metadata.snapshot();
        let audio_details = self.audio_metadata.snapshot();
        self.diagnostics = StreamDiagnostics {
            video_ingress_fps,
            decoded_fps,
            presented_fps,
            decoder_queue_dropped: dropped,
            callback_queue_dropped,
            video_mbps,
            audio_kbps,
            audio_ingress_pps,
            video_network_pps,
            audio_network_pps,
            video_packet_issues,
            audio_packet_issues,
            video_fec_recovered,
            audio_fec_recovered,
            video_clock_drift_ms: video_clock.map(|clock| clock.drift),
            audio_clock_drift_ms: audio_clock.map(|clock| clock.drift),
            decoder: video_details.decoder,
            memory_path: video_details.memory_path,
            video_bit_depth: video_details.bit_depth,
            hdr_source_active: video_details.hdr_source_active,
            video_color_space: video_details.color_space,
            hdr_metadata_available: video_details.hdr_metadata_available,
            hdr_max_content_light_level: video_details.hdr_max_content_light_level,
            hdr_presentation: video_details.hdr_presentation,
            audio_layout: audio_details.layout,
            audio_output: audio_details.output,
            frame_pacing_active: self.frame_pacing_active,
        };
        trace_stream_diagnostics(&self.diagnostics, submitted_fps, video_clock, audio_clock);
        self.last_video_report = current;
        self.last_ingress_report = ingress;
        self.last_network_report = network;
        self.last_video_report_at = now;
    }

    pub fn diagnostics(&self) -> StreamDiagnostics {
        self.diagnostics
    }

    pub fn set_audio_muted(&self, muted: bool) {
        self.audio_muted.store(muted, Ordering::Relaxed);
    }

    pub fn set_hdr_presentation(&self, hdr_active: bool, native: bool) {
        self.video_metadata.set_hdr_presentation(hdr_active, native);
    }

    fn video_clock(&self) -> Option<PlaybackClock> {
        let first_at = self.first_presented_at?;
        let last_at = self.last_presented_at?;
        let first_pts = self.first_presented_pts_us?;
        let last_pts = self.last_presented_pts_us?;
        Some(PlaybackClock::new(
            last_pts.saturating_sub(first_pts),
            last_at.duration_since(first_at),
        ))
    }

    pub fn poll_error(&self) -> Option<String> {
        self.video.poll_error().or_else(|| self.audio.poll_error())
    }

    pub fn shutdown(&mut self) {
        self.video.shutdown();
        self.audio.shutdown();
    }
}

fn trace_stream_diagnostics(
    diagnostics: &StreamDiagnostics,
    submitted_fps: f64,
    video_clock: Option<PlaybackClock>,
    audio_clock: Option<PlaybackClock>,
) {
    tracing::info!(
        target: "artemis::media",
        video_ingress_fps = diagnostics.video_ingress_fps,
        submitted_fps,
        decoded_fps = diagnostics.decoded_fps,
        presented_fps = diagnostics.presented_fps,
        decoder_queue_dropped = diagnostics.decoder_queue_dropped,
        callback_queue_dropped = diagnostics.callback_queue_dropped,
        audio_ingress_pps = diagnostics.audio_ingress_pps,
        video_mbps = diagnostics.video_mbps,
        audio_kbps = diagnostics.audio_kbps,
        video_network_pps = diagnostics.video_network_pps,
        audio_network_pps = diagnostics.audio_network_pps,
        video_packet_issues = diagnostics.video_packet_issues,
        audio_packet_issues = diagnostics.audio_packet_issues,
        video_fec_recovered = diagnostics.video_fec_recovered,
        audio_fec_recovered = diagnostics.audio_fec_recovered,
        video_media_elapsed_ms = ?video_clock.map(|clock| clock.media_elapsed),
        video_wall_elapsed_ms = ?video_clock.map(|clock| clock.wall_elapsed),
        video_clock_drift_ms = ?diagnostics.video_clock_drift_ms,
        audio_media_elapsed_ms = ?audio_clock.map(|clock| clock.media_elapsed),
        audio_wall_elapsed_ms = ?audio_clock.map(|clock| clock.wall_elapsed),
        audio_clock_drift_ms = ?diagnostics.audio_clock_drift_ms,
        video_bit_depth = diagnostics.video_bit_depth,
        hdr_source_active = diagnostics.hdr_source_active,
        video_color_space = diagnostics.video_color_space,
        hdr_metadata_available = diagnostics.hdr_metadata_available,
        hdr_max_content_light_level = ?diagnostics.hdr_max_content_light_level,
        hdr_presentation = diagnostics.hdr_presentation,
        audio_layout = diagnostics.audio_layout,
        audio_output = diagnostics.audio_output,
        audio_output_latency_ms = PIPEWIRE_AUDIO_LATENCY_MS,
        "media pipeline telemetry"
    );
}

struct VideoWorker {
    stop: Sender<()>,
    errors: Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl VideoWorker {
    fn spawn(
        events: VideoEventReceiver,
        frames: crossbeam_channel::Sender<DecodedFrame>,
        video_counters: Arc<VideoCounters>,
        video_metadata: Arc<VideoPipelineMetadata>,
        gl_interop: Option<GlInteropContext>,
        frame_pacing: bool,
    ) -> Result<Self, String> {
        let (stop, stop_receiver) = bounded(1);
        let (error_sender, errors) = unbounded();
        let thread = thread::Builder::new()
            .name("artemis-video".to_owned())
            .spawn(move || {
                run_video_worker(
                    &events,
                    &stop_receiver,
                    &error_sender,
                    &frames,
                    &video_counters,
                    &video_metadata,
                    gl_interop.as_ref(),
                    frame_pacing,
                );
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stop,
            errors,
            thread: Some(thread),
        })
    }

    fn poll_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }

    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let _ = self.stop.try_send(());
        let _ = thread.join();
    }
}

impl Drop for VideoWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_video_worker(
    events: &VideoEventReceiver,
    stop: &Receiver<()>,
    errors: &Sender<String>,
    frames: &crossbeam_channel::Sender<DecodedFrame>,
    video_counters: &Arc<VideoCounters>,
    video_metadata: &Arc<VideoPipelineMetadata>,
    gl_interop: Option<&GlInteropContext>,
    frame_pacing: bool,
) {
    let mut video = None;
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        match events.recv_timeout(Duration::from_millis(10)) {
            Ok(StreamEvent::VideoSetup {
                format,
                width,
                height,
                fps: _,
            }) => {
                let result = VideoCodec::from_native_format(format).map_or_else(
                    || {
                        Err(format!(
                            "host selected unsupported video format 0x{format:x}"
                        ))
                    },
                    |codec| {
                        VideoPipeline::new(
                            codec,
                            VideoBitDepth::from_native_format(format),
                            width,
                            height,
                            frames.clone(),
                            Arc::clone(video_counters),
                            Arc::clone(video_metadata),
                            gl_interop.cloned(),
                            frame_pacing,
                        )
                    },
                );
                match result {
                    Ok((pipeline, details)) => {
                        video_metadata.update(details);
                        video = Some(pipeline);
                    }
                    Err(error) => {
                        video_metadata.update(VideoPipelineDetails::default());
                        video = None;
                        let _ = errors.send(error);
                    }
                }
            }
            Ok(StreamEvent::VideoFrame {
                bytes,
                key_frame,
                presentation_time_us,
                color,
            }) => {
                let Some(pipeline) = &video else {
                    continue;
                };
                if let Err(error) = pipeline.push(bytes, key_frame, presentation_time_us, color) {
                    video_metadata.update(VideoPipelineDetails::default());
                    video = None;
                    let _ = errors.send(error);
                }
            }
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(error) = video
            .as_ref()
            .and_then(|pipeline| pipeline_error(&pipeline.pipeline))
        {
            video_metadata.update(VideoPipelineDetails::default());
            video = None;
            let _ = errors.send(error);
        }
    }
}

struct VideoPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
    video_counters: Arc<VideoCounters>,
    video_metadata: Arc<VideoPipelineMetadata>,
    codec: VideoCodec,
    last_color: Mutex<Option<VideoColorInfo>>,
    pending_colors: Arc<Mutex<HashMap<u64, VideoColorInfo>>>,
}

impl VideoPipeline {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn new(
        codec: VideoCodec,
        bit_depth: VideoBitDepth,
        width: i32,
        height: i32,
        frames: crossbeam_channel::Sender<DecodedFrame>,
        video_counters: Arc<VideoCounters>,
        video_metadata: Arc<VideoPipelineMetadata>,
        gl_interop: Option<GlInteropContext>,
        frame_pacing: bool,
    ) -> Result<(Self, VideoPipelineDetails), String> {
        let decoder = select_video_decoder(codec, bit_depth).ok_or_else(|| {
            format!(
                "no GStreamer {} {} decoder is installed",
                codec.label(),
                bit_depth.label()
            )
        })?;
        let use_gl_interop = decoder.hardware && gl_interop.is_some();
        if bit_depth == VideoBitDepth::Ten && !use_gl_interop {
            return Err("10-bit video requires the zero-copy EGL presentation path".to_owned());
        }
        tracing::info!(
            target: "artemis::media",
            codec = codec.label(),
            bit_depth = bit_depth.label(),
            decoder = decoder.element,
            memory_path = if use_gl_interop {
                "DMABuf-to-GLMemory"
            } else {
                "system-memory"
            },
            width,
            height,
            "configuring video pipeline"
        );
        let description = video_pipeline_description(
            codec,
            bit_depth,
            decoder,
            width,
            height,
            use_gl_interop,
            frame_pacing,
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| error.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "GStreamer did not construct a video pipeline".to_owned())?;
        if use_gl_interop {
            if let Some(gl_interop) = &gl_interop {
                gl_interop.configure_pipeline(&pipeline)?;
            }
        }
        let source = pipeline
            .by_name("video_src")
            .ok_or_else(|| "video appsrc is missing".to_owned())?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| "video source has the wrong GStreamer type".to_owned())?;
        let sink = pipeline
            .by_name("video_sink")
            .ok_or_else(|| "video appsink is missing".to_owned())?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| "video sink has the wrong GStreamer type".to_owned())?;
        let gl_producer = if use_gl_interop {
            pipeline.by_name("gl_convert")
        } else {
            None
        };
        let frame_gl_context = if use_gl_interop {
            gl_interop.map(|interop| interop.context)
        } else {
            None
        };
        let pending_colors = Arc::new(Mutex::new(HashMap::new()));
        configure_video_sink(
            &sink,
            frames,
            Arc::clone(&video_counters),
            gl_producer,
            frame_gl_context,
            Arc::clone(&pending_colors),
        );
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        Ok((
            Self {
                pipeline,
                source,
                video_counters,
                video_metadata,
                codec,
                last_color: Mutex::new(None),
                pending_colors,
            },
            VideoPipelineDetails {
                decoder: decoder.label,
                memory_path: if use_gl_interop {
                    "DMABUF to GL texture"
                } else {
                    "System memory"
                },
                bit_depth: bit_depth.label(),
                hdr_source_active: false,
                color_space: "Waiting for host",
                hdr_metadata_available: false,
                hdr_max_content_light_level: None,
                hdr_presentation: if bit_depth == VideoBitDepth::Ten {
                    "Waiting for HDR mode"
                } else {
                    "SDR compositor output"
                },
            },
        ))
    }

    fn push(
        &self,
        bytes: Vec<u8>,
        key_frame: bool,
        presentation_time_us: u64,
        color: VideoColorInfo,
    ) -> Result<(), String> {
        self.video_counters
            .submitted
            .fetch_add(1, Ordering::Relaxed);
        self.video_metadata.update_color(color);
        let mut last_color = self
            .last_color
            .lock()
            .map_err(|_| "video color state is unavailable".to_owned())?;
        if last_color.as_ref() != Some(&color) {
            self.source
                .set_caps(Some(&encoded_video_caps(self.codec, color)));
            *last_color = Some(color);
        }
        drop(last_color);
        self.pending_colors
            .lock()
            .map_err(|_| "video color queue is unavailable".to_owned())?
            .insert(presentation_time_us, color);
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        if let Some(buffer) = buffer.get_mut() {
            buffer.set_pts(gst::ClockTime::from_useconds(presentation_time_us));
            if !key_frame {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        self.source
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|error| {
                if let Ok(mut colors) = self.pending_colors.lock() {
                    colors.remove(&presentation_time_us);
                }
                error.to_string()
            })
    }
}

fn configure_video_sink(
    sink: &gst_app::AppSink,
    frames: Sender<DecodedFrame>,
    counters: Arc<VideoCounters>,
    gl_producer: Option<gst::Element>,
    gl_context: Option<gst_gl::GLContext>,
    pending_colors: Arc<Mutex<HashMap<u64, VideoColorInfo>>>,
) {
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                counters.decoded.fetch_add(1, Ordering::Relaxed);
                if frames.is_full() {
                    counters.dropped.fetch_add(1, Ordering::Relaxed);
                    return Ok(gst::FlowSuccess::Ok);
                }
                let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                let info = gst_video::VideoInfo::from_caps(caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                if !matches!(
                    info.format(),
                    gst_video::VideoFormat::Nv12
                        | gst_video::VideoFormat::Rgba
                        | gst_video::VideoFormat::Rgb10a2Le
                ) {
                    return Err(gst::FlowError::NotNegotiated);
                }
                let width = usize::try_from(info.width()).map_err(|_| gst::FlowError::Error)?;
                let height = usize::try_from(info.height()).map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let presentation_time_us = buffer.pts().map_or(0, gst::ClockTime::useconds);
                let color = pending_colors
                    .lock()
                    .map_err(|_| gst::FlowError::Error)?
                    .remove(&presentation_time_us)
                    .unwrap_or_default();
                if color.hdr_active
                    && (info.format() != gst_video::VideoFormat::Rgb10a2Le
                        || info.colorimetry().transfer()
                            != gst_video::VideoTransferFunction::Smpte2084
                        || info.colorimetry().primaries() != gst_video::VideoColorPrimaries::Bt2020)
                {
                    tracing::error!(
                        target: "artemis::media",
                        format = ?info.format(),
                        colorimetry = %info.colorimetry(),
                        "HDR frame lost RGB10A2 or BT.2020/PQ signaling before presentation"
                    );
                    return Err(gst::FlowError::NotNegotiated);
                }
                if matches!(
                    info.format(),
                    gst_video::VideoFormat::Rgba | gst_video::VideoFormat::Rgb10a2Le
                ) {
                    set_gl_sync_point(buffer, gl_producer.as_ref());
                }
                let frame = DecodedFrame {
                    width,
                    height,
                    sample,
                    gl_context: gl_context.clone(),
                    presentation_time_us,
                    color,
                };
                let _ = frames.try_send(frame);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

fn set_gl_sync_point(buffer: &gst::BufferRef, producer: Option<&gst::Element>) {
    let (Some(sync), Some(producer)) = (buffer.meta::<gst_gl::GLSyncMeta>(), producer) else {
        return;
    };
    let Some(producer_context) = producer.property::<Option<gst_gl::GLContext>>("context") else {
        return;
    };
    sync.set_sync_point(&producer_context);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaybackClock {
    media_elapsed: u64,
    wall_elapsed: u64,
    drift: i64,
}

impl PlaybackClock {
    fn new(media_elapsed_us: u64, wall_elapsed: Duration) -> Self {
        let wall_elapsed_us = duration_micros(wall_elapsed);
        Self {
            media_elapsed: media_elapsed_us / 1_000,
            wall_elapsed: wall_elapsed_us / 1_000,
            drift: signed_difference_millis(media_elapsed_us, wall_elapsed_us),
        }
    }
}

fn audio_clock(stats: AudioStats) -> Option<PlaybackClock> {
    if stats.packets < 2
        || stats.frame_duration_us == 0
        || stats.first_output_elapsed_us == 0
        || stats.last_output_elapsed_us < stats.first_output_elapsed_us
    {
        return None;
    }
    let media_elapsed_us = stats.media_us.saturating_sub(stats.frame_duration_us);
    let wall_elapsed_us = stats
        .last_output_elapsed_us
        .saturating_sub(stats.first_output_elapsed_us);
    Some(PlaybackClock {
        media_elapsed: media_elapsed_us / 1_000,
        wall_elapsed: wall_elapsed_us / 1_000,
        drift: signed_difference_millis(media_elapsed_us, wall_elapsed_us),
    })
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn signed_difference_millis(left_us: u64, right_us: u64) -> i64 {
    let (positive, difference_us) = if left_us >= right_us {
        (true, left_us - right_us)
    } else {
        (false, right_us - left_us)
    };
    let converted = i64::try_from(difference_us / 1_000).unwrap_or(i64::MAX);
    if positive { converted } else { -converted }
}

fn rate_per_second(current: u64, previous: u64, seconds: f64) -> f64 {
    let frames = u32::try_from(current.saturating_sub(previous)).unwrap_or(u32::MAX);
    f64::from(frames) / seconds
}

fn megabits_per_second(current: u64, previous: u64, seconds: f64) -> f64 {
    bytes_per_second(current, previous, seconds) * 8.0 / 1_000_000.0
}

fn kilobits_per_second(current: u64, previous: u64, seconds: f64) -> f64 {
    bytes_per_second(current, previous, seconds) * 8.0 / 1_000.0
}

fn bytes_per_second(current: u64, previous: u64, seconds: f64) -> f64 {
    let bytes = u32::try_from(current.saturating_sub(previous)).unwrap_or(u32::MAX);
    f64::from(bytes) / seconds
}

fn video_packet_issues(stats: &NetworkStats) -> u64 {
    u64::from(stats.video_fec_failed)
        .saturating_add(u64::from(stats.video_out_of_sequence))
        .saturating_add(u64::from(stats.video_invalid))
}

fn audio_packet_issues(stats: &NetworkStats) -> u64 {
    u64::from(stats.audio_fec_failed)
        .saturating_add(u64::from(stats.audio_out_of_sequence))
        .saturating_add(u64::from(stats.audio_invalid))
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        let _ = self.source.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn video_pipeline_description(
    codec: VideoCodec,
    bit_depth: VideoBitDepth,
    decoder: VideoDecoder,
    width: i32,
    height: i32,
    gl_interop_available: bool,
    frame_pacing: bool,
) -> String {
    debug_assert_eq!(codec, decoder.codec);
    let encoded_chain = encoded_video_chain(codec, decoder);
    let colorimetry = if bit_depth == VideoBitDepth::Ten {
        hdr10_colorimetry().to_string()
    } else {
        sdr_colorimetry().to_string()
    };
    let sink_timing = if frame_pacing {
        "sync=true qos=true max-lateness=20000000"
    } else {
        "sync=false"
    };
    if decoder.hardware && gl_interop_available {
        let gl_format = match bit_depth {
            VideoBitDepth::Eight => "RGBA",
            VideoBitDepth::Ten => "RGB10A2_LE",
        };
        return format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=false \
             {encoded_chain} ! {} ! \
             video/x-raw(memory:DMABuf),format=DMA_DRM,colorimetry={colorimetry},\
             width={width},height={height} ! \
             glupload ! glcolorconvert name=gl_convert ! \
             video/x-raw(memory:GLMemory),format={gl_format},texture-target=2D,\
             colorimetry={colorimetry},\
             width={width},height={height} ! \
             appsink name=video_sink max-buffers=2 drop=true {sink_timing}",
            decoder.element
        );
    }
    debug_assert_eq!(bit_depth, VideoBitDepth::Eight);
    let conversion = if decoder.hardware {
        ""
    } else {
        "videoconvert ! "
    };
    format!(
        "appsrc name=video_src is-live=true format=time do-timestamp=false \
         {encoded_chain} ! {} ! {conversion}\
         video/x-raw,format=NV12,colorimetry={colorimetry},width={width},height={height} ! \
         appsink name=video_sink max-buffers=2 drop=true {sink_timing}",
        decoder.element
    )
}

fn encoded_video_chain(codec: VideoCodec, decoder: VideoDecoder) -> &'static str {
    match codec {
        VideoCodec::H264 => "! h264parse config-interval=-1",
        VideoCodec::Hevc => "! h265parse config-interval=-1",
        VideoCodec::Av1 if decoder.hardware => {
            "! av1parse ! video/x-av1,stream-format=obu-stream,alignment=frame"
        }
        VideoCodec::Av1 => "! av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu",
    }
}

fn sdr_colorimetry() -> gst_video::VideoColorimetry {
    gst_video::VideoColorimetry::new(
        gst_video::VideoColorRange::Range16_235,
        gst_video::VideoColorMatrix::Bt709,
        gst_video::VideoTransferFunction::Bt709,
        gst_video::VideoColorPrimaries::Bt709,
    )
}

fn hdr10_colorimetry() -> gst_video::VideoColorimetry {
    gst_video::VideoColorimetry::new(
        gst_video::VideoColorRange::Range16_235,
        gst_video::VideoColorMatrix::Bt2020,
        gst_video::VideoTransferFunction::Smpte2084,
        gst_video::VideoColorPrimaries::Bt2020,
    )
}

fn encoded_video_caps(codec: VideoCodec, color: VideoColorInfo) -> gst::Caps {
    let (media_type, stream_format, alignment) = match codec {
        VideoCodec::H264 => ("video/x-h264", "byte-stream", "au"),
        VideoCodec::Hevc => ("video/x-h265", "byte-stream", "au"),
        VideoCodec::Av1 => ("video/x-av1", "obu-stream", "tu"),
    };
    let colorimetry = if color.hdr_active {
        hdr10_colorimetry()
    } else {
        sdr_colorimetry()
    };
    let mut caps = gst::Caps::builder(media_type)
        .field("stream-format", stream_format)
        .field("alignment", alignment)
        .field("colorimetry", colorimetry.to_string())
        .build();
    if color.hdr_active {
        if let Some(metadata) = color.hdr_metadata {
            add_hdr_metadata_to_caps(caps.make_mut(), metadata);
        }
    }
    caps
}

fn add_hdr_metadata_to_caps(caps: &mut gst::CapsRef, metadata: HdrMetadata) {
    let coordinate = |index: usize| gst_video::VideoMasteringDisplayInfoCoordinate {
        x: metadata.display_primaries_x[index],
        y: metadata.display_primaries_y[index],
    };
    let mastering = gst_video::VideoMasteringDisplayInfo::new(
        [coordinate(0), coordinate(1), coordinate(2)],
        gst_video::VideoMasteringDisplayInfoCoordinate {
            x: metadata.white_point_x,
            y: metadata.white_point_y,
        },
        u32::from(metadata.max_display_luminance).saturating_mul(10_000),
        u32::from(metadata.min_display_luminance),
    );
    mastering.add_to_caps(caps);
    let content_light = gst_video::VideoContentLightLevel::new(
        metadata.max_content_light_level,
        metadata.max_frame_average_light_level,
    );
    content_light.add_to_caps(caps);
}

struct AudioWorker {
    stop: Sender<()>,
    errors: Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl AudioWorker {
    fn spawn(
        events: AudioEventReceiver,
        audio_counters: Arc<AudioCounters>,
        audio_metadata: Arc<AudioPipelineMetadata>,
        audio_muted: Arc<AtomicBool>,
        clock_origin: Instant,
    ) -> Result<Self, String> {
        let (stop, stop_receiver) = bounded(1);
        let (error_sender, errors) = unbounded();
        let thread = thread::Builder::new()
            .name("artemis-audio".to_owned())
            .spawn(move || {
                run_audio_worker(
                    &events,
                    &stop_receiver,
                    &error_sender,
                    &audio_counters,
                    &audio_metadata,
                    &audio_muted,
                    clock_origin,
                );
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            stop,
            errors,
            thread: Some(thread),
        })
    }

    fn poll_error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }

    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let _ = self.stop.try_send(());
        let _ = thread.join();
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_audio_worker(
    events: &AudioEventReceiver,
    stop: &Receiver<()>,
    errors: &Sender<String>,
    audio_counters: &Arc<AudioCounters>,
    audio_metadata: &Arc<AudioPipelineMetadata>,
    audio_muted: &Arc<AtomicBool>,
    clock_origin: Instant,
) {
    let telemetry = AudioTelemetry {
        counters: Arc::clone(audio_counters),
        origin: clock_origin,
    };
    let mut configuration = None;
    let mut audio = None;
    let mut retry_at = None;
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        match events.recv_timeout(Duration::from_millis(10)) {
            Ok(StreamEvent::AudioSetup {
                sample_rate,
                channels,
                streams,
                coupled_streams,
                samples_per_frame,
                mapping,
            }) => {
                let next_configuration = AudioConfiguration {
                    sample_rate,
                    channels,
                    streams,
                    coupled_streams,
                    samples_per_frame,
                    mapping,
                };
                match next_configuration
                    .create_pipeline(telemetry.clone(), audio_muted.load(Ordering::Relaxed))
                {
                    Ok(pipeline) => {
                        audio_metadata.update(pipeline.details());
                        audio = Some(pipeline);
                        retry_at = None;
                    }
                    Err(error) => {
                        audio = None;
                        retry_at = Some(Instant::now() + AUDIO_RETRY_DELAY);
                        let _ = errors.send(error);
                    }
                }
                configuration = Some(next_configuration);
            }
            Ok(StreamEvent::AudioPacket(packet)) => {
                if audio.is_none() && retry_at.is_none_or(|deadline| Instant::now() >= deadline) {
                    let Some(configuration) = &configuration else {
                        continue;
                    };
                    match configuration
                        .create_pipeline(telemetry.clone(), audio_muted.load(Ordering::Relaxed))
                    {
                        Ok(pipeline) => {
                            audio_metadata.update(pipeline.details());
                            audio = Some(pipeline);
                            retry_at = None;
                        }
                        Err(error) => {
                            retry_at = Some(Instant::now() + AUDIO_RETRY_DELAY);
                            let _ = errors
                                .send(format!("audio output restart failed; retrying: {error}"));
                        }
                    }
                }
                let Some(pipeline) = &mut audio else {
                    continue;
                };
                if let Err(error) = pipeline.push(packet) {
                    audio = None;
                    retry_at = Some(Instant::now() + AUDIO_RETRY_DELAY);
                    let _ = errors.send(format!("audio output stopped; restarting: {error}"));
                }
            }
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(error) = audio
            .as_ref()
            .and_then(|pipeline| pipeline_error(&pipeline.pipeline))
        {
            audio = None;
            retry_at = Some(Instant::now() + AUDIO_RETRY_DELAY);
            let _ = errors.send(format!("audio pipeline stopped; restarting: {error}"));
        }
        if let Some(pipeline) = &mut audio {
            pipeline.set_muted(audio_muted.load(Ordering::Relaxed));
        }
    }
}

#[derive(Clone)]
struct AudioConfiguration {
    sample_rate: i32,
    channels: i32,
    streams: i32,
    coupled_streams: i32,
    samples_per_frame: i32,
    mapping: Vec<u8>,
}

impl AudioConfiguration {
    fn create_pipeline(
        &self,
        telemetry: AudioTelemetry,
        muted: bool,
    ) -> Result<AudioPipeline, String> {
        AudioPipeline::new(self, telemetry, muted)
    }

    fn layout(&self) -> Result<AudioLayout, String> {
        let channels = usize::try_from(self.channels)
            .ok()
            .filter(|channels| matches!(channels, 2 | 6))
            .ok_or_else(|| {
                format!(
                    "host selected unsupported audio channel count: {}",
                    self.channels
                )
            })?;
        if self.mapping.len() != channels {
            return Err(format!(
                "host supplied {} Opus mapping entries for {channels} channels",
                self.mapping.len()
            ));
        }
        let streams = u16::try_from(self.streams)
            .ok()
            .filter(|streams| *streams > 0)
            .ok_or_else(|| "host supplied an invalid Opus stream count".to_owned())?;
        let coupled_streams = u16::try_from(self.coupled_streams)
            .ok()
            .filter(|coupled| *coupled <= streams)
            .ok_or_else(|| "host supplied an invalid Opus coupled-stream count".to_owned())?;
        let coded_channels = streams
            .checked_add(coupled_streams)
            .filter(|channels| u8::try_from(*channels).is_ok())
            .ok_or_else(|| "host supplied an invalid Opus coded-channel count".to_owned())?;
        if self
            .mapping
            .iter()
            .any(|channel| u16::from(*channel) >= coded_channels && *channel != u8::MAX)
        {
            return Err(format!(
                "host supplied an invalid Opus mapping: {:?}",
                self.mapping
            ));
        }

        match channels {
            2 if self.streams == 1
                && self.coupled_streams == 1
                && self.mapping.as_slice() == [0, 1] =>
            {
                Ok(AudioLayout::Stereo)
            }
            2 => Err(format!(
                "host selected unsupported stereo Opus layout: {} streams, {} coupled streams, mapping {:?}",
                self.streams, self.coupled_streams, self.mapping
            )),
            6 => Ok(AudioLayout::Surround51),
            _ => Err("unreachable audio channel count".to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioLayout {
    Stereo,
    Surround51,
}

impl AudioLayout {
    const fn label(self) -> &'static str {
        match self {
            Self::Stereo => "Stereo (2 channels)",
            Self::Surround51 => "5.1 surround (6 channels)",
        }
    }

    const fn mapping_family(self) -> i32 {
        match self {
            Self::Stereo => 0,
            Self::Surround51 => 1,
        }
    }

    const fn channel_mask(self) -> u64 {
        match self {
            Self::Stereo => 0x3,
            Self::Surround51 => 0x3F,
        }
    }

    const fn pipewire_channel_map(self) -> &'static str {
        match self {
            Self::Stereo => "FL,FR",
            Self::Surround51 => "FL,FR,FC,LFE,RL,RR",
        }
    }
}

struct AudioPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
    volume: gst::Element,
    timeline: AudioTimeline,
    muted: bool,
    player: Option<PipeWirePlayer>,
    encoded_diagnostic: Option<BufWriter<File>>,
    telemetry: AudioTelemetry,
    details: AudioPipelineDetails,
}

#[derive(Clone)]
struct AudioTelemetry {
    counters: Arc<AudioCounters>,
    origin: Instant,
}

struct AudioTimeline {
    frame_duration: gst::ClockTime,
    next_pts: Option<gst::ClockTime>,
}

const AUDIO_MAX_BUFFER_NS: u64 = 100_000_000;
const PIPEWIRE_AUDIO_LATENCY_MS: u64 = 100;
const AUDIO_RETRY_DELAY: Duration = Duration::from_millis(250);

impl AudioTimeline {
    fn new(sample_rate: i32, samples_per_frame: i32) -> Result<Self, String> {
        let sample_rate = u64::try_from(sample_rate)
            .ok()
            .filter(|rate| *rate > 0)
            .ok_or_else(|| "host supplied an invalid Opus sample rate".to_owned())?;
        let samples_per_frame = u64::try_from(samples_per_frame)
            .ok()
            .filter(|samples| *samples > 0)
            .ok_or_else(|| "host supplied an invalid Opus frame size".to_owned())?;
        let duration_ns = samples_per_frame
            .checked_mul(gst::ClockTime::SECOND.nseconds())
            .and_then(|nanoseconds| nanoseconds.checked_div(sample_rate))
            .filter(|nanoseconds| *nanoseconds > 0)
            .ok_or_else(|| "host supplied an invalid Opus frame duration".to_owned())?;
        Ok(Self {
            frame_duration: gst::ClockTime::from_nseconds(duration_ns),
            next_pts: None,
        })
    }

    fn next(&mut self, start: gst::ClockTime) -> (gst::ClockTime, gst::ClockTime) {
        let pts = self.next_pts.unwrap_or(start);
        self.next_pts = Some(pts.saturating_add(self.frame_duration));
        (pts, self.frame_duration)
    }
}

struct PipeWirePlayer {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl PipeWirePlayer {
    fn spawn(sample_rate: i32, channels: i32, layout: AudioLayout) -> Result<Self, String> {
        let mut child = Command::new("pw-play")
            .arg("--rate")
            .arg(sample_rate.to_string())
            .arg("--channels")
            .arg(channels.to_string())
            .args([
                "--format",
                "f32",
                "--channel-map",
                layout.pipewire_channel_map(),
            ])
            .arg("--latency")
            .arg(format!("{PIPEWIRE_AUDIO_LATENCY_MS}ms"))
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to start pw-play: {error}"))?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("pw-play did not expose its standard input".to_owned());
        };
        Ok(Self {
            child,
            stdin: Some(stdin),
        })
    }

    fn input_fd(&self) -> Result<i32, String> {
        self.stdin
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .ok_or_else(|| "pw-play input is closed".to_owned())
    }

    fn ensure_running(&mut self) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => Err(format!("pw-play exited unexpectedly with {status}")),
            Err(error) => Err(format!("failed to inspect pw-play: {error}")),
        }
    }
}

impl Drop for PipeWirePlayer {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[derive(Clone, Copy)]
enum AudioOutput {
    PipeWirePlayer(i32),
    PipeWireSink,
    PulseSink,
}

impl AudioOutput {
    const fn label(self) -> &'static str {
        match self {
            Self::PipeWirePlayer(_) => "PipeWire playback client",
            Self::PipeWireSink => "GStreamer PipeWire sink",
            Self::PulseSink => "GStreamer PulseAudio sink",
        }
    }
}

impl AudioPipeline {
    fn new(
        configuration: &AudioConfiguration,
        telemetry: AudioTelemetry,
        muted: bool,
    ) -> Result<Self, String> {
        let sample_rate = configuration.sample_rate;
        let channels = configuration.channels;
        let streams = configuration.streams;
        let coupled_streams = configuration.coupled_streams;
        let samples_per_frame = configuration.samples_per_frame;
        let mapping = &configuration.mapping;
        let layout = configuration.layout()?;
        let timeline = AudioTimeline::new(sample_rate, samples_per_frame)?;
        telemetry
            .counters
            .reset(timeline.frame_duration.nseconds() / 1_000);
        let player = PipeWirePlayer::spawn(sample_rate, channels, layout).ok();
        let output = if let Some(player) = &player {
            AudioOutput::PipeWirePlayer(player.input_fd()?)
        } else if gst::ElementFactory::find("pipewiresink").is_some() {
            AudioOutput::PipeWireSink
        } else {
            AudioOutput::PulseSink
        };
        let description = audio_pipeline_description(configuration, layout, output);
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| error.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "GStreamer did not construct an audio pipeline".to_owned())?;
        attach_audio_diagnostic(&pipeline)?;
        let source = pipeline
            .by_name("audio_src")
            .ok_or_else(|| "audio appsrc is missing".to_owned())?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| "audio source has the wrong GStreamer type".to_owned())?;
        let volume = pipeline
            .by_name("audio_volume")
            .ok_or_else(|| "audio volume control is missing".to_owned())?;
        volume.set_property("mute", muted);
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        let encoded_diagnostic = open_encoded_audio_diagnostic(
            sample_rate,
            channels,
            streams,
            coupled_streams,
            samples_per_frame,
            mapping,
        )?;
        Ok(Self {
            pipeline,
            source,
            volume,
            timeline,
            muted,
            player,
            encoded_diagnostic,
            telemetry,
            details: AudioPipelineDetails {
                layout: layout.label(),
                output: output.label(),
            },
        })
    }

    const fn details(&self) -> AudioPipelineDetails {
        self.details
    }

    fn push(&mut self, packet: Vec<u8>) -> Result<(), String> {
        if let Some(player) = &mut self.player {
            player.ensure_running()?;
        }
        if let Some(diagnostic) = &mut self.encoded_diagnostic {
            let length = u32::try_from(packet.len())
                .map_err(|_| "encoded Opus diagnostic packet is too large".to_owned())?;
            diagnostic
                .write_all(&length.to_le_bytes())
                .and_then(|()| diagnostic.write_all(&packet))
                .map_err(|error| format!("failed to write encoded Opus diagnostic: {error}"))?;
        }
        let start = self
            .pipeline
            .current_running_time()
            .unwrap_or(gst::ClockTime::ZERO);
        let (pts, duration) = self.timeline.next(start);
        if packet.is_empty() {
            if self.source.send_event(gst::event::Gap::new(pts, duration)) {
                self.telemetry
                    .counters
                    .record_output(self.telemetry.origin, duration.nseconds() / 1_000);
                return Ok(());
            }
            return Err("GStreamer rejected an Opus packet-loss gap".to_owned());
        }
        let mut buffer = gst::Buffer::from_mut_slice(packet);
        if let Some(buffer) = buffer.get_mut() {
            buffer.set_pts(pts);
            buffer.set_duration(duration);
        }
        self.source
            .push_buffer(buffer)
            .map(|_| {
                self.telemetry
                    .counters
                    .record_output(self.telemetry.origin, duration.nseconds() / 1_000);
            })
            .map_err(|error| error.to_string())
    }

    fn set_muted(&mut self, muted: bool) {
        if self.muted != muted {
            self.volume.set_property("mute", muted);
            self.muted = muted;
        }
    }
}

fn audio_pipeline_description(
    configuration: &AudioConfiguration,
    layout: AudioLayout,
    output: AudioOutput,
) -> String {
    // PipeWire's own playback client provides a bounded, clocked device queue and applies
    // backpressure through its stdin pipe. This avoids the long-running underruns observed when
    // GStreamer scheduled live network packets directly on pipewiresink. Keep native PipeWire and
    // clocked Pulse as dependency fallbacks for broader beta diagnostics.
    let sink = match output {
        AudioOutput::PipeWirePlayer(fd) => {
            format!("fdsink fd={fd} sync=false async=false")
        }
        AudioOutput::PipeWireSink => "pipewiresink sync=true".to_owned(),
        AudioOutput::PulseSink => {
            "pulsesink sync=true buffer-time=15000 latency-time=3750".to_owned()
        }
    };
    let mapping = configuration
        .mapping
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let opus_caps = if layout.mapping_family() == 0 {
        format!(
            "audio/x-opus,rate={},channels={},channel-mapping-family=0",
            configuration.sample_rate, configuration.channels
        )
    } else {
        format!(
            "audio/x-opus,rate={},channels={},channel-mapping-family=1,\
             stream-count={},coupled-count={},channel-mapping=(int)<{}>",
            configuration.sample_rate,
            configuration.channels,
            configuration.streams,
            configuration.coupled_streams,
            mapping
        )
    };
    format!(
        "appsrc name=audio_src is-live=true format=time do-timestamp=false \
         block=true max-time={AUDIO_MAX_BUFFER_NS} \
         caps={opus_caps} ! opusparse ! \
         opusdec plc=true use-inband-fec=true ! audioconvert ! \
         volume name=audio_volume ! \
         audioresample ! \
         audio/x-raw,format=F32LE,rate={},channels={},layout=interleaved,\
         channel-mask=(bitmask)0x{:016x} ! \
         tee name=audio_tee \
         audio_tee. ! queue max-size-buffers=0 max-size-bytes=0 \
         max-size-time={AUDIO_MAX_BUFFER_NS} ! {sink}",
        configuration.sample_rate,
        configuration.channels,
        layout.channel_mask(),
    )
}

fn open_encoded_audio_diagnostic(
    sample_rate: i32,
    channels: i32,
    streams: i32,
    coupled_streams: i32,
    samples_per_frame: i32,
    mapping: &[u8],
) -> Result<Option<BufWriter<File>>, String> {
    let Ok(location) = std::env::var("ARTEMIS_AUDIO_DIAGNOSTIC_OPUS") else {
        return Ok(None);
    };
    if location.trim().is_empty() {
        return Ok(None);
    }

    let file = File::create(&location)
        .map_err(|error| format!("failed to create encoded Opus diagnostic: {error}"))?;
    let mut writer = BufWriter::new(file);
    let mapping_length = u32::try_from(mapping.len())
        .map_err(|_| "Opus diagnostic channel mapping is too large".to_owned())?;
    writer
        .write_all(b"AML_OPUS")
        .and_then(|()| writer.write_all(&1_u32.to_le_bytes()))
        .and_then(|()| writer.write_all(&sample_rate.to_le_bytes()))
        .and_then(|()| writer.write_all(&channels.to_le_bytes()))
        .and_then(|()| writer.write_all(&streams.to_le_bytes()))
        .and_then(|()| writer.write_all(&coupled_streams.to_le_bytes()))
        .and_then(|()| writer.write_all(&samples_per_frame.to_le_bytes()))
        .and_then(|()| writer.write_all(&mapping_length.to_le_bytes()))
        .and_then(|()| writer.write_all(mapping))
        .map_err(|error| format!("failed to initialize encoded Opus diagnostic: {error}"))?;
    Ok(Some(writer))
}

fn attach_audio_diagnostic(pipeline: &gst::Pipeline) -> Result<(), String> {
    let Ok(location) = std::env::var("ARTEMIS_AUDIO_DIAGNOSTIC_PCM") else {
        return Ok(());
    };
    if location.trim().is_empty() {
        return Ok(());
    }

    let tee = pipeline
        .by_name("audio_tee")
        .ok_or_else(|| "audio diagnostic tee is missing".to_owned())?;
    let queue = gst::ElementFactory::make("queue")
        .name("audio_diagnostic_queue")
        .build()
        .map_err(|error| error.to_string())?;
    let sink = gst::ElementFactory::make("filesink")
        .name("audio_diagnostic_sink")
        .property("location", &location)
        .property("sync", false)
        .property("async", false)
        .build()
        .map_err(|error| error.to_string())?;
    pipeline
        .add_many([&queue, &sink])
        .map_err(|error| error.to_string())?;
    gst::Element::link_many([&tee, &queue, &sink]).map_err(|error| error.to_string())
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        let _ = self.source.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
        self.player.take();
        if let Some(diagnostic) = &mut self.encoded_diagnostic {
            let _ = diagnostic.flush();
        }
    }
}

fn pipeline_error(pipeline: &gst::Pipeline) -> Option<String> {
    let bus = pipeline.bus()?;
    while let Some(message) = bus.pop() {
        if let gst::MessageView::Error(error) = message.view() {
            return Some(format!(
                "{}: {} ({:?})",
                error.src().map_or_else(
                    || "GStreamer".to_owned(),
                    |source| source.path_string().to_string()
                ),
                error.error(),
                error.debug()
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        AudioConfiguration, AudioLayout, AudioOutput, AudioStats, AudioTimeline, PipeWirePlayer,
        PlaybackClock, VA_AV1_DECODER, VA_H264_DECODER, VA_HEVC_DECODER, audio_clock,
        audio_pipeline_description, edid_supports_hdr10, encoded_video_caps, megabits_per_second,
        video_packet_issues, video_pipeline_description,
    };
    use artemis_moonlight::{
        HdrMetadata, NetworkStats, VideoBitDepth, VideoCodec, VideoColorInfo, VideoColorSpace,
    };
    use gstreamer as gst;
    use gstreamer_video as gst_video;

    fn stereo_audio_configuration() -> AudioConfiguration {
        AudioConfiguration {
            sample_rate: 48_000,
            channels: 2,
            streams: 1,
            coupled_streams: 1,
            samples_per_frame: 240,
            mapping: vec![0, 1],
        }
    }

    fn surround_audio_configuration() -> AudioConfiguration {
        AudioConfiguration {
            sample_rate: 48_000,
            channels: 6,
            streams: 4,
            coupled_streams: 2,
            samples_per_frame: 240,
            mapping: vec![0, 4, 1, 2, 3, 5],
        }
    }

    #[test]
    fn moonlight_audio_layouts_validate_before_pipeline_creation() {
        assert_eq!(
            stereo_audio_configuration().layout(),
            Ok(AudioLayout::Stereo)
        );
        assert_eq!(
            surround_audio_configuration().layout(),
            Ok(AudioLayout::Surround51)
        );

        let mut invalid = surround_audio_configuration();
        invalid.mapping[5] = 9;
        assert!(invalid.layout().is_err());
    }

    #[test]
    fn opus_timeline_preserves_five_millisecond_cadence_for_batched_packets() {
        let mut timeline = AudioTimeline::new(48_000, 240).expect("valid Opus timing");
        let start = gst::ClockTime::from_mseconds(500);

        let timestamps = (0..4).map(|_| timeline.next(start).0).collect::<Vec<_>>();

        assert_eq!(
            timestamps,
            [
                gst::ClockTime::from_mseconds(500),
                gst::ClockTime::from_mseconds(505),
                gst::ClockTime::from_mseconds(510),
                gst::ClockTime::from_mseconds(515),
            ]
        );
    }

    #[test]
    fn opus_timeline_rejects_invalid_configuration() {
        assert!(AudioTimeline::new(0, 240).is_err());
        assert!(AudioTimeline::new(48_000, 0).is_err());
    }

    #[test]
    fn playback_clock_reports_media_drift_against_wall_time() {
        let clock = PlaybackClock::new(10_050_000, Duration::from_secs(10));

        assert_eq!(clock.media_elapsed, 10_050);
        assert_eq!(clock.wall_elapsed, 10_000);
        assert_eq!(clock.drift, 50);
    }

    #[test]
    fn audio_clock_excludes_the_first_packet_duration_from_elapsed_time() {
        let clock = audio_clock(AudioStats {
            packets: 201,
            media_us: 1_005_000,
            frame_duration_us: 5_000,
            first_output_elapsed_us: 1,
            last_output_elapsed_us: 1_000_001,
        })
        .expect("enough audio samples");

        assert_eq!(clock.media_elapsed, 1_000);
        assert_eq!(clock.wall_elapsed, 1_000);
        assert_eq!(clock.drift, 0);
    }

    #[test]
    fn encoded_video_bandwidth_uses_decimal_megabits() {
        assert!((megabits_per_second(12_500_000, 0, 1.0) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn video_packet_issues_exclude_successfully_recovered_fec() {
        let stats = NetworkStats {
            video_fec_recovered: 12,
            video_fec_failed: 3,
            video_out_of_sequence: 2,
            video_invalid: 1,
            ..NetworkStats::default()
        };

        assert_eq!(video_packet_issues(&stats), 6);
    }

    #[test]
    fn pipewire_player_reports_an_exited_child() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 17"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn test child");
        let stdin = child.stdin.take().expect("test child stdin");
        let mut player = PipeWirePlayer {
            child,
            stdin: Some(stdin),
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let error = loop {
            match player.ensure_running() {
                Ok(()) if Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
                Ok(()) => panic!("test child did not exit"),
                Err(error) => break error,
            }
        };
        assert!(error.contains("pw-play exited unexpectedly"));
        assert!(error.contains("17"));
    }

    #[test]
    fn pipewire_player_owns_audio_pacing_with_a_bounded_input() {
        let configuration = stereo_audio_configuration();
        let description = audio_pipeline_description(
            &configuration,
            AudioLayout::Stereo,
            AudioOutput::PipeWirePlayer(17),
        );

        assert!(description.contains("audio/x-raw,format=F32LE,rate=48000,channels=2"));
        assert!(description.contains("channel-mapping-family=0"));
        assert!(description.contains("block=true max-time=100000000"));
        assert!(description.contains("volume name=audio_volume"));
        assert!(description.contains(
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=100000000 ! \
             fdsink fd=17 sync=false async=false"
        ));
        assert!(!description.contains("min-threshold-time"));
        assert!(!description.contains("pulsesink"));
        assert!(!description.contains("pipewiresink"));
    }

    #[test]
    fn native_pipewire_sink_is_retained_as_a_player_fallback() {
        let configuration = stereo_audio_configuration();
        let description = audio_pipeline_description(
            &configuration,
            AudioLayout::Stereo,
            AudioOutput::PipeWireSink,
        );

        assert!(description.contains(
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=100000000 ! \
             pipewiresink sync=true"
        ));
        assert!(!description.contains("fdsink"));
    }

    #[test]
    fn clocked_pulse_sink_is_retained_as_a_plugin_fallback() {
        let configuration = stereo_audio_configuration();
        let description =
            audio_pipeline_description(&configuration, AudioLayout::Stereo, AudioOutput::PulseSink);

        assert!(description.contains(
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=100000000 ! \
             pulsesink sync=true buffer-time=15000 latency-time=3750"
        ));
        assert!(!description.contains("min-threshold-time"));
        assert!(!description.contains("pipewiresink"));
    }

    #[test]
    fn surround_pipeline_preserves_moonlight_opus_mapping_and_channel_positions() {
        gst::init().expect("initialize GStreamer");
        let configuration = surround_audio_configuration();
        let description = audio_pipeline_description(
            &configuration,
            AudioLayout::Surround51,
            AudioOutput::PipeWirePlayer(17),
        );

        assert!(description.contains("channel-mapping-family=1"));
        assert!(description.contains("stream-count=4,coupled-count=2"));
        assert!(description.contains("channel-mapping=(int)<0,4,1,2,3,5>"));
        assert!(description.contains("channel-mask=(bitmask)0x000000000000003f"));
        assert_eq!(
            AudioLayout::Surround51.pipewire_channel_map(),
            "FL,FR,FC,LFE,RL,RR"
        );
        gst::parse::launch(&description).expect("construct the 5.1 Opus pipeline");
    }

    #[test]
    fn video_decoders_match_each_negotiated_codec() {
        assert_eq!(VA_H264_DECODER.element, "vah264dec");
        assert_eq!(VA_HEVC_DECODER.element, "vah265dec");
        assert_eq!(VA_AV1_DECODER.element, "vaav1dec");
    }

    #[test]
    fn video_pipeline_preserves_native_stream_resolution_without_a_frame_rate_cap() {
        let description = video_pipeline_description(
            VideoCodec::H264,
            VideoBitDepth::Eight,
            VA_H264_DECODER,
            3840,
            2160,
            false,
            false,
        );

        assert!(description.contains("video/x-raw,format=NV12,colorimetry="));
        assert!(description.contains("width=3840,height=2160"));
        assert!(!description.contains("videorate"));
        assert!(!description.contains("max-rate=30"));
    }

    #[test]
    fn video_pipeline_keeps_va_frames_on_the_gpu_when_egl_interop_is_available() {
        let description = video_pipeline_description(
            VideoCodec::Av1,
            VideoBitDepth::Eight,
            VA_AV1_DECODER,
            3840,
            2160,
            true,
            false,
        );

        assert!(description.contains("video/x-av1,stream-format=obu-stream,alignment=frame"));
        assert!(description.contains("vaav1dec"));
        assert!(description.contains("video/x-raw(memory:DMABuf),format=DMA_DRM"));
        assert!(description.contains("glupload ! glcolorconvert name=gl_convert"));
        assert!(description.contains("video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D"));
        assert!(!description.contains("video/x-raw,format=NV12"));
    }

    #[test]
    fn hevc_pipeline_uses_byte_stream_access_units() {
        let description = video_pipeline_description(
            VideoCodec::Hevc,
            VideoBitDepth::Eight,
            VA_HEVC_DECODER,
            2560,
            1440,
            false,
            false,
        );

        assert!(description.contains("h265parse config-interval=-1"));
        assert!(description.contains("vah265dec"));
    }

    #[test]
    fn software_av1_pipeline_converts_dav1d_output_to_nv12() {
        let decoder = super::AV1_DECODERS[1];
        let description = video_pipeline_description(
            VideoCodec::Av1,
            VideoBitDepth::Eight,
            decoder,
            1920,
            1080,
            false,
            false,
        );

        assert!(description.contains("video/x-av1,stream-format=obu-stream,alignment=tu"));
        assert!(description.contains("av1dec ! videoconvert"));
        assert!(description.contains("video/x-raw,format=NV12"));
    }

    #[test]
    fn main10_pipeline_preserves_ten_bits_through_gpu_presentation() {
        gst::init().expect("initialize GStreamer");
        let description = video_pipeline_description(
            VideoCodec::Hevc,
            VideoBitDepth::Ten,
            VA_HEVC_DECODER,
            3840,
            2160,
            true,
            true,
        );

        assert!(description.contains("vah265dec"));
        assert!(description.contains("video/x-raw(memory:DMABuf),format=DMA_DRM"));
        assert!(
            description
                .contains("video/x-raw(memory:GLMemory),format=RGB10A2_LE,texture-target=2D")
        );
        assert!(!description.contains("format=RGBA"));
        assert!(description.contains("colorimetry=bt2100-pq"));
        assert!(description.contains("sync=true qos=true max-lateness=20000000"));
        if gst::ElementFactory::find(VA_HEVC_DECODER.element).is_some() {
            gst::parse::launch(&description).expect("construct the Main10 HEVC pipeline");
        }
    }

    #[test]
    fn hdr_encoded_caps_include_pq_bt2020_and_sunshine_metadata() {
        gst::init().expect("initialize GStreamer");
        let caps = encoded_video_caps(
            VideoCodec::Av1,
            VideoColorInfo {
                hdr_active: true,
                color_space: VideoColorSpace::Rec2020,
                hdr_metadata: Some(HdrMetadata {
                    display_primaries_x: [35_400, 8_500, 6_550],
                    display_primaries_y: [14_600, 39_850, 2_300],
                    white_point_x: 15_635,
                    white_point_y: 16_450,
                    max_display_luminance: 1_000,
                    min_display_luminance: 50,
                    max_content_light_level: 1_000,
                    max_frame_average_light_level: 400,
                    max_full_frame_luminance: 600,
                }),
            },
        );

        let structure = caps.structure(0).expect("AV1 caps structure");
        assert_eq!(
            structure.get::<String>("colorimetry").expect("colorimetry"),
            "bt2100-pq"
        );
        let mastering = gst_video::VideoMasteringDisplayInfo::from_caps(&caps)
            .expect("mastering display metadata");
        assert_eq!(mastering.max_display_mastering_luminance(), 10_000_000);
        assert_eq!(mastering.min_display_mastering_luminance(), 50);
        let content =
            gst_video::VideoContentLightLevel::from_caps(&caps).expect("content light metadata");
        assert_eq!(content.max_content_light_level(), 1_000);
        assert_eq!(content.max_frame_average_light_level(), 400);
    }

    #[test]
    fn edid_hdr_probe_requires_the_pq_eotf_bit() {
        let mut edid = vec![0_u8; 256];
        edid[128] = 0x02;
        edid[130] = 8;
        edid[132] = (0x07 << 5) | 3;
        edid[133] = 0x06;
        edid[134] = 0x04;
        edid[135] = 0x01;
        assert!(edid_supports_hdr10(&edid));

        edid[134] = 0x01;
        assert!(!edid_supports_hdr10(&edid));
    }
}
