use std::ffi::c_void;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
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
    AudioEventReceiver, NetworkStats, Session, StreamEvent, VideoEventReceiver,
};

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub(crate) sample: gst::Sample,
    pub(crate) gl_context: Option<gst_gl::GLContext>,
    pub presentation_time_us: u64,
}

#[derive(Clone)]
pub struct GlInteropContext {
    display: gst_gl::GLDisplay,
    context: gst_gl::GLContext,
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
        type EglGetCurrentDisplay = unsafe extern "C" fn() -> *mut c_void;
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
        }))
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
    audio_counters: Arc<AudioCounters>,
    first_presented_at: Option<Instant>,
    last_presented_at: Option<Instant>,
    first_presented_pts_us: Option<u64>,
    last_presented_pts_us: Option<u64>,
    last_video_report_at: Instant,
    last_video_report: VideoStats,
    last_ingress_report: artemis_moonlight::MediaIngressStats,
    last_network_report: NetworkStats,
}

impl MediaRuntime {
    pub fn new(
        audio_events: AudioEventReceiver,
        video_events: VideoEventReceiver,
        gl_interop: Option<GlInteropContext>,
    ) -> Result<Self, String> {
        gst::init().map_err(|error| error.to_string())?;
        let clock_origin = Instant::now();
        let (frame_sender, frames) = bounded(2);
        let video_counters = Arc::new(VideoCounters::default());
        let audio_counters = Arc::new(AudioCounters::default());
        let audio = AudioWorker::spawn(audio_events, Arc::clone(&audio_counters), clock_origin)?;
        let video = VideoWorker::spawn(
            video_events,
            frame_sender,
            Arc::clone(&video_counters),
            gl_interop,
        )?;
        Ok(Self {
            video,
            audio,
            frames,
            video_counters,
            audio_counters,
            first_presented_at: None,
            last_presented_at: None,
            first_presented_pts_us: None,
            last_presented_pts_us: None,
            last_video_report_at: Instant::now(),
            last_video_report: VideoStats::default(),
            last_ingress_report: artemis_moonlight::MediaIngressStats::default(),
            last_network_report: NetworkStats::default(),
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
        let video_clock = self.video_clock();
        let audio_clock = audio_clock(self.audio_counters.snapshot());
        tracing::info!(
            target: "artemis::media",
            video_ingress_fps,
            submitted_fps,
            decoded_fps,
            presented_fps,
            decoder_queue_dropped = dropped,
            callback_queue_dropped,
            audio_ingress_pps,
            video_network_pps,
            audio_network_pps,
            video_media_elapsed_ms = ?video_clock.map(|clock| clock.media_elapsed),
            video_wall_elapsed_ms = ?video_clock.map(|clock| clock.wall_elapsed),
            video_clock_drift_ms = ?video_clock.map(|clock| clock.drift),
            audio_media_elapsed_ms = ?audio_clock.map(|clock| clock.media_elapsed),
            audio_wall_elapsed_ms = ?audio_clock.map(|clock| clock.wall_elapsed),
            audio_clock_drift_ms = ?audio_clock.map(|clock| clock.drift),
            audio_output_latency_ms = PIPEWIRE_AUDIO_LATENCY_MS,
            "media pipeline telemetry"
        );
        self.last_video_report = current;
        self.last_ingress_report = ingress;
        self.last_network_report = network;
        self.last_video_report_at = now;
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
        gl_interop: Option<GlInteropContext>,
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
                    gl_interop,
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

fn run_video_worker(
    events: &VideoEventReceiver,
    stop: &Receiver<()>,
    errors: &Sender<String>,
    frames: &crossbeam_channel::Sender<DecodedFrame>,
    video_counters: &Arc<VideoCounters>,
    gl_interop: Option<GlInteropContext>,
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
                fps,
            }) => {
                let result = if format.trailing_zeros() >= 4 {
                    Err(format!(
                        "host selected unsupported video format 0x{format:x}"
                    ))
                } else {
                    VideoPipeline::new(
                        width,
                        height,
                        fps,
                        frames.clone(),
                        Arc::clone(video_counters),
                        gl_interop.clone(),
                    )
                };
                match result {
                    Ok(pipeline) => video = Some(pipeline),
                    Err(error) => {
                        video = None;
                        let _ = errors.send(error);
                    }
                }
            }
            Ok(StreamEvent::VideoFrame {
                bytes,
                key_frame,
                presentation_time_us,
            }) => {
                let Some(pipeline) = &video else {
                    continue;
                };
                if let Err(error) = pipeline.push(bytes, key_frame, presentation_time_us) {
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
            video = None;
            let _ = errors.send(error);
        }
    }
}

struct VideoPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
    video_counters: Arc<VideoCounters>,
}

impl VideoPipeline {
    fn new(
        width: i32,
        height: i32,
        _fps: i32,
        frames: crossbeam_channel::Sender<DecodedFrame>,
        video_counters: Arc<VideoCounters>,
        gl_interop: Option<GlInteropContext>,
    ) -> Result<Self, String> {
        let has_va_decoder = gst::ElementFactory::find("vah264dec").is_some();
        let use_gl_interop = has_va_decoder && gl_interop.is_some();
        let description = video_pipeline_description(width, height, has_va_decoder, use_gl_interop);
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| error.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "GStreamer did not construct a video pipeline".to_owned())?;
        if let Some(gl_interop) = &gl_interop {
            gl_interop.configure_pipeline(&pipeline)?;
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
        let frame_gl_context = gl_interop.map(|interop| interop.context);
        let callback_counters = Arc::clone(&video_counters);
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    callback_counters.decoded.fetch_add(1, Ordering::Relaxed);
                    if frames.is_full() {
                        callback_counters.dropped.fetch_add(1, Ordering::Relaxed);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                    let info = gst_video::VideoInfo::from_caps(caps)
                        .map_err(|_| gst::FlowError::NotNegotiated)?;
                    if !matches!(
                        info.format(),
                        gst_video::VideoFormat::Nv12 | gst_video::VideoFormat::Rgba
                    ) {
                        return Err(gst::FlowError::NotNegotiated);
                    }
                    let width = usize::try_from(info.width()).map_err(|_| gst::FlowError::Error)?;
                    let height =
                        usize::try_from(info.height()).map_err(|_| gst::FlowError::Error)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let presentation_time_us = buffer.pts().map_or(0, gst::ClockTime::useconds);
                    if info.format() == gst_video::VideoFormat::Rgba {
                        set_gl_sync_point(buffer, gl_producer.as_ref());
                    }
                    let frame = DecodedFrame {
                        width,
                        height,
                        sample,
                        gl_context: frame_gl_context.clone(),
                        presentation_time_us,
                    };
                    let _ = frames.try_send(frame);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            pipeline,
            source,
            video_counters,
        })
    }

    fn push(
        &self,
        bytes: Vec<u8>,
        key_frame: bool,
        presentation_time_us: u64,
    ) -> Result<(), String> {
        self.video_counters
            .submitted
            .fetch_add(1, Ordering::Relaxed);
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
            .map_err(|error| error.to_string())
    }
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

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        let _ = self.source.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn video_decoder_chain(hardware_available: bool) -> &'static str {
    if hardware_available {
        "vah264dec"
    } else {
        "avdec_h264"
    }
}

fn video_pipeline_description(
    width: i32,
    height: i32,
    hardware_available: bool,
    gl_interop_available: bool,
) -> String {
    let decoder_chain = video_decoder_chain(hardware_available);
    if hardware_available && gl_interop_available {
        return format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=false \
             caps=video/x-h264,stream-format=byte-stream,alignment=au ! \
             h264parse config-interval=-1 ! {decoder_chain} ! \
             video/x-raw(memory:DMABuf),format=DMA_DRM,width={width},height={height} ! \
             glupload ! glcolorconvert name=gl_convert ! \
             video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D,\
             width={width},height={height} ! \
             appsink name=video_sink max-buffers=2 drop=true sync=false"
        );
    }
    format!(
        "appsrc name=video_src is-live=true format=time do-timestamp=false \
         caps=video/x-h264,stream-format=byte-stream,alignment=au ! \
         h264parse config-interval=-1 ! {decoder_chain} ! \
         video/x-raw,format=NV12,width={width},height={height} ! \
         appsink name=video_sink max-buffers=2 drop=true sync=false"
    )
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
    clock_origin: Instant,
) {
    let mut audio = None;
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
                match AudioPipeline::new(
                    sample_rate,
                    channels,
                    streams,
                    coupled_streams,
                    samples_per_frame,
                    &mapping,
                    AudioTelemetry {
                        counters: Arc::clone(audio_counters),
                        origin: clock_origin,
                    },
                ) {
                    Ok(pipeline) => audio = Some(pipeline),
                    Err(error) => {
                        audio = None;
                        let _ = errors.send(error);
                    }
                }
            }
            Ok(StreamEvent::AudioPacket(packet)) => {
                let Some(pipeline) = &mut audio else {
                    continue;
                };
                if let Err(error) = pipeline.push(packet) {
                    audio = None;
                    let _ = errors.send(error);
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
            let _ = errors.send(error);
        }
    }
}

struct AudioPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
    timeline: AudioTimeline,
    player: Option<PipeWirePlayer>,
    encoded_diagnostic: Option<BufWriter<File>>,
    telemetry: AudioTelemetry,
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
    fn spawn(sample_rate: i32, channels: i32) -> Result<Self, String> {
        let mut child = Command::new("pw-play")
            .arg("--rate")
            .arg(sample_rate.to_string())
            .arg("--channels")
            .arg(channels.to_string())
            .args(["--format", "f32", "--channel-map", "stereo"])
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

impl AudioPipeline {
    fn new(
        sample_rate: i32,
        channels: i32,
        streams: i32,
        coupled_streams: i32,
        samples_per_frame: i32,
        mapping: &[u8],
        telemetry: AudioTelemetry,
    ) -> Result<Self, String> {
        if channels != 2 || streams != 1 || coupled_streams != 1 || mapping != [0, 1] {
            return Err(format!(
                "host selected unsupported Opus layout: {channels} channels, \
                 {streams} streams, {coupled_streams} coupled streams, mapping {mapping:?}"
            ));
        }
        let timeline = AudioTimeline::new(sample_rate, samples_per_frame)?;
        telemetry
            .counters
            .reset(timeline.frame_duration.nseconds() / 1_000);
        let player = PipeWirePlayer::spawn(sample_rate, channels).ok();
        let output = if let Some(player) = &player {
            AudioOutput::PipeWirePlayer(player.input_fd()?)
        } else if gst::ElementFactory::find("pipewiresink").is_some() {
            AudioOutput::PipeWireSink
        } else {
            AudioOutput::PulseSink
        };
        let description = audio_pipeline_description(sample_rate, channels, output);
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
            timeline,
            player,
            encoded_diagnostic,
            telemetry,
        })
    }

    fn push(&mut self, packet: Vec<u8>) -> Result<(), String> {
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
}

fn audio_pipeline_description(sample_rate: i32, channels: i32, output: AudioOutput) -> String {
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
    format!(
        "appsrc name=audio_src is-live=true format=time do-timestamp=false \
         block=true max-time={AUDIO_MAX_BUFFER_NS} \
         caps=audio/x-opus,rate={sample_rate},channels={channels},\
         channel-mapping-family=0 ! opusparse ! \
         opusdec plc=true use-inband-fec=true ! audioconvert ! \
         audioresample ! \
         audio/x-raw,format=F32LE,rate={sample_rate},channels={channels} ! \
         tee name=audio_tee \
         audio_tee. ! queue max-size-buffers=0 max-size-bytes=0 \
         max-size-time={AUDIO_MAX_BUFFER_NS} ! {sink}"
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
    use std::time::Duration;

    use super::{
        AudioOutput, AudioStats, AudioTimeline, PlaybackClock, audio_clock,
        audio_pipeline_description, video_decoder_chain, video_pipeline_description,
    };
    use gstreamer as gst;

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
    fn pipewire_player_owns_audio_pacing_with_a_bounded_input() {
        let description = audio_pipeline_description(48_000, 2, AudioOutput::PipeWirePlayer(17));

        assert!(description.contains("audio/x-raw,format=F32LE,rate=48000,channels=2"));
        assert!(description.contains("block=true max-time=100000000"));
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
        let description = audio_pipeline_description(48_000, 2, AudioOutput::PipeWireSink);

        assert!(description.contains(
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=100000000 ! \
             pipewiresink sync=true"
        ));
        assert!(!description.contains("fdsink"));
    }

    #[test]
    fn clocked_pulse_sink_is_retained_as_a_plugin_fallback() {
        let description = audio_pipeline_description(48_000, 2, AudioOutput::PulseSink);

        assert!(description.contains(
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=100000000 ! \
             pulsesink sync=true buffer-time=15000 latency-time=3750"
        ));
        assert!(!description.contains("min-threshold-time"));
        assert!(!description.contains("pipewiresink"));
    }

    #[test]
    fn video_decoder_prefers_va_api_when_available() {
        assert_eq!(video_decoder_chain(true), "vah264dec");
        assert_eq!(video_decoder_chain(false), "avdec_h264");
    }

    #[test]
    fn video_pipeline_preserves_native_stream_resolution_without_a_frame_rate_cap() {
        let description = video_pipeline_description(3840, 2160, true, false);

        assert!(description.contains("video/x-raw,format=NV12,width=3840,height=2160"));
        assert!(!description.contains("videorate"));
        assert!(!description.contains("max-rate=30"));
    }

    #[test]
    fn video_pipeline_keeps_va_frames_on_the_gpu_when_egl_interop_is_available() {
        let description = video_pipeline_description(3840, 2160, true, true);

        assert!(description.contains("video/x-raw(memory:DMABuf),format=DMA_DRM"));
        assert!(description.contains("glupload ! glcolorconvert name=gl_convert"));
        assert!(description.contains("video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D"));
        assert!(!description.contains("video/x-raw,format=NV12"));
    }
}
