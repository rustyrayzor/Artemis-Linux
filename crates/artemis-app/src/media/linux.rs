use crossbeam_channel::{Receiver, bounded};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use artemis_moonlight::StreamEvent;

pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub struct MediaRuntime {
    video: Option<VideoPipeline>,
    audio: Option<AudioPipeline>,
    frames: Receiver<DecodedFrame>,
    frame_sender: crossbeam_channel::Sender<DecodedFrame>,
}

impl MediaRuntime {
    pub fn new() -> Result<Self, String> {
        gst::init().map_err(|error| error.to_string())?;
        let (frame_sender, frames) = bounded(2);
        Ok(Self {
            video: None,
            audio: None,
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
            StreamEvent::AudioSetup {
                sample_rate,
                channels,
                ..
            } => {
                self.audio = Some(AudioPipeline::new(sample_rate, channels)?);
            }
            StreamEvent::AudioPacket(packet) => {
                if let Some(audio) = &self.audio {
                    audio.push(packet)?;
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
            .or_else(|| {
                self.audio
                    .as_ref()
                    .and_then(|audio| pipeline_error(&audio.pipeline))
            })
    }

    pub fn shutdown(&mut self) {
        self.video.take();
        self.audio.take();
    }
}

struct VideoPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
}

impl VideoPipeline {
    fn new(
        _width: i32,
        _height: i32,
        _fps: i32,
        frames: crossbeam_channel::Sender<DecodedFrame>,
    ) -> Result<Self, String> {
        let pipeline = gst::parse::launch(
            "appsrc name=video_src is-live=true format=time do-timestamp=false \
             caps=video/x-h264,stream-format=byte-stream,alignment=au ! \
             h264parse config-interval=-1 ! avdec_h264 ! videoconvert ! \
             video/x-raw,format=RGBA ! \
             appsink name=video_sink max-buffers=2 drop=true sync=false",
        )
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

struct AudioPipeline {
    pipeline: gst::Pipeline,
    source: gst_app::AppSrc,
}

impl AudioPipeline {
    fn new(sample_rate: i32, channels: i32) -> Result<Self, String> {
        let description = format!(
            "appsrc name=audio_src is-live=true format=time do-timestamp=true \
             caps=audio/x-opus,rate={sample_rate},channels={channels},\
             channel-mapping-family=0 ! opusparse ! opusdec ! audioconvert ! \
             audioresample ! autoaudiosink sync=false"
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| error.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "GStreamer did not construct an audio pipeline".to_owned())?;
        let source = pipeline
            .by_name("audio_src")
            .ok_or_else(|| "audio appsrc is missing".to_owned())?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| "audio source has the wrong GStreamer type".to_owned())?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        Ok(Self { pipeline, source })
    }

    fn push(&self, packet: Vec<u8>) -> Result<(), String> {
        self.source
            .push_buffer(gst::Buffer::from_mut_slice(packet))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        let _ = self.source.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
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
