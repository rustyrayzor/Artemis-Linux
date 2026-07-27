use std::sync::Arc;

use eframe::egui;
use eframe::glow::{self, HasContext};

use crate::media::DecodedFrame;

const VERTEX_SHADER: &str = r#"#version 330 core
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
"#;

const FRAGMENT_SHADER: &str = r#"#version 330 core
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
"#;

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
        let expected_bytes = expected_nv12_bytes(decoded.width, decoded.height)?;
        if decoded.nv12.len() != expected_bytes {
            return Err(format!(
                "decoded NV12 buffer has {} bytes; expected {expected_bytes}",
                decoded.nv12.len()
            ));
        }
        let width = i32::try_from(decoded.width)
            .map_err(|_| "decoded video width is too large".to_owned())?;
        let height = i32::try_from(decoded.height)
            .map_err(|_| "decoded video height is too large".to_owned())?;
        let luma_bytes = decoded
            .width
            .checked_mul(decoded.height)
            .ok_or_else(|| "decoded video dimensions overflowed".to_owned())?;
        let chroma_offset = i32::try_from(luma_bytes)
            .map_err(|_| "decoded video buffer is too large for OpenGL".to_owned())?;
        let allocate = self.size != [decoded.width, decoded.height];
        let pixel_buffer = self.pixel_buffers[self.next_pixel_buffer];

        // SAFETY: Every GL object belongs to the current eframe context. The validated NV12 slice
        // is copied into an orphaned pixel buffer before two ordered texture uploads and the
        // shader conversion are queued.
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
            self.gl.buffer_data_u8_slice(
                glow::PIXEL_UNPACK_BUFFER,
                &decoded.nv12,
                glow::STREAM_DRAW,
            );
            upload_plane(&self.gl, self.luma, width, height, glow::RED, 0);
            upload_plane(
                &self.gl,
                self.chroma,
                width / 2,
                height / 2,
                glow::RG,
                chroma_offset,
            );
            self.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            render_nv12(self, width, height);
            let error = self.gl.get_error();
            if error != glow::NO_ERROR {
                return Err(format!("OpenGL video upload failed with error 0x{error:x}"));
            }
        }

        self.next_pixel_buffer = (self.next_pixel_buffer + 1) % self.pixel_buffers.len();
        self.size = [decoded.width, decoded.height];
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
    offset: i32,
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
    use super::expected_nv12_bytes;

    #[test]
    fn nv12_size_is_one_and_a_half_bytes_per_pixel() {
        assert_eq!(expected_nv12_bytes(3_840, 2_160), Ok(12_441_600));
    }

    #[test]
    fn nv12_dimensions_must_be_even() {
        assert!(expected_nv12_bytes(1_919, 1_080).is_err());
        assert!(expected_nv12_bytes(1_920, 1_079).is_err());
    }
}
