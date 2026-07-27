#![allow(unsafe_code)]

use std::sync::Arc;

use eframe::egui;
use eframe::glow::{self, HasContext};
#[cfg(target_os = "linux")]
use gstreamer as gst;
#[cfg(target_os = "linux")]
use gstreamer_gl as gst_gl;
#[cfg(target_os = "linux")]
use gstreamer_gl::GLVideoFrameExt;
#[cfg(target_os = "linux")]
use gstreamer_video as gst_video;
#[cfg(target_os = "linux")]
use gstreamer_video::VideoFrameExt;

use crate::media::DecodedFrame;

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

pub struct StreamTexture {
    gl: Arc<glow::Context>,
    id: egui::TextureId,
    output: glow::Texture,
    luma: glow::Texture,
    chroma: glow::Texture,
    pixel_buffers: [glow::Buffer; 2],
    next_pixel_buffer: usize,
    program: glow::Program,
    luma_uniform: glow::UniformLocation,
    chroma_uniform: glow::UniformLocation,
    vertex_array: glow::VertexArray,
    framebuffer: glow::Framebuffer,
    source_framebuffer: glow::Framebuffer,
    size: [usize; 2],
}

impl StreamTexture {
    pub fn new(frame: &mut eframe::Frame, decoded: &DecodedFrame) -> Result<Self, String> {
        let gl = frame
            .gl()
            .cloned()
            .ok_or_else(|| "the OpenGL renderer is unavailable".to_owned())?;
        // SAFETY: eframe's OpenGL context is current on the UI thread during `App::update`.
        let resources = unsafe { create_resources(&gl)? };
        let id = frame.register_native_glow_texture(resources.output);
        let mut texture = Self {
            gl,
            id,
            output: resources.output,
            luma: resources.luma,
            chroma: resources.chroma,
            pixel_buffers: resources.pixel_buffers,
            next_pixel_buffer: 0,
            program: resources.program,
            luma_uniform: resources.luma_uniform,
            chroma_uniform: resources.chroma_uniform,
            vertex_array: resources.vertex_array,
            framebuffer: resources.framebuffer,
            source_framebuffer: resources.source_framebuffer,
            size: [0, 0],
        };
        texture.upload(decoded)?;
        Ok(texture)
    }

    pub fn id(&self) -> egui::TextureId {
        self.id
    }

    pub fn size_vec2(&self) -> egui::Vec2 {
        let width = u16::try_from(self.size[0]).unwrap_or(u16::MAX);
        let height = u16::try_from(self.size[1]).unwrap_or(u16::MAX);
        egui::vec2(f32::from(width), f32::from(height))
    }

    pub fn upload(&mut self, decoded: &DecodedFrame) -> Result<(), String> {
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
            if info.format() == gst_video::VideoFormat::Rgba {
                return self.upload_gl_frame(decoded, &info);
            }
            if info.format() != gst_video::VideoFormat::Nv12 {
                return Err(format!(
                    "decoded video format is {:?}; expected NV12 or RGBA",
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
    fn upload_gl_frame(
        &mut self,
        decoded: &DecodedFrame,
        info: &gst_video::VideoInfo,
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

        // SAFETY: The producer texture belongs to a context that shares objects with eframe's
        // current context. GLSyncMeta has completed the cross-context wait before the texture is
        // attached, and both framebuffers are valid for this context.
        unsafe {
            if self.size != [decoded.width, decoded.height] {
                allocate_textures(
                    &self.gl,
                    self.output,
                    self.luma,
                    self.chroma,
                    self.framebuffer,
                    width,
                    height,
                )?;
            }
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
        self.size = [decoded.width, decoded.height];
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
        let allocate = self.size != [decoded_width, decoded_height];
        let pixel_buffer = self.pixel_buffers[self.next_pixel_buffer];

        // SAFETY: Every GL object belongs to the current eframe context. The validated decoded
        // planes are copied directly into an orphaned pixel buffer. UNPACK_ROW_LENGTH preserves
        // hardware-decoder stride without repacking the full 4K frame in the GStreamer callback.
        unsafe {
            if allocate {
                allocate_textures(
                    &self.gl,
                    self.output,
                    self.luma,
                    self.chroma,
                    self.framebuffer,
                    width,
                    height,
                )?;
            }
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
        self.size = [decoded_width, decoded_height];
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
            self.gl.delete_vertex_array(self.vertex_array);
            self.gl.delete_framebuffer(self.framebuffer);
            self.gl.delete_framebuffer(self.source_framebuffer);
        }
    }
}

struct Resources {
    output: glow::Texture,
    luma: glow::Texture,
    chroma: glow::Texture,
    pixel_buffers: [glow::Buffer; 2],
    program: glow::Program,
    luma_uniform: glow::UniformLocation,
    chroma_uniform: glow::UniformLocation,
    vertex_array: glow::VertexArray,
    framebuffer: glow::Framebuffer,
    source_framebuffer: glow::Framebuffer,
}

#[allow(unsafe_code)]
unsafe fn create_resources(gl: &glow::Context) -> Result<Resources, String> {
    unsafe {
        let output = gl.create_texture()?;
        let luma = match gl.create_texture() {
            Ok(texture) => texture,
            Err(error) => {
                gl.delete_texture(output);
                return Err(error);
            }
        };
        let chroma = match gl.create_texture() {
            Ok(texture) => texture,
            Err(error) => {
                gl.delete_texture(luma);
                gl.delete_texture(output);
                return Err(error);
            }
        };
        let first_buffer = match gl.create_buffer() {
            Ok(buffer) => buffer,
            Err(error) => {
                delete_textures(gl, [output, luma, chroma]);
                return Err(error);
            }
        };
        let second_buffer = match gl.create_buffer() {
            Ok(buffer) => buffer,
            Err(error) => {
                gl.delete_buffer(first_buffer);
                delete_textures(gl, [output, luma, chroma]);
                return Err(error);
            }
        };
        let program = match compile_program(gl) {
            Ok(program) => program,
            Err(error) => {
                gl.delete_buffer(first_buffer);
                gl.delete_buffer(second_buffer);
                delete_textures(gl, [output, luma, chroma]);
                return Err(error);
            }
        };
        let Some(luma_uniform) = gl.get_uniform_location(program, "luma_texture") else {
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                [output, luma, chroma],
            );
            return Err("NV12 shader is missing its luma sampler".to_owned());
        };
        let Some(chroma_uniform) = gl.get_uniform_location(program, "chroma_texture") else {
            delete_program_resources(
                gl,
                program,
                [first_buffer, second_buffer],
                [output, luma, chroma],
            );
            return Err("NV12 shader is missing its chroma sampler".to_owned());
        };
        let vertex_array = match gl.create_vertex_array() {
            Ok(array) => array,
            Err(error) => {
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    [output, luma, chroma],
                );
                return Err(error);
            }
        };
        let framebuffer = match gl.create_framebuffer() {
            Ok(framebuffer) => framebuffer,
            Err(error) => {
                gl.delete_vertex_array(vertex_array);
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    [output, luma, chroma],
                );
                return Err(error);
            }
        };
        let source_framebuffer = match gl.create_framebuffer() {
            Ok(framebuffer) => framebuffer,
            Err(error) => {
                gl.delete_framebuffer(framebuffer);
                gl.delete_vertex_array(vertex_array);
                delete_program_resources(
                    gl,
                    program,
                    [first_buffer, second_buffer],
                    [output, luma, chroma],
                );
                return Err(error);
            }
        };
        Ok(Resources {
            output,
            luma,
            chroma,
            pixel_buffers: [first_buffer, second_buffer],
            program,
            luma_uniform,
            chroma_uniform,
            vertex_array,
            framebuffer,
            source_framebuffer,
        })
    }
}

#[allow(unsafe_code)]
unsafe fn compile_program(gl: &glow::Context) -> Result<glow::Program, String> {
    unsafe {
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX_SHADER)?;
        let fragment = match compile_shader(gl, glow::FRAGMENT_SHADER, FRAGMENT_SHADER) {
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

#[allow(unsafe_code)]
#[allow(clippy::too_many_arguments)]
unsafe fn allocate_textures(
    gl: &glow::Context,
    output: glow::Texture,
    luma: glow::Texture,
    chroma: glow::Texture,
    framebuffer: glow::Framebuffer,
    width: i32,
    height: i32,
) -> Result<(), String> {
    unsafe {
        allocate_texture(gl, luma, glow::R8, width, height, glow::RED)?;
        allocate_texture(gl, chroma, glow::RG8, width / 2, height / 2, glow::RG)?;
        allocate_texture(gl, output, glow::SRGB8_ALPHA8, width, height, glow::RGBA)?;
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
        if status != glow::FRAMEBUFFER_COMPLETE {
            return Err(format!(
                "OpenGL video framebuffer is incomplete: 0x{status:x}"
            ));
        }
        Ok(())
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
            glow::UNSIGNED_BYTE,
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
unsafe fn delete_textures(gl: &glow::Context, textures: [glow::Texture; 3]) {
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
    textures: [glow::Texture; 3],
) {
    unsafe {
        gl.delete_program(program);
        for buffer in buffers {
            gl.delete_buffer(buffer);
        }
        delete_textures(gl, textures);
    }
}

#[cfg(test)]
mod tests {
    use super::{Nv12UploadLayout, expected_nv12_bytes, nv12_upload_layout};

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
}
