// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../probe_support/gl_api.rs"]
mod gl_api;
#[path = "../probe_support/gl_painter.rs"]
mod gl_painter;
#[path = "../probe_support/shared_gl.rs"]
mod shared_gl;
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use gl_api::Api;
use glow::HasContext;
use gstreamer::{self as gst, glib, prelude::*};
use gstreamer_gl::{self as gst_gl, prelude::*};
use image::ImageEncoder;
use khronos_egl as egl;
use shared_gl::{Lease, retire_one, source};
use std::{
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    fs::OpenOptions,
    marker::PhantomData,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
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
    #[arg(long, value_enum, default_value_t=Api::Gles)]
    gl_api: Api,
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
    fn new(selected: Api) -> Result<Self> {
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
        native.api.bind_api(selected.egl_api())?;
        let config = native
            .api
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::PBUFFER_BIT,
                    egl::RENDERABLE_TYPE,
                    selected.renderable(),
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
            .context("no EGL configuration for the selected GL API")?;
        native.context = Some(native.api.create_context(
            display,
            config,
            None,
            selected.context_attributes(),
        )?);
        native.surface = Some(native.api.create_pbuffer_surface(
            display,
            config,
            &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE],
        )?);
        native
            .api
            .make_current(display, native.surface, native.surface, native.context)?;
        native.gst_display.filter_gl_api(gst_api(selected));
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
    painter: gl_painter::Painter<'a>,
}

impl Drop for Target<'_> {
    fn drop(&mut self) {
        // SAFETY: the creating EGL context is current and outlives this guard.
        // Source leases separately retain samples until their GPU fences signal.
        // GL retains resources referenced by commands queued before deletion.
        unsafe {
            if let Some(value) = self.framebuffer.take() {
                self.gl.delete_framebuffer(value);
            }
            if let Some(value) = self.texture.take() {
                self.gl.delete_texture(value);
            }
        }
    }
}

impl<'a> Target<'a> {
    fn new(
        gl: &'a glow::Context,
        width: u32,
        height: u32,
        frames: u16,
        selected: Api,
    ) -> Result<Self> {
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
            painter: gl_painter::Painter::new(gl, selected)?,
        };
        // SAFETY: context ownership is thread-local. Dimensions and texture names were
        // validated by the caller. Only application-owned objects are modified/deleted;
        // a separate sampler preserves the producer's texture parameters.
        unsafe {
            let texture = gl.create_texture().map_err(anyhow::Error::msg)?;
            target.texture = Some(texture);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            // Mutable storage also works on core GL 3.3 without ARB_texture_storage.
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                atlas_height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
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
            Ok(target)
        }
    }

    fn draw(&self, source: &shared_gl::Source, index: u16) -> Result<()> {
        ensure!(index < self.frames, "atlas frame index is out of bounds");
        // Frame zero is at the top of the PNG, despite GL's bottom-up coordinates.
        self.painter.draw(
            source,
            self.framebuffer,
            [
                0,
                (self.frames - index - 1) as u32 * self.height,
                self.width,
                self.height,
            ],
        )
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

fn gst_api(selected: Api) -> gst_gl::GLAPI {
    match selected {
        Api::Gles => gst_gl::GLAPI::GLES2,
        Api::Desktop => gst_gl::GLAPI::OPENGL3,
    }
}

type DecoderBuffers = Arc<Mutex<BTreeMap<String, serde_json::Value>>>;

fn watch_decoder_buffers(pipeline: &gst::Element) -> Result<DecoderBuffers> {
    let result = DecoderBuffers::default();
    let recorded = result.clone();
    pipeline
        .clone()
        .downcast::<gst::Bin>()
        .map_err(|_| anyhow::anyhow!("playbin is not a bin"))?
        .connect_deep_element_added(move |_, _, element| {
            let is_video_decoder = element.factory().is_some_and(|factory| {
                factory
                    .metadata("klass")
                    .is_some_and(|klass| klass.contains("Decoder") && klass.contains("Video"))
            });
            if !is_video_decoder {
                return;
            }
            if let Some(pad) = element.static_pad("src") {
                let recorded = recorded.clone();
                let name = element.name().to_string();
                pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                    if let Some(buffer) = info.buffer()
                        && let Ok(mut recorded) = recorded.lock()
                        && !recorded.contains_key(&name)
                        && recorded.len() < 8
                    {
                        let count = buffer.n_memory();
                        let all_gl = count > 0
                            && count <= 16
                            && (0..count).all(|index| {
                                buffer
                                    .peek_memory(index)
                                    .downcast_memory_ref::<gst_gl::GLMemory>()
                                    .is_some()
                            });
                        let allocators: Vec<_> = (0..count.min(16))
                            .map(|index| {
                                buffer
                                    .peek_memory(index)
                                    .allocator()
                                    .map(|allocator| allocator.memory_type().to_string())
                            })
                            .collect();
                        // Introspection only: no source memory is mapped or copied.
                        recorded.insert(
                            name.clone(),
                            serde_json::json!({
                                "memories": count,
                                "all_memory_is_gl": all_gl,
                                "allocator_types": allocators,
                                "caps": pad.current_caps().map(|caps| caps.to_string()),
                            }),
                        );
                        return gst::PadProbeReturn::Remove;
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        });
    Ok(result)
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
    let native = NativeContext::new(args.gl_api)?;
    // SAFETY: this live EGL context is current on this thread. NativeContext owns it
    // until after the wrapped context, pipeline, samples and GL resources are gone.
    let wrapped = WrappedContext(
        unsafe {
            gst_gl::GLContext::new_wrapped(
                &native.gst_display,
                native.context.context("EGL context missing")?.as_ptr() as usize,
                gst_gl::GLPlatform::EGL,
                gst_api(args.gl_api),
            )
        }
        .context("could not wrap the owned EGL context")?,
    );
    wrapped.0.activate(true)?;
    wrapped.0.fill_info()?;
    // SAFETY: functions are loaded for the selected current GL context; native retains EGL.
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
    let decoder_buffers = watch_decoder_buffers(&pipeline)?;
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
            target = Some(Target::new(
                &gl,
                source.width,
                source.height,
                args.frames,
                args.gl_api,
            )?);
        }
        let target = target.as_ref().context("owned target is missing")?;
        let mut lease = Lease {
            gl: &gl,
            sample: Some(sample),
            fence: None,
            pending: false,
            cleanup_finishes: &cleanup_finishes,
            submitted_at: Instant::now(),
        };
        // The guard covers every return after a source-reading command may have been queued.
        lease.pending = true;
        target.draw(&source, index)?;
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
                "output_caps":element.static_pad("src").and_then(|pad| pad.current_caps()).map(|caps| caps.to_string()),
                "first_actual_buffer":decoder_buffers.lock().map_err(|_|anyhow::anyhow!("decoder buffer trace poisoned"))?.get(element.name().as_str()).cloned()}));
        }
    }
    let result = serde_json::json!({
        "schema_version": 1, "status": "passed", "mode": "owned-gl",
        "gl_api":args.gl_api.name(),
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
