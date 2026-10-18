// SPDX-License-Identifier: AGPL-3.0-or-later
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use glow::HasContext;
use gstreamer::{self as gst, glib, prelude::*};
use gstreamer_gl::{self as gst_gl, prelude::*};
use image::ImageEncoder;
use khronos_egl as egl;
use std::{
    cell::Cell,
    collections::VecDeque,
    fs::OpenOptions,
    marker::PhantomData,
    num::NonZeroU32,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    about = "Render a bounded shared GL video sequence into an owned framebuffer; no desktop window"
)]
struct Args {
    video: PathBuf,
    #[arg(long)]
    first_frame: PathBuf,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=120))]
    frames: u16,
    /// Optional vertical atlas of all frames, required for a multi-frame proof.
    #[arg(long)]
    atlas: Option<PathBuf>,
    /// Pace appsink delivery on the media clock; this is not presentation feedback.
    #[arg(long)]
    paced: bool,
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
    width: u32,
    height: u32,
    frames: u16,
    texture: Option<glow::NativeTexture>,
    framebuffer: Option<glow::NativeFramebuffer>,
    sampler: Option<glow::NativeSampler>,
    program: Option<glow::NativeProgram>,
    vertex_array: Option<glow::NativeVertexArray>,
}

impl Drop for Target<'_> {
    fn drop(&mut self) {
        // SAFETY: the creating EGL context is current and outlives this guard.
        // Source leases separately retain samples until their GPU fences signal.
        // GL retains resources referenced by commands queued before deletion.
        unsafe {
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

impl<'a> Target<'a> {
    fn new(gl: &'a glow::Context, width: u32, height: u32, frames: u16) -> Result<Self> {
        let atlas_height = height * u32::from(frames);
        ensure!(
            atlas_height <= 16_384
                && width as usize * atlas_height as usize * 4 <= 256 * 1024 * 1024,
            "atlas dimensions exceed proof limits"
        );
        let mut target = Target {
            gl,
            width,
            height,
            frames,
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
                atlas_height as i32,
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
            ensure!(gl.get_error() == glow::NO_ERROR, "owned GL setup failed");
            Ok(target)
        }
    }

    fn draw(
        &self,
        source: glow::NativeTexture,
        texture_size: (i32, i32),
        index: u16,
    ) -> Result<()> {
        ensure!(index < self.frames, "atlas frame index is out of bounds");
        let gl = self.gl;
        // SAFETY: context is current; validated texture is retained by a source lease.
        unsafe {
            let program = self.program.context("owned program missing")?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, self.framebuffer);
            gl.bind_vertex_array(self.vertex_array);
            gl.use_program(Some(program));
            gl.uniform_2_f32(
                gl.get_uniform_location(program, "source_scale").as_ref(),
                self.width as f32 / texture_size.0 as f32,
                self.height as f32 / texture_size.1 as f32,
            );
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(source));
            gl.bind_sampler(0, self.sampler);
            // Frame zero is at the top of the final PNG, despite GL's bottom-up coordinates.
            gl.viewport(
                0,
                ((self.frames - index - 1) as u32 * self.height) as i32,
                self.width as i32,
                self.height as i32,
            );
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            ensure!(gl.get_error() == glow::NO_ERROR, "owned GL draw failed");
        }
        Ok(())
    }

    fn readback(&self, first_only: bool) -> Result<Vec<u8>> {
        let gl = self.gl;
        let (width, height) = (
            self.width,
            if first_only {
                self.height
            } else {
                self.height * u32::from(self.frames)
            },
        );
        let y = if first_only {
            (self.frames - 1) as u32 * self.height
        } else {
            0
        };
        // SAFETY: framebuffer is owned, current and bounded; destination fits the requested rectangle.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, self.framebuffer);
            let mut rgba = vec![0; width as usize * height as usize * 4];
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.read_pixels(
                0,
                y as i32,
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
}

// A sample must remain leased until the consumer's read commands have completed.
// The guard also covers draw/fence failures before it can enter the bounded queue.
struct Lease<'a> {
    gl: &'a glow::Context,
    sample: Option<gst::Sample>,
    fence: Option<glow::NativeFence>,
    pending: bool,
    cleanup_finishes: &'a Cell<u32>,
}

impl Lease<'_> {
    fn seal(&mut self) -> Result<()> {
        // SAFETY: the owned context is current and the lease retains the source.
        unsafe {
            let fence = self
                .gl
                .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
                .map_err(anyhow::Error::msg)?;
            ensure!(!fence.0.is_null(), "consumer fence creation returned null");
            self.fence = Some(fence);
            self.gl.flush();
            ensure!(
                self.gl.get_error() == glow::NO_ERROR,
                "consumer fence submission failed"
            );
        }
        Ok(())
    }

    fn poll(&mut self, wait: bool) -> Result<bool> {
        let fence = self.fence.context("consumer fence is missing")?;
        // SAFETY: this fence belongs to the current context, and has not been deleted.
        let result = unsafe {
            self.gl
                .client_wait_sync(fence, 0, if wait { 1_000_000 } else { 0 })
        };
        match result {
            glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => {
                // SAFETY: the completed fence is deleted once before releasing the sample.
                unsafe { self.gl.delete_sync(fence) };
                self.fence = None;
                self.pending = false;
                self.sample.take();
                Ok(true)
            }
            glow::TIMEOUT_EXPIRED => Ok(false),
            _ => bail!("consumer fence wait failed: {result:#x}"),
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        // SAFETY: the creating context outlives every lease and is still current.
        // Only an incomplete/error path finishes globally. Successful retirement
        // deletes the fence and releases the sample without glFinish.
        unsafe {
            if self.pending {
                self.gl.finish();
                self.cleanup_finishes.set(self.cleanup_finishes.get() + 1);
            }
            if let Some(fence) = self.fence.take() {
                self.gl.delete_sync(fence);
            }
        }
    }
}

fn retire_one(
    leases: &mut VecDeque<Lease<'_>>,
    wait: bool,
    interrupted: &AtomicBool,
) -> Result<bool> {
    let Some(first) = leases.front_mut() else {
        return Ok(false);
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "owned GL probe interrupted"
        );
        if first.poll(wait)? {
            leases.pop_front();
            return Ok(true);
        }
        if !wait {
            return Ok(false);
        }
        ensure!(
            Instant::now() < deadline,
            "consumer fence did not signal within five seconds"
        );
    }
}

fn pull_sample(
    sink: &gstreamer_app::AppSink,
    bus: &gst::Bus,
    interrupted: &AtomicBool,
) -> Result<gst::Sample> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "owned GL probe interrupted"
        );
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            return Ok(sample);
        }
        if let Some(message) = bus.pop_filtered(&[gst::MessageType::Error])
            && let gst::MessageView::Error(error) = message.view()
        {
            bail!(
                "shared GL pipeline failed: {} ({:?})",
                error.error(),
                error.debug()
            );
        }
        ensure!(
            !sink.is_eos(),
            "video ended before the requested frame count"
        );
        ensure!(
            Instant::now() < deadline,
            "no shared GL frame within 15 seconds"
        );
    }
}

struct Source {
    width: u32,
    height: u32,
    texture: glow::NativeTexture,
    texture_size: (i32, i32),
    pts_ns: Option<u64>,
    caps: String,
    memory: serde_json::Value,
}

fn source(sample: &gst::Sample, wrapped: &gst_gl::GLContext, gl: &glow::Context) -> Result<Source> {
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
        memory.context().can_share(wrapped),
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
    sync.wait(wrapped);
    let texture =
        glow::NativeTexture(NonZeroU32::new(memory.texture_id()).context("texture ID is zero")?);
    // SAFETY: context is current; check before creating any of our own texture names.
    ensure!(
        unsafe { gl.is_texture(texture) },
        "shared texture is not visible in the owned context"
    );
    Ok(Source {
        width,
        height,
        texture,
        texture_size: size,
        pts_ns: buffer.pts().map(|pts| pts.nseconds()),
        caps: caps.to_string(),
        memory: serde_json::json!({"allocator_type":memory.allocator().map(|allocator|allocator.type_().name().to_owned()),
            "texture_width":size.0,"texture_height":size.1,"texture_target":"2D","shared_context":true,
            "producer_sync_meta":true}),
    })
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.frames == 1 || args.atlas.is_some(),
        "--frames greater than one requires --atlas"
    );
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = interrupted.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
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
    sink.set_property("sync", args.paced);
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
    let bus = playing.0.bus().context("pipeline has no bus")?;
    let cleanup_finishes = Cell::new(0);
    let mut target: Option<Target<'_>> = None;
    // At most three consumer samples may remain in flight; appsink is separately bounded to one.
    let mut leases = VecDeque::new();
    let mut retired = 0;
    let mut peak = 0;
    let mut frame_records = Vec::new();
    let mut first_capture = None;
    let mut first_memory = None;
    let mut rgba = Vec::new();
    for index in 0..args.frames {
        while retire_one(&mut leases, false, &interrupted)? {
            retired += 1;
        }
        if leases.len() == 3 && retire_one(&mut leases, true, &interrupted)? {
            retired += 1;
        }
        let sample = pull_sample(&sink, &bus, &interrupted)?;
        let source = source(&sample, &wrapped.0, &gl)?;
        if let Some(target) = &target {
            ensure!(
                (source.width, source.height) == (target.width, target.height),
                "dynamic frame dimensions are not supported by this proof"
            );
        } else {
            target = Some(Target::new(&gl, source.width, source.height, args.frames)?);
        }
        let target = target.as_ref().context("owned target is missing")?;
        let mut lease = Lease {
            gl: &gl,
            sample: Some(sample),
            fence: None,
            pending: false,
            cleanup_finishes: &cleanup_finishes,
        };
        // The guard covers every return after a source-reading command may have been queued.
        lease.pending = true;
        target.draw(source.texture, source.texture_size, index)?;
        lease.seal()?;
        if index == 0 {
            rgba = target.readback(true)?;
            first_capture = Some(
                serde_json::json!({"width":source.width,"height":source.height,"bytes":rgba.len(),
                "pts_ns":source.pts_ns,"caps":source.caps}),
            );
            first_memory = Some(source.memory);
            eprintln!("Owned GL probe drew its first shared texture");
        }
        frame_records
            .push(serde_json::json!({"pts_ns":source.pts_ns,"texture_id":source.texture.0.get()}));
        leases.push_back(lease);
        peak = peak.max(leases.len());
    }
    while retire_one(&mut leases, true, &interrupted)? {
        retired += 1;
    }
    let target = target.context("no frames were rendered")?;
    let atlas = args
        .atlas
        .as_ref()
        .map(|_| target.readback(false))
        .transpose()?;
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
        "scope": if args.frames == 1 {"single_shared_texture_draw_into_owned_fbo"} else {"bounded_shared_texture_sequence_into_owned_atlas"},
        "gstreamer": gst::version_string().to_string(),
        "consumer_cpu_buffer_maps": 0,
        "snapshot_readbacks": 1 + u32::from(atlas.is_some()),
        "decoders": decoders,
        "input_memory": first_memory,
        "synchronization": "producer GL sync wait; consumer fences retain samples until signaled",
        "consumer_fences": {"submitted":args.frames,"retired":retired,"peak_in_flight":peak,"limit":3,
            "cleanup_finish_calls":cleanup_finishes.get()},
        "frames":frame_records,"paced":args.paced,
        "gl": unsafe { serde_json::json!({"vendor": gl.get_parameter_string(glow::VENDOR),
            "renderer": gl.get_parameter_string(glow::RENDERER), "version": gl.get_parameter_string(glow::VERSION)}) },
        "first_frame_capture": first_capture,
        "atlas_capture":atlas.as_ref().map(|bytes|serde_json::json!({"width":target.width,
            "height":target.height * u32::from(args.frames),"bytes":bytes.len()})),
    });
    playing.0.set_state(gst::State::Null)?;
    drop(playing);
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "owned GL probe interrupted"
    );
    write_png(&args.first_frame, &rgba, target.width, target.height)?;
    if let (Some(path), Some(bytes)) = (args.atlas, atlas) {
        write_png(
            &path,
            &bytes,
            target.width,
            target.height * u32::from(args.frames),
        )?;
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn write_png(path: &std::path::Path, rgba: &[u8], width: u32, height: u32) -> Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    image::codecs::png::PngEncoder::new(file).write_image(
        rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(())
}
