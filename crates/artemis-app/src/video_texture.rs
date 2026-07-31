#![allow(unsafe_code)]

use std::sync::Arc;

#[cfg(target_os = "linux")]
use artemis_moonlight::VideoColorInfo;
use eframe::egui;
use eframe::glow::{self, HasContext};
#[cfg(target_os = "linux")]
use gstreamer_gl as gst_gl;
#[cfg(target_os = "linux")]
use gstreamer_gl::GLVideoFrameExt;
#[cfg(target_os = "linux")]
use gstreamer_video as gst_video;
#[cfg(target_os = "linux")]
use gstreamer_video::VideoFrameExt;

#[cfg(target_os = "linux")]
use crate::hdr_surface::NativeHdrSurface;
use crate::media::DecodedFrame;

const OUTPUT_TEXTURE_COUNT: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputTextureFormat {
    EightBitSrgb,
    TenBitRgb,
}

const VERTEX_SHADER: &str = r"#version 330 core
out vec2 texture_coordinate;

void main() {
    const vec2 positions[3] = vec2[3](
        vec2(-1.0, -1.0),
        vec2( 3.0, -1.0),
        vec2(-1.0,  3.0)
    );
    vec2 position = positions[gl_VertexID];
    texture_coordinate = position * 0.5 + 0.5;
    gl_Position = vec4(position, 0.0, 1.0);
}
";

const FRAGMENT_SHADER: &str = r"#version 330 core
in vec2 texture_coordinate;
out vec4 output_color;

uniform sampler2D luma_texture;
uniform sampler2D chroma_texture;

void main() {
    float y = texture(luma_texture, texture_coordinate).r;
    vec2 uv = texture(chroma_texture, texture_coordinate).rg - vec2(0.5);
    y = 1.16438356 * (y - (16.0 / 255.0));
    vec3 rgb = vec3(
        y + 1.79274107 * uv.y,
        y - 0.21324861 * uv.x - 0.53290933 * uv.y,
        y + 2.11240179 * uv.x
    );
    output_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
";

const HDR_TONE_MAP_FRAGMENT_SHADER: &str = r"#version 330 core
in vec2 texture_coordinate;
out vec4 output_color;

uniform sampler2D source_texture;
uniform sampler2D pq_lut;
uniform float source_peak_nits;

vec3 pq_eotf(vec3 encoded) {
    vec3 coordinate = clamp(encoded, vec3(0.0), vec3(1.0));
    return vec3(
        texture(pq_lut, vec2(coordinate.r, 0.5)).r,
        texture(pq_lut, vec2(coordinate.g, 0.5)).r,
        texture(pq_lut, vec2(coordinate.b, 0.5)).r
    );
}

vec3 bt2020_to_bt709(vec3 color) {
    return vec3(
        dot(color, vec3( 1.6605, -0.5876, -0.0728)),
        dot(color, vec3(-0.1246,  1.1329, -0.0083)),
        dot(color, vec3(-0.0182, -0.1006,  1.1187))
    );
}

vec3 aces_fitted(vec3 color) {
    const float a = 2.51;
    const float b = 0.03;
    const float c = 2.43;
    const float d = 0.59;
    const float e = 0.14;
    return clamp((color * (a * color + b)) / (color * (c * color + d) + e), 0.0, 1.0);
}

void main() {
    vec3 pq_bt2020 = texture(source_texture, texture_coordinate).rgb;
    vec3 linear_nits = bt2020_to_bt709(pq_eotf(pq_bt2020));
    float exposure = clamp(1000.0 / max(source_peak_nits, 100.0), 0.5, 2.0);
    vec3 tone_mapped = aces_fitted(max(linear_nits, vec3(0.0)) * (exposure / 203.0));
    output_color = vec4(tone_mapped, 1.0);
}
";

const HDR_NATIVE_FRAGMENT_SHADER: &str = r"#version 330 core
in vec2 texture_coordinate;
out vec4 output_color;

uniform sampler2D source_texture;

void main() {
    output_color = vec4(texture(source_texture, texture_coordinate).rgb, 1.0);
}
";

pub struct StreamTexture {
    gl: Arc<glow::Context>,
    ids: [egui::TextureId; OUTPUT_TEXTURE_COUNT],
    outputs: [glow::Texture; OUTPUT_TEXTURE_COUNT],
    current_output: usize,
    luma: glow::Texture,
    chroma: glow::Texture,
    pixel_buffers: [glow::Buffer; 2],
    next_pixel_buffer: usize,
    program: glow::Program,
    hdr_tone_map_program: glow::Program,
    hdr_native_program: glow::Program,
    pq_lut: glow::Texture,
    luma_uniform: glow::UniformLocation,
    chroma_uniform: glow::UniformLocation,
    hdr_source_uniform: glow::UniformLocation,
    hdr_pq_lut_uniform: glow::UniformLocation,
    hdr_peak_uniform: glow::UniformLocation,
    hdr_native_source_uniform: glow::UniformLocation,
    vertex_array: glow::VertexArray,
    framebuffer: glow::Framebuffer,
    source_framebuffer: glow::Framebuffer,
    size: [usize; 2],
    output_format: Option<OutputTextureFormat>,
    hdr_source: Option<glow::Texture>,
    hdr_source_peak_nits: f32,
    #[cfg(target_os = "linux")]
    hdr_surface: Option<NativeHdrSurface>,
    #[cfg(target_os = "linux")]
    hdr_native_failure: Option<String>,
    #[cfg(target_os = "linux")]
    hdr_source_sample: Option<gstreamer::Sample>,
}

impl StreamTexture {
    pub fn new(
        frame: &mut eframe::Frame,
        decoded: &DecodedFrame,
        native_hdr_allowed: bool,
    ) -> Result<Self, String> {
        let gl = frame
            .gl()
            .cloned()
            .ok_or_else(|| "the OpenGL renderer is unavailable".to_owned())?;
        // SAFETY: eframe's OpenGL context is current on the UI thread during `App::update`.
        let resources = unsafe { create_resources(&gl)? };
        let ids = resources
            .outputs
            .map(|output| frame.register_native_glow_texture(output));
        let mut texture = Self {
            gl,
            ids,
            outputs: resources.outputs,
            current_output: OUTPUT_TEXTURE_COUNT - 1,
            luma: resources.luma,
            chroma: resources.chroma,
            pixel_buffers: resources.pixel_buffers,
            next_pixel_buffer: 0,
            program: resources.program,
            hdr_tone_map_program: resources.hdr_tone_map_program,
            hdr_native_program: resources.hdr_native_program,
            pq_lut: resources.pq_lut,
            luma_uniform: resources.luma_uniform,
            chroma_uniform: resources.chroma_uniform,
            hdr_source_uniform: resources.hdr_source_uniform,
            hdr_pq_lut_uniform: resources.hdr_pq_lut_uniform,
            hdr_peak_uniform: resources.hdr_peak_uniform,
            hdr_native_source_uniform: resources.hdr_native_source_uniform,
            vertex_array: resources.vertex_array,
            framebuffer: resources.framebuffer,
            source_framebuffer: resources.source_framebuffer,
            size: [0, 0],
            output_format: None,
            hdr_source: None,
            hdr_source_peak_nits: 1_000.0,
            #[cfg(target_os = "linux")]
            hdr_surface: match NativeHdrSurface::new(frame) {
                Ok(surface) => Some(surface),
                Err(error) => {
                    tracing::info!(target: "artemis::hdr", %error, "native HDR is unavailable");
                    None
                }
            },
            #[cfg(target_os = "linux")]
            hdr_native_failure: None,
            #[cfg(target_os = "linux")]
            hdr_source_sample: None,
        };
        texture.upload(decoded, native_hdr_allowed)?;
        Ok(texture)
    }

    pub fn id(&self) -> egui::TextureId {
        self.ids[self.current_output]
    }

    pub fn size_vec2(&self) -> egui::Vec2 {
        let width = u16::try_from(self.size[0]).unwrap_or(u16::MAX);
        let height = u16::try_from(self.size[1]).unwrap_or(u16::MAX);
        egui::vec2(f32::from(width), f32::from(height))
    }

    pub fn hdr_paint_callback(&self, rect: egui::Rect) -> Option<egui::PaintCallback> {
        let source = self.hdr_source?;
        #[cfg(target_os = "linux")]
        let native_hdr = self
            .hdr_surface
            .as_ref()
            .is_some_and(NativeHdrSurface::is_active);
        #[cfg(not(target_os = "linux"))]
        let native_hdr = false;
        let tone_map_program = self.hdr_tone_map_program;
        let native_program = self.hdr_native_program;
        let pq_lut = self.pq_lut;
        let source_uniform = self.hdr_source_uniform;
        let pq_lut_uniform = self.hdr_pq_lut_uniform;
        let peak_uniform = self.hdr_peak_uniform;
        let native_source_uniform = self.hdr_native_source_uniform;
        let vertex_array = self.vertex_array;
        let source_peak_nits = self.hdr_source_peak_nits;
        let callback = eframe::egui_glow::CallbackFn::new(move |_info, painter| {
            // SAFETY: The callback executes on eframe's GL thread while the decoded GStreamer
            // sample retained by `StreamTexture` keeps `source` alive in a shared GL context.
            // egui restores its own GL state after the callback returns.
            unsafe {
                let gl = painter.gl();
                if native_hdr {
                    gl.disable(glow::FRAMEBUFFER_SRGB);
                } else {
                    gl.enable(glow::FRAMEBUFFER_SRGB);
                }
                gl.disable(glow::BLEND);
                gl.disable(glow::CULL_FACE);
                gl.disable(glow::DEPTH_TEST);
                gl.use_program(Some(if native_hdr {
                    native_program
                } else {
                    tone_map_program
                }));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(source));
                if native_hdr {
                    gl.uniform_1_i32(Some(&native_source_uniform), 0);
                } else {
                    gl.uniform_1_i32(Some(&source_uniform), 0);
                    gl.active_texture(glow::TEXTURE1);
                    gl.bind_texture(glow::TEXTURE_2D, Some(pq_lut));
                    gl.uniform_1_i32(Some(&pq_lut_uniform), 1);
                    gl.uniform_1_f32(Some(&peak_uniform), source_peak_nits);
                }
                gl.bind_vertex_array(Some(vertex_array));
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
        });
        Some(egui::PaintCallback {
            rect,
            callback: Arc::new(callback),
        })
    }

    pub fn native_hdr_active(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.hdr_surface
                .as_ref()
                .is_some_and(NativeHdrSurface::is_active)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub fn upload(
        &mut self,
        decoded: &DecodedFrame,
        native_hdr_allowed: bool,
    ) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            let caps = decoded
                .sample
                .caps()
                .ok_or_else(|| "decoded video sample has no caps".to_owned())?;
            let info = gst_video::VideoInfo::from_caps(caps)
                .map_err(|error| format!("invalid decoded video caps: {error}"))?;
            let width = usize::try_from(info.width())
                .map_err(|_| "decoded video width is too large".to_owned())?;
            let height = usize::try_from(info.height())
                .map_err(|_| "decoded video height is too large".to_owned())?;
            if [width, height] != [decoded.width, decoded.height] {
                return Err(format!(
                    "decoded video caps are {width}x{height}; expected {}x{}",
                    decoded.width, decoded.height
                ));
            }
            if matches!(
                info.format(),
                gst_video::VideoFormat::Rgba | gst_video::VideoFormat::Rgb10a2Le
            ) {
                return self.upload_gl_frame(decoded, &info, native_hdr_allowed);
            }
            if info.format() != gst_video::VideoFormat::Nv12 {
                return Err(format!(
                    "decoded video format is {:?}; expected NV12, RGBA, or RGB10A2_LE",
                    info.format()
                ));
            }
            let buffer = decoded
                .sample
                .buffer()
                .ok_or_else(|| "decoded video sample has no buffer".to_owned())?;
            let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                .map_err(|error| format!("could not map decoded video frame: {error}"))?;
            let strides = frame.plane_stride();
            let luma_stride = usize::try_from(strides[0])
                .map_err(|_| "decoded luma stride is invalid".to_owned())?;
            let chroma_stride = usize::try_from(strides[1])
                .map_err(|_| "decoded chroma stride is invalid".to_owned())?;
            let luma = frame
                .plane_data(0)
                .map_err(|error| format!("could not map decoded luma plane: {error}"))?;
            let chroma = frame
                .plane_data(1)
                .map_err(|error| format!("could not map decoded chroma plane: {error}"))?;
            self.upload_nv12_planes(
                decoded.width,
                decoded.height,
                luma,
                luma_stride,
                chroma,
                chroma_stride,
            )
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = native_hdr_allowed;
            let expected_bytes = expected_nv12_bytes(decoded.width, decoded.height)?;
            if decoded.nv12.len() != expected_bytes {
                return Err(format!(
                    "decoded NV12 buffer has {} bytes; expected {expected_bytes}",
                    decoded.nv12.len()
                ));
            }
            let luma_bytes = decoded
                .width
                .checked_mul(decoded.height)
                .ok_or_else(|| "decoded video dimensions overflowed".to_owned())?;
            self.upload_nv12_planes(
                decoded.width,
                decoded.height,
                &decoded.nv12[..luma_bytes],
                decoded.width,
                &decoded.nv12[luma_bytes..],
                decoded.width,
            )
        }
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_lines)]
    fn upload_gl_frame(
        &mut self,
        decoded: &DecodedFrame,
        info: &gst_video::VideoInfo,
        native_hdr_allowed: bool,
    ) -> Result<(), String> {
        let context = decoded
            .gl_context
            .as_ref()
            .ok_or_else(|| "decoded GL frame has no application GL context".to_owned())?;
        let buffer = decoded
            .sample
            .buffer()
            .ok_or_else(|| "decoded GL sample has no buffer".to_owned())?;
        let sync = buffer
            .meta::<gst_gl::GLSyncMeta>()
            .ok_or_else(|| "decoded GL frame has no synchronization metadata".to_owned())?;
        sync.wait(context);
        let frame = gst_gl::GLVideoFrameRef::from_buffer_ref_readable(buffer, info)
            .map_err(|error| format!("could not map decoded GL frame: {error}"))?;
        if frame.texture_target(0).map_err(|error| error.to_string())?
            != gst_gl::GLTextureTarget::_2d
        {
            return Err("decoded GL frame is not a 2D texture".to_owned());
        }
        let source_name = frame.texture_id(0).map_err(|error| error.to_string())?;
        let source_name = std::num::NonZeroU32::new(source_name)
            .ok_or_else(|| "decoded GL frame has an invalid texture name".to_owned())?;
        let source = glow::NativeTexture(source_name);
        let width = i32::try_from(decoded.width)
            .map_err(|_| "decoded video width is too large".to_owned())?;
        let height = i32::try_from(decoded.height)
            .map_err(|_| "decoded video height is too large".to_owned())?;
        if decoded.color.hdr_active {
            if !native_hdr_allowed {
                if let Some(surface) = &mut self.hdr_surface {
                    surface.deactivate()?;
                }
            } else if self.hdr_native_failure.is_none() {
                if let Some(surface) = &mut self.hdr_surface {
                    let was_active = surface.is_active();
                    if let Err(error) = surface.activate(decoded.color.hdr_metadata) {
                        tracing::warn!(target: "artemis::hdr", %error, "native HDR activation failed; using SDR tone map");
                        self.hdr_native_failure = Some(error);
                    } else if !was_active {
                        tracing::info!(
                            target: "artemis::hdr",
                            metadata = decoded.color.hdr_metadata.is_some(),
                            "native BT.2020/PQ HDR presentation active"
                        );
                    }
                }
            }
            self.hdr_source = Some(source);
            self.hdr_source_peak_nits = hdr_peak_nits(decoded.color);
            self.hdr_source_sample = Some(decoded.sample.clone());
            self.size = [decoded.width, decoded.height];
            self.output_format = None;
            return Ok(());
        }
        if let Some(surface) = &mut self.hdr_surface {
            if let Err(error) = surface.deactivate() {
                tracing::warn!(target: "artemis::hdr", %error, "could not clear native HDR state");
            }
        }
        self.hdr_native_failure = None;
        self.hdr_source = None;
        self.hdr_source_sample = None;
        let output_index = next_output_index(self.current_output);
        let output = self.outputs[output_index];
        let output_format = match (info.format(), decoded.color.hdr_active) {
            (gst_video::VideoFormat::Rgb10a2Le, true) | (gst_video::VideoFormat::Rgba, _) => {
                OutputTextureFormat::EightBitSrgb
            }
            (gst_video::VideoFormat::Rgb10a2Le, false) => OutputTextureFormat::TenBitRgb,
            (format, _) => return Err(format!("unsupported GL video format {format:?}")),
        };

        // SAFETY: The producer texture belongs to a context that shares objects with eframe's
        // current context. GLSyncMeta has completed the cross-context wait before the texture is
        // attached, and both framebuffers are valid for this context.
        unsafe {
            if self.size != [decoded.width, decoded.height]
                || self.output_format != Some(output_format)
            {
                allocate_textures(
                    &self.gl,
                    self.outputs,
                    self.luma,
                    self.chroma,
                    width,
                    height,
                    output_format,
                )?;
            }
            attach_output_texture(&self.gl, self.framebuffer, output)?;
            let srgb_enabled = self.gl.is_enabled(glow::FRAMEBUFFER_SRGB);
            self.gl.disable(glow::FRAMEBUFFER_SRGB);
            self.gl
                .bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.source_framebuffer));
            self.gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(source),
                0,
            );
            let source_status = self.gl.check_framebuffer_status(glow::READ_FRAMEBUFFER);
            if source_status != glow::FRAMEBUFFER_COMPLETE {
                self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
                if srgb_enabled {
                    self.gl.enable(glow::FRAMEBUFFER_SRGB);
                }
                return Err(format!(
                    "decoded GL framebuffer is incomplete: 0x{source_status:x}"
                ));
            }
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.framebuffer));
            self.gl.blit_framebuffer(
                0,
                0,
                width,
                height,
                0,
                0,
                width,
                height,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            if srgb_enabled {
                self.gl.enable(glow::FRAMEBUFFER_SRGB);
            }
            let error = self.gl.get_error();
            if error != glow::NO_ERROR {
                return Err(format!("OpenGL video copy failed with error 0x{error:x}"));
            }
        }
        self.current_output = output_index;
        self.size = [decoded.width, decoded.height];
        self.output_format = Some(output_format);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_nv12_planes(
        &mut self,
        decoded_width: usize,
        decoded_height: usize,
        luma: &[u8],
        luma_stride: usize,
        chroma: &[u8],
        chroma_stride: usize,
    ) -> Result<(), String> {
        self.hdr_source = None;
        #[cfg(target_os = "linux")]
        {
            self.hdr_source_sample = None;
        }
        let layout = nv12_upload_layout(
            decoded_width,
            decoded_height,
            luma.len(),
            luma_stride,
            chroma.len(),
            chroma_stride,
        )?;
        let width = i32::try_from(decoded_width)
            .map_err(|_| "decoded video width is too large".to_owned())?;
        let height = i32::try_from(decoded_height)
            .map_err(|_| "decoded video height is too large".to_owned())?;
        let luma_row_length = i32::try_from(luma_stride)
            .map_err(|_| "decoded luma stride is too large for OpenGL".to_owned())?;
        let chroma_row_length = i32::try_from(chroma_stride / 2)
            .map_err(|_| "decoded chroma stride is too large for OpenGL".to_owned())?;
        let pixel_buffer_bytes = i32::try_from(layout.total)
            .map_err(|_| "decoded video buffer is too large for OpenGL".to_owned())?;
        let chroma_offset = i32::try_from(layout.luma_span)
            .map_err(|_| "decoded video buffer is too large for OpenGL".to_owned())?;
        let chroma_upload_offset = u32::try_from(layout.luma_span)
            .map_err(|_| "decoded video buffer is too large for OpenGL".to_owned())?;
        let output_format = OutputTextureFormat::EightBitSrgb;
        let allocate = self.size != [decoded_width, decoded_height]
            || self.output_format != Some(output_format);
        let output_index = next_output_index(self.current_output);
        let output = self.outputs[output_index];
        let pixel_buffer = self.pixel_buffers[self.next_pixel_buffer];

        // SAFETY: Every GL object belongs to the current eframe context. The validated decoded
        // planes are copied directly into an orphaned pixel buffer. UNPACK_ROW_LENGTH preserves
        // hardware-decoder stride without repacking the full 4K frame in the GStreamer callback.
        unsafe {
            if allocate {
                allocate_textures(
                    &self.gl,
                    self.outputs,
                    self.luma,
                    self.chroma,
                    width,
                    height,
                    output_format,
                )?;
            }
            attach_output_texture(&self.gl, self.framebuffer, output)?;
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            self.gl
                .bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(pixel_buffer));
            self.gl.buffer_data_size(
                glow::PIXEL_UNPACK_BUFFER,
                pixel_buffer_bytes,
                glow::STREAM_DRAW,
            );
            self.gl.buffer_sub_data_u8_slice(
                glow::PIXEL_UNPACK_BUFFER,
                0,
                &luma[..layout.luma_span],
            );
            self.gl.buffer_sub_data_u8_slice(
                glow::PIXEL_UNPACK_BUFFER,
                chroma_offset,
                &chroma[..layout.chroma_span],
            );
            self.gl
                .pixel_store_i32(glow::UNPACK_ROW_LENGTH, luma_row_length);
            upload_plane(&self.gl, self.luma, width, height, glow::RED, 0);
            self.gl
                .pixel_store_i32(glow::UNPACK_ROW_LENGTH, chroma_row_length);
            upload_plane(
                &self.gl,
                self.chroma,
                width / 2,
                height / 2,
                glow::RG,
                chroma_upload_offset,
            );
            self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            self.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            render_nv12(self, width, height);
            let error = self.gl.get_error();
            if error != glow::NO_ERROR {
                return Err(format!("OpenGL video upload failed with error 0x{error:x}"));
            }
        }

        self.next_pixel_buffer = (self.next_pixel_buffer + 1) % self.pixel_buffers.len();
        self.current_output = output_index;
        self.size = [decoded_width, decoded_height];
        self.output_format = Some(output_format);
        Ok(())
    }
}

impl Drop for StreamTexture {
    fn drop(&mut self) {
        // SAFETY: Artemis creates and destroys the renderer on eframe's UI thread while the same
        // context is alive. The output texture is owned and deleted by eframe after registration.
        unsafe {
            self.gl.delete_texture(self.luma);
            self.gl.delete_texture(self.chroma);
            for buffer in self.pixel_buffers {
                self.gl.delete_buffer(buffer);
            }
            self.gl.delete_program(self.program);
            self.gl.delete_program(self.hdr_tone_map_program);
            self.gl.delete_program(self.hdr_native_program);
            self.gl.delete_texture(self.pq_lut);
            self.gl.delete_vertex_array(self.vertex_array);
            self.gl.delete_framebuffer(self.framebuffer);
            self.gl.delete_framebuffer(self.source_framebuffer);
        }
    }
}

struct Resources {
    outputs: [glow::Texture; OUTPUT_TEXTURE_COUNT],
    luma: glow::Texture,
    chroma: glow::Texture,
    pixel_buffers: [glow::Buffer; 2],
    program: glow::Program,
    hdr_tone_map_program: glow::Program,
    hdr_native_program: glow::Program,
    pq_lut: glow::Texture,
    luma_uniform: glow::UniformLocation,
    chroma_uniform: glow::UniformLocation,
    hdr_source_uniform: glow::UniformLocation,
    hdr_pq_lut_uniform: glow::UniformLocation,
    hdr_peak_uniform: glow::UniformLocation,
    hdr_native_source_uniform: glow::UniformLocation,
    vertex_array: glow::VertexArray,
    framebuffer: glow::Framebuffer,
    source_framebuffer: glow::Framebuffer,
}

#[allow(unsafe_code)]
#[allow(clippy::too_many_lines)]
unsafe fn create_resources(gl: &glow::Context) -> Result<Resources, String> {
    unsafe {
        let first_output = gl.create_texture()?;
        let second_output = match gl.create_texture() {
            Ok(texture) => texture,
            Err(error) => {
                gl.delete_texture(first_output);
                return Err(error);
            }
        };
        let third_output = match gl.create_texture() {
            Ok(texture) => texture,
            Err(error) => {
                delete_textures(gl, [first_output, second_output]);
                return Err(error);
            }
        };
        let outputs = [first_output, second_output, third_output];
        let luma = match gl.create_texture() {
            Ok(texture) => texture,
            Err(error) => {
                delete_textures(gl, outputs);
                return Err(error);
            }
        };
        let chroma = match gl.create_texture() {
            Ok(texture) => texture,
            Err(error) => {
                gl.delete_texture(luma);
                delete_textures(gl, outputs);
                return Err(error);
            }
        };
        let first_buffer = match gl.create_buffer() {
            Ok(buffer) => buffer,
            Err(error) => {
                delete_textures(gl, outputs);
                delete_textures(gl, [luma, chroma]);
                return Err(error);
            }
        };
        let second_buffer = match gl.create_buffer() {
            Ok(buffer) => buffer,
            Err(error) => {
                gl.delete_buffer(first_buffer);
                delete_textures(gl, outputs);
                delete_textures(gl, [luma, chroma]);
                return Err(error);
            }
        };
        let program = match compile_program(gl, FRAGMENT_SHADER) {
            Ok(program) => program,
            Err(error) => {
                gl.delete_buffer(first_buffer);
                gl.delete_buffer(second_buffer);
                delete_textures(gl, outputs);
                delete_textures(gl, [luma, chroma]);
                return Err(error);
            }
        };
        let Some(luma_uniform) = gl.get_uniform_location(program, "luma_texture") else {
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                outputs,
                [luma, chroma],
            );
            return Err("NV12 shader is missing its luma sampler".to_owned());
        };
        let Some(chroma_uniform) = gl.get_uniform_location(program, "chroma_texture") else {
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                outputs,
                [luma, chroma],
            );
            return Err("NV12 shader is missing its chroma sampler".to_owned());
        };
        let hdr_tone_map_program = match compile_program(gl, HDR_TONE_MAP_FRAGMENT_SHADER) {
            Ok(program) => program,
            Err(error) => {
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    outputs,
                    [luma, chroma],
                );
                return Err(error);
            }
        };
        let Some(hdr_source_uniform) =
            gl.get_uniform_location(hdr_tone_map_program, "source_texture")
        else {
            gl.delete_program(hdr_tone_map_program);
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                outputs,
                [luma, chroma],
            );
            return Err("HDR tone-map shader is missing its source sampler".to_owned());
        };
        let Some(hdr_peak_uniform) =
            gl.get_uniform_location(hdr_tone_map_program, "source_peak_nits")
        else {
            gl.delete_program(hdr_tone_map_program);
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                outputs,
                [luma, chroma],
            );
            return Err("HDR tone-map shader is missing its peak luminance uniform".to_owned());
        };
        let Some(hdr_pq_lut_uniform) = gl.get_uniform_location(hdr_tone_map_program, "pq_lut")
        else {
            gl.delete_program(hdr_tone_map_program);
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                outputs,
                [luma, chroma],
            );
            return Err("HDR tone-map shader is missing its PQ lookup sampler".to_owned());
        };
        let pq_lut = match create_pq_lut_texture(gl) {
            Ok(texture) => texture,
            Err(error) => {
                gl.delete_program(hdr_tone_map_program);
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    outputs,
                    [luma, chroma],
                );
                return Err(error);
            }
        };
        let hdr_native_program = match compile_program(gl, HDR_NATIVE_FRAGMENT_SHADER) {
            Ok(program) => program,
            Err(error) => {
                gl.delete_texture(pq_lut);
                gl.delete_program(hdr_tone_map_program);
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    outputs,
                    [luma, chroma],
                );
                return Err(error);
            }
        };
        let Some(hdr_native_source_uniform) =
            gl.get_uniform_location(hdr_native_program, "source_texture")
        else {
            gl.delete_program(hdr_native_program);
            gl.delete_texture(pq_lut);
            gl.delete_program(hdr_tone_map_program);
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                outputs,
                [luma, chroma],
            );
            return Err("native HDR shader is missing its source sampler".to_owned());
        };
        let vertex_array = match gl.create_vertex_array() {
            Ok(array) => array,
            Err(error) => {
                gl.delete_program(hdr_native_program);
                gl.delete_texture(pq_lut);
                gl.delete_program(hdr_tone_map_program);
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    outputs,
                    [luma, chroma],
                );
                return Err(error);
            }
        };
        let [framebuffer, source_framebuffer] = match create_framebuffers(gl) {
            Ok(framebuffers) => framebuffers,
            Err(error) => {
                gl.delete_vertex_array(vertex_array);
                gl.delete_program(hdr_native_program);
                gl.delete_texture(pq_lut);
                gl.delete_program(hdr_tone_map_program);
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    outputs,
                    [luma, chroma],
                );
                return Err(error);
            }
        };
        Ok(Resources {
            outputs,
            luma,
            chroma,
            pixel_buffers: [first_buffer, second_buffer],
            program,
            hdr_tone_map_program,
            hdr_native_program,
            pq_lut,
            luma_uniform,
            chroma_uniform,
            hdr_source_uniform,
            hdr_pq_lut_uniform,
            hdr_peak_uniform,
            hdr_native_source_uniform,
            vertex_array,
            framebuffer,
            source_framebuffer,
        })
    }
}

#[allow(unsafe_code)]
unsafe fn create_framebuffers(gl: &glow::Context) -> Result<[glow::Framebuffer; 2], String> {
    unsafe {
        let framebuffer = gl.create_framebuffer()?;
        match gl.create_framebuffer() {
            Ok(source_framebuffer) => Ok([framebuffer, source_framebuffer]),
            Err(error) => {
                gl.delete_framebuffer(framebuffer);
                Err(error)
            }
        }
    }
}

#[allow(unsafe_code)]
unsafe fn compile_program(
    gl: &glow::Context,
    fragment_source: &str,
) -> Result<glow::Program, String> {
    unsafe {
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX_SHADER)?;
        let fragment = match compile_shader(gl, glow::FRAGMENT_SHADER, fragment_source) {
            Ok(shader) => shader,
            Err(error) => {
                gl.delete_shader(vertex);
                return Err(error);
            }
        };
        let program = match gl.create_program() {
            Ok(program) => program,
            Err(error) => {
                gl.delete_shader(vertex);
                gl.delete_shader(fragment);
                return Err(error);
            }
        };
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
        gl.detach_shader(program, vertex);
        gl.detach_shader(program, fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
        if gl.get_program_link_status(program) {
            Ok(program)
        } else {
            let error = gl.get_program_info_log(program);
            gl.delete_program(program);
            Err(error)
        }
    }
}

#[allow(unsafe_code)]
unsafe fn compile_shader(
    gl: &glow::Context,
    shader_type: u32,
    source: &str,
) -> Result<glow::Shader, String> {
    unsafe {
        let shader = gl.create_shader(shader_type)?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if gl.get_shader_compile_status(shader) {
            Ok(shader)
        } else {
            let error = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            Err(error)
        }
    }
}

const PQ_LUT_SIZE: u16 = 4_096;

fn pq_eotf_nits(encoded: f32) -> f32 {
    const M1: f32 = 2_610.0 / 16_384.0;
    const M2: f32 = 2_523.0 / 32.0;
    const C1: f32 = 3_424.0 / 4_096.0;
    const C2: f32 = 2_413.0 / 128.0;
    const C3: f32 = 2_392.0 / 128.0;
    let power = encoded.clamp(0.0, 1.0).powf(1.0 / M2);
    let numerator = (power - C1).max(0.0);
    let denominator = (C2 - C3 * power).max(f32::EPSILON);
    (numerator / denominator).powf(1.0 / M1) * 10_000.0
}

#[allow(unsafe_code)]
unsafe fn create_pq_lut_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    let denominator = f32::from(PQ_LUT_SIZE - 1);
    let values = (0..PQ_LUT_SIZE)
        .map(|index| pq_eotf_nits(f32::from(index) / denominator))
        .collect::<Vec<_>>();
    let byte_length = values
        .len()
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| "PQ lookup texture size overflowed".to_owned())?;
    // SAFETY: `values` is contiguous initialized f32 storage. Reinterpreting it as bytes for
    // the synchronous OpenGL upload preserves the exact allocation bounds and lifetime.
    let bytes = unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), byte_length) };
    let linear = i32::try_from(glow::LINEAR).map_err(|_| "invalid GL filter".to_owned())?;
    let clamp = i32::try_from(glow::CLAMP_TO_EDGE).map_err(|_| "invalid GL wrap".to_owned())?;
    let internal_format =
        i32::try_from(glow::R32F).map_err(|_| "invalid GL PQ format".to_owned())?;
    let width = i32::from(PQ_LUT_SIZE);
    let texture = unsafe { gl.create_texture()? };
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, linear);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, linear);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, clamp);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, clamp);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal_format,
            width,
            1,
            0,
            glow::RED,
            glow::FLOAT,
            glow::PixelUnpackData::Slice(Some(bytes)),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
        let error = gl.get_error();
        if error != glow::NO_ERROR {
            gl.delete_texture(texture);
            return Err(format!("could not upload PQ lookup texture: 0x{error:x}"));
        }
    }
    Ok(texture)
}

#[allow(unsafe_code)]
#[allow(clippy::too_many_arguments)]
unsafe fn allocate_textures(
    gl: &glow::Context,
    outputs: [glow::Texture; OUTPUT_TEXTURE_COUNT],
    luma: glow::Texture,
    chroma: glow::Texture,
    width: i32,
    height: i32,
    output_format: OutputTextureFormat,
) -> Result<(), String> {
    unsafe {
        allocate_texture(
            gl,
            luma,
            glow::R8,
            width,
            height,
            glow::RED,
            glow::UNSIGNED_BYTE,
        )?;
        allocate_texture(
            gl,
            chroma,
            glow::RG8,
            width / 2,
            height / 2,
            glow::RG,
            glow::UNSIGNED_BYTE,
        )?;
        let (internal_format, pixel_type) = match output_format {
            OutputTextureFormat::EightBitSrgb => (glow::SRGB8_ALPHA8, glow::UNSIGNED_BYTE),
            OutputTextureFormat::TenBitRgb => (glow::RGB10_A2, glow::UNSIGNED_INT_2_10_10_10_REV),
        };
        for output in outputs {
            allocate_texture(
                gl,
                output,
                internal_format,
                width,
                height,
                glow::RGBA,
                pixel_type,
            )?;
        }
        Ok(())
    }
}

#[allow(unsafe_code)]
unsafe fn attach_output_texture(
    gl: &glow::Context,
    framebuffer: glow::Framebuffer,
    output: glow::Texture,
) -> Result<(), String> {
    unsafe {
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(output),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        if status == glow::FRAMEBUFFER_COMPLETE {
            Ok(())
        } else {
            Err(format!(
                "OpenGL video framebuffer is incomplete: 0x{status:x}"
            ))
        }
    }
}

#[allow(unsafe_code)]
unsafe fn allocate_texture(
    gl: &glow::Context,
    texture: glow::Texture,
    internal_format: u32,
    width: i32,
    height: i32,
    format: u32,
    pixel_type: u32,
) -> Result<(), String> {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        let linear =
            i32::try_from(glow::LINEAR).map_err(|_| "invalid GL filter value".to_owned())?;
        let clamp =
            i32::try_from(glow::CLAMP_TO_EDGE).map_err(|_| "invalid GL wrap value".to_owned())?;
        let internal_format = i32::try_from(internal_format)
            .map_err(|_| "invalid GL texture format value".to_owned())?;
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, linear);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, linear);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, clamp);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, clamp);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal_format,
            width,
            height,
            0,
            format,
            pixel_type,
            glow::PixelUnpackData::Slice(None),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(())
    }
}

#[allow(unsafe_code)]
unsafe fn upload_plane(
    gl: &glow::Context,
    texture: glow::Texture,
    width: i32,
    height: i32,
    format: u32,
    offset: u32,
) {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            0,
            0,
            width,
            height,
            format,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::BufferOffset(offset),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
}

#[cfg(target_os = "linux")]
fn hdr_peak_nits(color: VideoColorInfo) -> f32 {
    let Some(metadata) = color.hdr_metadata else {
        return 1_000.0;
    };
    [
        metadata.max_content_light_level,
        metadata.max_display_luminance,
        metadata.max_full_frame_luminance,
    ]
    .into_iter()
    .find(|value| *value > 0)
    .map_or(1_000.0, f32::from)
}

#[allow(unsafe_code)]
unsafe fn render_nv12(texture: &StreamTexture, width: i32, height: i32) {
    unsafe {
        let srgb_enabled = texture.gl.is_enabled(glow::FRAMEBUFFER_SRGB);
        texture.gl.disable(glow::FRAMEBUFFER_SRGB);
        texture
            .gl
            .bind_framebuffer(glow::FRAMEBUFFER, Some(texture.framebuffer));
        texture.gl.viewport(0, 0, width, height);
        texture.gl.disable(glow::BLEND);
        texture.gl.disable(glow::DEPTH_TEST);
        texture.gl.disable(glow::CULL_FACE);
        texture.gl.disable(glow::SCISSOR_TEST);
        texture.gl.use_program(Some(texture.program));
        texture.gl.active_texture(glow::TEXTURE0);
        texture
            .gl
            .bind_texture(glow::TEXTURE_2D, Some(texture.luma));
        texture.gl.uniform_1_i32(Some(&texture.luma_uniform), 0);
        texture.gl.active_texture(glow::TEXTURE1);
        texture
            .gl
            .bind_texture(glow::TEXTURE_2D, Some(texture.chroma));
        texture.gl.uniform_1_i32(Some(&texture.chroma_uniform), 1);
        texture.gl.bind_vertex_array(Some(texture.vertex_array));
        texture.gl.draw_arrays(glow::TRIANGLES, 0, 3);
        texture.gl.bind_vertex_array(None);
        texture.gl.active_texture(glow::TEXTURE1);
        texture.gl.bind_texture(glow::TEXTURE_2D, None);
        texture.gl.active_texture(glow::TEXTURE0);
        texture.gl.bind_texture(glow::TEXTURE_2D, None);
        texture.gl.use_program(None);
        texture.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        if srgb_enabled {
            texture.gl.enable(glow::FRAMEBUFFER_SRGB);
        }
    }
}

fn expected_nv12_bytes(width: usize, height: usize) -> Result<usize, String> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err("decoded NV12 dimensions must be non-zero and even".to_owned());
    }
    width
        .checked_mul(height)
        .and_then(|luma| luma.checked_add(luma / 2))
        .ok_or_else(|| "decoded video dimensions overflowed".to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Nv12UploadLayout {
    luma_span: usize,
    chroma_span: usize,
    total: usize,
}

fn nv12_upload_layout(
    width: usize,
    height: usize,
    luma_len: usize,
    luma_stride: usize,
    chroma_len: usize,
    chroma_stride: usize,
) -> Result<Nv12UploadLayout, String> {
    expected_nv12_bytes(width, height)?;
    if luma_stride < width || chroma_stride < width {
        return Err("decoded video plane stride is shorter than its visible width".to_owned());
    }
    if chroma_stride % 2 != 0 {
        return Err("decoded chroma stride must contain whole RG pixels".to_owned());
    }

    let luma_bytes = luma_stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|bytes| bytes.checked_add(width))
        .ok_or_else(|| "decoded luma plane dimensions overflowed".to_owned())?;
    let chroma_height = height / 2;
    let chroma_bytes = chroma_stride
        .checked_mul(chroma_height.saturating_sub(1))
        .and_then(|bytes| bytes.checked_add(width))
        .ok_or_else(|| "decoded chroma plane dimensions overflowed".to_owned())?;
    if luma_len < luma_bytes || chroma_len < chroma_bytes {
        return Err(format!(
            "decoded NV12 planes are too short: luma {luma_len}/{luma_bytes}, chroma {chroma_len}/{chroma_bytes}"
        ));
    }
    let total_bytes = luma_bytes
        .checked_add(chroma_bytes)
        .ok_or_else(|| "decoded video buffer size overflowed".to_owned())?;
    Ok(Nv12UploadLayout {
        luma_span: luma_bytes,
        chroma_span: chroma_bytes,
        total: total_bytes,
    })
}

#[allow(unsafe_code)]
unsafe fn delete_textures<const N: usize>(gl: &glow::Context, textures: [glow::Texture; N]) {
    unsafe {
        for texture in textures {
            gl.delete_texture(texture);
        }
    }
}

#[allow(unsafe_code)]
unsafe fn delete_program_resources(
    gl: &glow::Context,
    program: glow::Program,
    buffers: [glow::Buffer; 2],
    outputs: [glow::Texture; OUTPUT_TEXTURE_COUNT],
    plane_textures: [glow::Texture; 2],
) {
    unsafe {
        gl.delete_program(program);
        for buffer in buffers {
            gl.delete_buffer(buffer);
        }
        delete_textures(gl, outputs);
        delete_textures(gl, plane_textures);
    }
}

fn next_output_index(current: usize) -> usize {
    (current + 1) % OUTPUT_TEXTURE_COUNT
}

#[cfg(test)]
mod tests {
    use super::{
        Nv12UploadLayout, OUTPUT_TEXTURE_COUNT, expected_nv12_bytes, hdr_peak_nits,
        next_output_index, nv12_upload_layout, pq_eotf_nits,
    };
    use artemis_moonlight::{HdrMetadata, VideoColorInfo, VideoColorSpace};

    #[test]
    fn output_textures_rotate_without_reusing_the_current_surface() {
        let mut current = OUTPUT_TEXTURE_COUNT - 1;
        for expected in [0, 1, 2, 0] {
            current = next_output_index(current);
            assert_eq!(current, expected);
        }
    }

    #[test]
    fn nv12_size_is_one_and_a_half_bytes_per_pixel() {
        assert_eq!(expected_nv12_bytes(3_840, 2_160), Ok(12_441_600));
    }

    #[test]
    fn nv12_dimensions_must_be_even() {
        assert!(expected_nv12_bytes(1_919, 1_080).is_err());
        assert!(expected_nv12_bytes(1_920, 1_079).is_err());
    }

    #[test]
    fn nv12_upload_layout_preserves_decoder_plane_stride() {
        assert_eq!(
            nv12_upload_layout(4, 4, 28, 8, 12, 8),
            Ok(Nv12UploadLayout {
                luma_span: 28,
                chroma_span: 12,
                total: 40,
            })
        );
    }

    #[test]
    fn nv12_upload_layout_rejects_short_planes() {
        assert!(nv12_upload_layout(4, 4, 27, 8, 12, 8).is_err());
        assert!(nv12_upload_layout(4, 4, 28, 8, 11, 8).is_err());
    }

    #[test]
    fn tone_mapper_prefers_content_peak_and_has_a_safe_default() {
        assert!((hdr_peak_nits(VideoColorInfo::default()) - 1_000.0).abs() < f32::EPSILON);
        assert!(
            (hdr_peak_nits(VideoColorInfo {
                hdr_active: true,
                color_space: VideoColorSpace::Rec2020,
                hdr_metadata: Some(HdrMetadata {
                    max_content_light_level: 1_400,
                    max_display_luminance: 1_000,
                    ..HdrMetadata::default()
                }),
            }) - 1_400.0)
                .abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn pq_lookup_covers_the_hdr10_signal_range() {
        assert!(pq_eotf_nits(0.0).abs() < f32::EPSILON);
        assert!((pq_eotf_nits(1.0) - 10_000.0).abs() < 1.0);
    }
}
