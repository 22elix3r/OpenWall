// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use glow::HasContext;
use gstreamer::{self as gst, glib, prelude::*};
use gstreamer_gl::{self as gst_gl, prelude::*};
use image::ImageEncoder;
use khronos_egl as egl;
use std::{fs::OpenOptions, marker::PhantomData, num::NonZeroU32, path::PathBuf, rc::Rc};

#[derive(Parser)]
#[command(
    about = "Render one shared GL video texture into an owned framebuffer; no desktop window"
)]
struct Args {
    video: PathBuf,
    #[arg(long)]
    first_frame: PathBuf,
}

struct NativeContext {
    api: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    gst_display: gstreamer_gl_egl::GLDisplayEGL,
    context: Option<egl::Context>,
    surface: Option<egl::Surface>,
    // EGL current-context ownership stays on the creating thread.
    _thread: PhantomData<Rc<()>>,
}

impl NativeContext {
    fn new() -> Result<Self> {
        // SAFETY: load the system EGL ABI. The instance outlives all function pointers,
        // contexts and surfaces created below; no wallpaper path selects native code.
        let api = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.so.1")
        }?;
        let gst_display = gstreamer_gl_egl::GLDisplayEGL::new_surfaceless()?;
        let handle = gst_display.handle();
        ensure!(handle != 0, "surfaceless EGL display has no native handle");
        // SAFETY: GLDisplayEGL owns this live EGLDisplay and remains in this guard.
        let display = unsafe { egl::Display::from_ptr(handle as *mut _) };
        api.initialize(display)?;
        let mut native = Self {
            api,
            display,
            gst_display,
            context: None,
            surface: None,
            _thread: PhantomData,
        };
        native.api.bind_api(egl::OPENGL_ES_API)?;
        let config = native
            .api
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::PBUFFER_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_ES3_BIT,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::NONE,
                ],
            )?
            .context("no EGL GLES3 pbuffer configuration")?;
        native.context = Some(native.api.create_context(
            display,
            config,
            None,
            &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
        )?);
        native.surface = Some(native.api.create_pbuffer_surface(
            display,
            config,
            &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE],
        )?);
        native
            .api
            .make_current(display, native.surface, native.surface, native.context)?;
        native.gst_display.filter_gl_api(gst_gl::GLAPI::GLES2);
        Ok(native)
    }
}

impl Drop for NativeContext {
    fn drop(&mut self) {
        let _ = self.api.make_current(self.display, None, None, None);
        if let Some(surface) = self.surface.take() {
            let _ = self.api.destroy_surface(self.display, surface);
        }
        if let Some(context) = self.context.take() {
            let _ = self.api.destroy_context(self.display, context);
        }
        // GLDisplayEGL owns display termination, after the native objects are gone.
    }
}

struct WrappedContext(gst_gl::GLContext);
impl Drop for WrappedContext {
    fn drop(&mut self) {
        let _ = self.0.activate(false);
    }
}

struct Playing(gst::Element);
impl Drop for Playing {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

struct Target<'a> {
    gl: &'a glow::Context,
    texture: Option<glow::NativeTexture>,
    framebuffer: Option<glow::NativeFramebuffer>,
    sampler: Option<glow::NativeSampler>,
    program: Option<glow::NativeProgram>,
    vertex_array: Option<glow::NativeVertexArray>,
}

impl Drop for Target<'_> {
    fn drop(&mut self) {
        // SAFETY: the creating EGL context is current and outlives this guard.
        // This one-frame diagnostic deliberately finishes the consumer before the
        // sample can return to GStreamer's buffer pool. A steady-state presenter
        // will need bounded asynchronous consumer-fence retirement instead.
        unsafe {
            self.gl.finish();
            if let Some(value) = self.vertex_array.take() {
                self.gl.delete_vertex_array(value);
            }
            if let Some(value) = self.program.take() {
                self.gl.delete_program(value);
            }
            if let Some(value) = self.sampler.take() {
                self.gl.delete_sampler(value);
            }
            if let Some(value) = self.framebuffer.take() {
                self.gl.delete_framebuffer(value);
            }
            if let Some(value) = self.texture.take() {
                self.gl.delete_texture(value);
            }
        }
    }
}

fn shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::NativeShader> {
    // SAFETY: the native context is current; shader strings are fixed repository code.
    unsafe {
        let shader = gl.create_shader(kind).map_err(anyhow::Error::msg)?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let error = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            bail!("owned shader compile failed: {error}");
        }
        Ok(shader)
    }
}

fn draw(
    gl: &glow::Context,
    source: glow::NativeTexture,
    width: u32,
    height: u32,
    texture_size: (i32, i32),
) -> Result<Vec<u8>> {
    let mut target = Target {
        gl,
        texture: None,
        framebuffer: None,
        sampler: None,
        program: None,
        vertex_array: None,
    };
    // SAFETY: context ownership is thread-local. Dimensions and texture names were
    // validated by the caller. Only application-owned objects are modified/deleted;
    // a separate sampler preserves the producer's texture parameters.
    unsafe {
        let texture = gl.create_texture().map_err(anyhow::Error::msg)?;
        target.texture = Some(texture);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_storage_2d(
            glow::TEXTURE_2D,
            1,
            glow::RGBA8,
            width as i32,
            height as i32,
        );
        let framebuffer = gl.create_framebuffer().map_err(anyhow::Error::msg)?;
        target.framebuffer = Some(framebuffer);
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        ensure!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE,
            "owned RGBA framebuffer is incomplete"
        );
        let sampler = gl.create_sampler().map_err(anyhow::Error::msg)?;
        target.sampler = Some(sampler);
        for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.sampler_parameter_i32(sampler, parameter, glow::LINEAR as i32);
        }
        for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
            gl.sampler_parameter_i32(sampler, parameter, glow::CLAMP_TO_EDGE as i32);
        }
        let program = gl.create_program().map_err(anyhow::Error::msg)?;
        target.program = Some(program);
        let vertex = shader(
            gl,
            glow::VERTEX_SHADER,
            r#"#version 300 es
precision highp float;
uniform vec2 source_scale;
out vec2 uv;
void main() {
    vec2 p = vec2[](vec2(-1., -1.), vec2(3., -1.), vec2(-1., 3.))[gl_VertexID];
    gl_Position = vec4(p, 0., 1.);
    uv = vec2((p.x + 1.) * .5, (1. - p.y) * .5) * source_scale;
}"#,
        )?;
        let fragment = match shader(
            gl,
            glow::FRAGMENT_SHADER,
            r#"#version 300 es
precision highp float;
uniform sampler2D source_texture;
in vec2 uv;
out vec4 color;
void main() { color = texture(source_texture, uv); }
"#,
        ) {
            Ok(value) => value,
            Err(error) => {
                gl.delete_shader(vertex);
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
        ensure!(
            gl.get_program_link_status(program),
            "owned shader link failed: {}",
            gl.get_program_info_log(program)
        );
        let vao = gl.create_vertex_array().map_err(anyhow::Error::msg)?;
        target.vertex_array = Some(vao);
        gl.bind_vertex_array(Some(vao));
        gl.use_program(Some(program));
        gl.uniform_1_i32(
            gl.get_uniform_location(program, "source_texture").as_ref(),
            0,
        );
        gl.uniform_2_f32(
            gl.get_uniform_location(program, "source_scale").as_ref(),
            width as f32 / texture_size.0 as f32,
            height as f32 / texture_size.1 as f32,
        );
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(source));
        gl.bind_sampler(0, Some(sampler));
        gl.viewport(0, 0, width as i32, height as i32);
        gl.draw_arrays(glow::TRIANGLES, 0, 3);
        let mut rgba = vec![0; width as usize * height as usize * 4];
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
        gl.read_pixels(
            0,
            0,
            width as i32,
            height as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut rgba)),
        );
        ensure!(
            gl.get_error() == glow::NO_ERROR,
            "owned GL draw/readback failed"
        );
        // glReadPixels starts at the bottom row. PNG stores rows from the top.
        let stride = width as usize * 4;
        for row in 0..height as usize / 2 {
            let opposite = height as usize - row - 1;
            let (before, after) = rgba.split_at_mut(opposite * stride);
            before[row * stride..(row + 1) * stride].swap_with_slice(&mut after[..stride]);
        }
        Ok(rgba)
    }
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("GST_GL_WINDOW").as_deref() == Ok("surfaceless"),
        "owned GL probe requires GST_GL_WINDOW=surfaceless"
    );
    gst::init()?;
    let (major, minor, _, _) = gst::version();
    ensure!(
        (major, minor) >= (1, 26),
        "owned GL color proof requires GStreamer 1.26 or newer"
    );
    let uri = glib::filename_to_uri(args.video.canonicalize()?, None)?;
    let native = NativeContext::new()?;
    // SAFETY: this live EGL context is current on this thread. NativeContext owns it
    // until after the wrapped context, pipeline, samples and GL resources are gone.
    let wrapped = WrappedContext(
        unsafe {
            gst_gl::GLContext::new_wrapped(
                &native.gst_display,
                native.context.context("EGL context missing")?.as_ptr() as usize,
                gst_gl::GLPlatform::EGL,
                gst_gl::GLAPI::GLES2,
            )
        }
        .context("could not wrap the owned EGL context")?,
    );
    wrapped.0.activate(true)?;
    wrapped.0.fill_info()?;
    // SAFETY: functions are loaded for the current GLES3 context; native retains EGL.
    let gl = unsafe {
        glow::Context::from_loader_function(|name| {
            native
                .api
                .get_proc_address(name)
                .map_or(std::ptr::null(), |function| function as *const _)
        })
    };
    let sink_bin = gst::parse::bin_from_description(
        r#"glupload ! glcolorconvert ! capsfilter caps="video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D" ! appsink name=probe sync=false max-buffers=1 enable-last-sample=false wait-on-eos=false"#,
        true,
    )?;
    let sink = sink_bin
        .by_name("probe")
        .context("appsink missing")?
        .dynamic_cast::<gstreamer_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("not an appsink"))?;
    let pipeline = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("video-sink", sink_bin)
        .build()?;
    pipeline.set_property_from_str("flags", "video+native-video");
    let mut display_context = gst::Context::new("gst.gl.GLDisplay", true);
    display_context
        .get_mut()
        .context("display context not writable")?
        .set_gl_display(&native.gst_display);
    pipeline.set_context(&display_context);
    let mut app_context = gst::Context::new("gst.gl.app_context", true);
    app_context
        .get_mut()
        .context("app context not writable")?
        .structure_mut()
        .set("context", &wrapped.0);
    pipeline.set_context(&app_context);
    let playing = Playing(pipeline);
    playing.0.set_state(gst::State::Playing)?;
    let sample = sink
        .try_pull_sample(gst::ClockTime::from_seconds(15))
        .ok_or_else(|| {
            let message = playing
                .0
                .bus()
                .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]));
            if let Some(message) = message
                && let gst::MessageView::Error(error) = message.view()
            {
                anyhow::anyhow!(
                    "shared GL pipeline failed: {} ({:?})",
                    error.error(),
                    error.debug()
                )
            } else {
                anyhow::anyhow!("no shared GL frame within 15 seconds")
            }
        })?;
    let caps = sample.caps().context("sample has no caps")?;
    let info = gstreamer_video::VideoInfo::from_caps(caps)?;
    let (width, height) = (info.width(), info.height());
    ensure!(
        info.format() == gstreamer_video::VideoFormat::Rgba && info.n_planes() == 1,
        "proof supports one-plane RGBA only"
    );
    ensure!(
        info.interlace_mode() == gstreamer_video::VideoInterlaceMode::Progressive
            && matches!(
                info.multiview_mode(),
                gstreamer_video::VideoMultiviewMode::None
                    | gstreamer_video::VideoMultiviewMode::Mono
            ),
        "proof supports progressive mono frames only"
    );
    ensure!(
        width > 0
            && height > 0
            && width <= 16_384
            && height <= 16_384
            && width as usize * height as usize * 4 <= 256 * 1024 * 1024,
        "frame dimensions exceed proof limits"
    );
    let buffer = sample.buffer().context("sample has no buffer")?;
    ensure!(buffer.n_memory() == 1, "proof requires one GL memory block");
    ensure!(
        buffer
            .meta::<gstreamer_video::VideoAffineTransformationMeta>()
            .is_none(),
        "affine metadata is not supported by this proof"
    );
    ensure!(
        buffer.meta::<gstreamer_video::VideoCropMeta>().is_none(),
        "crop metadata is not supported by this proof"
    );
    let memory = buffer
        .peek_memory(0)
        .downcast_memory_ref::<gst_gl::GLMemory>()
        .context("sample memory is not a GL texture")?;
    ensure!(
        memory.texture_target() == gst_gl::GLTextureTarget::_2d,
        "proof requires a 2D texture"
    );
    ensure!(
        memory.context().can_share(&wrapped.0),
        "producer did not share the owned context"
    );
    let size = (memory.texture_width(), memory.texture_height());
    ensure!(
        size.0 >= width as i32 && size.1 >= height as i32 && size.0 <= 16_384 && size.1 <= 16_384,
        "texture dimensions are invalid"
    );
    let sync = buffer
        .meta::<gst_gl::GLSyncMeta>()
        .context("producer provided no GL synchronization meta")?;
    sync.wait(&wrapped.0);
    let texture =
        glow::NativeTexture(NonZeroU32::new(memory.texture_id()).context("texture ID is zero")?);
    // SAFETY: context is current; check before creating any of our own texture names.
    ensure!(
        unsafe { gl.is_texture(texture) },
        "shared texture is not visible in the owned context"
    );
    let rgba = draw(&gl, texture, width, height, size)?;
    let mut decoders = Vec::new();
    for element in playing
        .0
        .clone()
        .downcast::<gst::Bin>()
        .map_err(|_| anyhow::anyhow!("playbin is not a bin"))?
        .iterate_recurse()
        .into_iter()
        .take(512)
    {
        let element = element?;
        if let Some(factory) = element.factory()
            && factory
                .metadata("klass")
                .is_some_and(|klass| klass.contains("Decoder") && klass.contains("Video"))
        {
            decoders.push(serde_json::json!({"factory":factory.name().to_string(),
                "output_caps":element.static_pad("src").and_then(|pad| pad.current_caps()).map(|caps| caps.to_string())}));
        }
    }
    let result = serde_json::json!({
        "schema_version": 1, "status": "passed", "mode": "owned-gl",
        "scope": "single_shared_texture_draw_into_owned_fbo",
        "gstreamer": gst::version_string().to_string(),
        "consumer_cpu_buffer_maps": 0,
        "snapshot_readbacks": 1,
        "decoders": decoders,
        "input_memory": {"allocator_type":memory.allocator().map(|allocator|allocator.type_().name().to_owned()),
            "texture_width":size.0,"texture_height":size.1,"texture_target":"2D","shared_context":true,
            "producer_sync_meta":true},
        "synchronization": "producer GL sync wait; consumer glFinish before releasing sample",
        "gl": unsafe { serde_json::json!({"vendor": gl.get_parameter_string(glow::VENDOR),
            "renderer": gl.get_parameter_string(glow::RENDERER), "version": gl.get_parameter_string(glow::VERSION)}) },
        "first_frame_capture": {"width":width,"height":height,"bytes":rgba.len(),
            "pts_ns":buffer.pts().map(|pts|pts.nseconds()),"caps":caps.to_string()},
    });
    drop(sample);
    playing.0.set_state(gst::State::Null)?;
    drop(playing);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.first_frame)?;
    image::codecs::png::PngEncoder::new(file).write_image(
        &rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
