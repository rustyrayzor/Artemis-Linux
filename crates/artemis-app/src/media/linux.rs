use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use artemis_moonlight::{AudioEventReceiver, StreamEvent};

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub struct MediaRuntime {
    video: Option<VideoPipeline>,
    audio: AudioWorker,
    frames: Receiver<DecodedFrame>,
    frame_sender: crossbeam_channel::Sender<DecodedFrame>,
}

impl MediaRuntime {
    pub fn new(audio_events: AudioEventReceiver) -> Result<Self, String> {
        gst::init().map_err(|error| error.to_string())?;
        let (frame_sender, frames) = bounded(2);
        Ok(Self {
            video: None,
            audio: AudioWorker::spawn(audio_events)?,
            frames,
            frame_sender,
        })
    }

    pub fn handle(&mut self, event: StreamEvent) -> Result<(), String> {
        match event {
            StreamEvent::VideoSetup {
                format,
                width,
                height,
                fps,
            } => {
                if format.trailing_zeros() >= 4 {
                    return Err(format!(
                        "host selected unsupported video format 0x{format:x}"
                    ));
                }
                self.video = Some(VideoPipeline::new(
                    width,
                    height,
                    fps,
                    self.frame_sender.clone(),
                )?);
            }
            StreamEvent::VideoFrame {
                bytes,
                key_frame,
                presentation_time_us,
            } => {
                if let Some(video) = &self.video {
                    video.push(bytes, key_frame, presentation_time_us)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn try_frame(&self) -> Option<DecodedFrame> {
        let mut latest = None;
        while let Ok(frame) = self.frames.try_recv() {
            latest = Some(frame);
        }
        latest
    }

    pub fn poll_error(&self) -> Option<String> {
        self.video
            .as_ref()
            .and_then(|video| pipeline_error(&video.pipeline))
            .or_else(|| self.audio.poll_error())
    }

    pub fn shutdown(&mut self) {
        self.video.take();
        self.audio.shutdown();
    }
}

struct VideoPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
}

impl VideoPipeline {
    fn new(
        width: i32,
        height: i32,
        _fps: i32,
        frames: crossbeam_channel::Sender<DecodedFrame>,
    ) -> Result<Self, String> {
        let has_va_decoder = gst::ElementFactory::find("vah264dec").is_some()
            && gst::ElementFactory::find("vapostproc").is_some();
        let decoder_chain = video_decoder_chain(has_va_decoder);
        let (presentation_width, presentation_height) = presentation_size(width, height);
        let description = format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=false \
             caps=video/x-h264,stream-format=byte-stream,alignment=au ! \
             h264parse config-interval=-1 ! {decoder_chain} ! \
             video/x-raw,format=RGBA,width={presentation_width},height={presentation_height} ! \
             appsink name=video_sink max-buffers=2 drop=true sync=false"
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| error.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "GStreamer did not construct a video pipeline".to_owned())?;
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
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                    let structure = caps.structure(0).ok_or(gst::FlowError::NotNegotiated)?;
                    let width = structure
                        .get::<i32>("width")
                        .map_err(|_| gst::FlowError::NotNegotiated)?;
                    let height = structure
                        .get::<i32>("height")
                        .map_err(|_| gst::FlowError::NotNegotiated)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let frame = DecodedFrame {
                        width: usize::try_from(width).map_err(|_| gst::FlowError::Error)?,
                        height: usize::try_from(height).map_err(|_| gst::FlowError::Error)?,
                        rgba: map.as_slice().to_vec(),
                    };
                    let _ = frames.try_send(frame);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        Ok(Self { pipeline, source })
    }

    fn push(
        &self,
        bytes: Vec<u8>,
        key_frame: bool,
        presentation_time_us: u64,
    ) -> Result<(), String> {
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

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        let _ = self.source.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn video_decoder_chain(hardware_available: bool) -> &'static str {
    if hardware_available {
        "vah264dec ! videorate drop-only=true max-rate=30 ! vapostproc"
    } else {
        "avdec_h264 ! videorate drop-only=true max-rate=30 ! videoconvert ! videoscale"
    }
}

fn presentation_size(width: i32, height: i32) -> (i32, i32) {
    const MAX_PRESENTATION_WIDTH: i32 = 1280;

    if width <= MAX_PRESENTATION_WIDTH || width <= 0 || height <= 0 {
        return (width, height);
    }
    let scaled_height = i64::from(height) * i64::from(MAX_PRESENTATION_WIDTH) / i64::from(width);
    let scaled_height = i32::try_from(scaled_height).unwrap_or(height).max(2) & !1;
    (MAX_PRESENTATION_WIDTH, scaled_height)
}

struct AudioWorker {
    stop: Sender<()>,
    errors: Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl AudioWorker {
    fn spawn(events: AudioEventReceiver) -> Result<Self, String> {
        let (stop, stop_receiver) = bounded(1);
        let (error_sender, errors) = unbounded();
        let thread = thread::Builder::new()
            .name("artemis-audio".to_owned())
            .spawn(move || run_audio_worker(&events, &stop_receiver, &error_sender))
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

fn run_audio_worker(events: &AudioEventReceiver, stop: &Receiver<()>, errors: &Sender<String>) {
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
}

struct AudioTimeline {
    frame_duration: gst::ClockTime,
    next_pts: Option<gst::ClockTime>,
}

const AUDIO_MAX_BUFFER_NS: u64 = 100_000_000;

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
            .args([
                "--format",
                "f32",
                "--channel-map",
                "stereo",
                "--latency",
                "100ms",
                "-",
            ])
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
            .map(|stdin| stdin.as_raw_fd())
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
    ) -> Result<Self, String> {
        if channels != 2 || streams != 1 || coupled_streams != 1 || mapping != [0, 1] {
            return Err(format!(
                "host selected unsupported Opus layout: {channels} channels, \
                 {streams} streams, {coupled_streams} coupled streams, mapping {mapping:?}"
            ));
        }
        let timeline = AudioTimeline::new(sample_rate, samples_per_frame)?;
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
            .map(|_| ())
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
    use super::{
        AudioOutput, AudioTimeline, audio_pipeline_description, presentation_size,
        video_decoder_chain,
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
        assert_eq!(
            video_decoder_chain(true),
            "vah264dec ! videorate drop-only=true max-rate=30 ! vapostproc"
        );
        assert_eq!(
            video_decoder_chain(false),
            "avdec_h264 ! videorate drop-only=true max-rate=30 ! videoconvert ! videoscale"
        );
    }

    #[test]
    fn presentation_is_scaled_to_the_reference_window_before_readback() {
        assert_eq!(presentation_size(1920, 1080), (1280, 720));
        assert_eq!(presentation_size(1280, 720), (1280, 720));
        assert_eq!(presentation_size(800, 600), (800, 600));
    }
}
