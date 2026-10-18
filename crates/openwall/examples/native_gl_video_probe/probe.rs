// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../owned_egl_wayland_probe/egl_window.rs"]
#[allow(dead_code)] // The static quadrant entry point also uses this native owner.
mod egl_window;
#[path = "../probe_support/gl_api.rs"]
mod gl_api;
#[path = "../probe_support/gl_painter.rs"]
mod gl_painter;
mod media;
#[path = "../probe_support/shared_gl.rs"]
mod shared_gl;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
#[allow(dead_code)] // The static diagnostic uses unlabelled feedback.
mod wayland;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use gstreamer as gst;
use gstreamer_gl::{self as gst_gl, prelude::*};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    cell::Cell,
    collections::VecDeque,
    io::Read,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    about = "Shared-texture video in a private compositor; never launch on a desktop session"
)]
struct Args {
    video: PathBuf,
    #[arg(long)]
    output: String,
    #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u8).range(1..=3))]
    loops: u8,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
    #[arg(long)]
    intercept_input: bool,
}

struct Wrapped(gst_gl::GLContext);
impl Drop for Wrapped {
    fn drop(&mut self) {
        let _ = self.0.activate(false);
    }
}

fn wait(
    queue: &mut wayland_client::EventQueue<wayland::State>,
    connection: &wayland_client::Connection,
    reader: &UnixStream,
    deadline: Instant,
) -> Result<()> {
    connection.flush()?;
    let Some(guard) = queue.prepare_read() else {
        return Ok(());
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let timeout = Timespec {
        tv_sec: remaining.as_secs() as i64,
        tv_nsec: remaining.subsec_nanos() as i64,
    };
    let mut fds = [
        PollFd::new(connection, PollFlags::IN),
        PollFd::new(reader, PollFlags::IN),
    ];
    match poll(&mut fds, Some(&timeout)) {
        Ok(_)
            if fds[0]
                .revents()
                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) =>
        {
            bail!("private Wayland compositor disconnected")
        }
        Ok(_) if fds[0].revents().contains(PollFlags::IN) => {
            guard.read()?;
        }
        Ok(_) | Err(rustix::io::Errno::INTR) => drop(guard),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "native GL video probe requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
    gst::init()?;
    ensure!(
        gst::version().0 == 1 && gst::version().1 >= 26,
        "native GL video probe requires GStreamer 1.26 or newer"
    );
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    let writer = Arc::new(Mutex::new(writer));
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    let signal_writer = writer.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
        media::wake(&signal_writer);
    })?;
    let (connection, mut queue, mut state) = wayland::connect(&args.output, args.intercept_input)?;
    let startup = Instant::now() + Duration::from_secs(10);
    while state.configured.is_none() {
        queue.dispatch_pending(&mut state)?;
        ensure!(
            !state.closed && !stopped.load(Ordering::Relaxed),
            "probe stopped during configure"
        );
        ensure!(Instant::now() < startup, "native video configure timed out");
        if state.configured.is_none() {
            wait(&mut queue, &connection, &reader, startup)?;
        }
    }
    let (width, height) = state.configured.context("layer configure missing")?;
    // Single-output diagnostic: this owner terminates EGLDisplay. It must never
    // be copied into independent per-output owners on one shared wl_display.
    let window = egl_window::Window::new(
        &connection,
        state.surface.as_ref().context("surface missing")?,
        width,
        height,
        args.gl_api,
    )?;
    let (native_display, native_context) = window.handles()?;
    // SAFETY: foreign display wrapper does not terminate EGLDisplay. Window owns
    // it and remains alive until after every pipeline, context and sample below.
    let display = unsafe {
        gstreamer_gl_egl::GLDisplayEGL::with_egl_display(native_display.as_ptr() as usize)
    }
    .context("could not borrow owned Wayland EGLDisplay")?;
    let api = match args.gl_api {
        gl_api::Api::Desktop => gst_gl::GLAPI::OPENGL3,
        gl_api::Api::Gles => gst_gl::GLAPI::GLES2,
    };
    display.filter_gl_api(api);
    // SAFETY: Window's context is current on this thread for the whole diagnostic.
    let wrapped = Wrapped(
        unsafe {
            gst_gl::GLContext::new_wrapped(
                &display,
                native_context.as_ptr() as usize,
                gst_gl::GLPlatform::EGL,
                api,
            )
        }
        .context("could not wrap current Wayland GL context")?,
    );
    wrapped.0.activate(true)?;
    wrapped.0.fill_info()?;
    let gl = window.gl()?;
    let painter = gl_painter::Painter::new(gl, args.gl_api)?;
    let mut media = media::Media::new(&args.video, &display, &wrapped.0, writer)?;
    // Drop order is deliberate: samples/fences before Media's NULL transition,
    // Media before Wrapped/GLDisplayEGL, all before Window/EGL termination.
    let finishes = Cell::new(0);
    let mut leases = VecDeque::new();
    let mut current = None;
    let mut records = Vec::new();
    let mut retired = 0;
    let mut peak = 0;
    let mut completed_loops = 0;
    let mut first_memory = None;
    let mut source_size = None;
    let mut clicks = 0;
    let mut ready = false;
    let mut feedback_deadline = None;
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
        media.messages()?;
        ensure!(!state.closed, "selected private output or witness closed");
        ensure!(
            !state.intercepted,
            "test pointer was intercepted by video surface"
        );
        ensure!(state.discarded <= 3, "video commits repeatedly discarded");
        while shared_gl::retire_one(&mut leases, false, &AtomicBool::new(false))? {
            retired += 1;
        }
        let fence_deadline = leases
            .front()
            .map(|lease| lease.submitted_at + Duration::from_secs(5));
        if !state.pending_feedback {
            feedback_deadline = None;
        }
        if let Some(until) = feedback_deadline {
            ensure!(
                Instant::now() < until,
                "native video presentation feedback timed out"
            );
        }
        if let Some(until) = fence_deadline {
            ensure!(
                Instant::now() < until,
                "native video consumer fence timed out"
            );
        }
        if state.witness_buttons >= clicks + 2 {
            clicks = state.witness_buttons;
            eprintln!("Private video click-through confirmed ({clicks} button events)");
        }
        if !ready && state.presented > 0 && clicks >= 2 {
            ready = true;
            eprintln!("Native GL video presented; pointer reached underlying witness");
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "native video probe exceeded its bounded duration"
        );
        ensure!(
            ready || Instant::now() < startup,
            "native video presentation/input readiness timed out"
        );
        // Exercise input between confirmed presentation and the next commit.
        state.inject_if_ready();
        if !state.pending_feedback && leases.len() < 3 {
            if let Some(sample) = media.take()? {
                current = Some(sample);
                state.dirty = true;
            }
            if state.dirty
                && let Some(sample) = current.as_ref()
            {
                ensure!(
                    records.len() < 240,
                    "native video diagnostic frame limit exceeded"
                );
                let source = shared_gl::source(sample, &wrapped.0, gl)?;
                let size = (source.width, source.height);
                ensure!(
                    source_size.is_none_or(|previous| previous == size),
                    "dynamic video dimensions unsupported"
                );
                source_size = Some(size);
                if first_memory.is_none() {
                    first_memory =
                        Some(serde_json::json!({"caps":source.caps,"memory":source.memory}));
                }
                let (width, height) = state.configured.context("layer configure missing")?;
                window.resize(width, height)?;
                let mut lease = shared_gl::Lease {
                    gl,
                    sample: Some(sample.clone()),
                    fence: None,
                    pending: true,
                    cleanup_finishes: &finishes,
                    submitted_at: Instant::now(),
                };
                painter.draw(&source, None, [0, 0, width, height])?;
                lease.seal()?;
                let id = records.len() as u64 + 1;
                state.request_frame_feedback(&queue.handle(), id)?;
                window.present()?;
                records.push(
                    serde_json::json!({"id":id,"pts_ns":source.pts_ns,"loop":completed_loops,
                    "logical_size":[width,height],"texture_id":source.texture.0.get()}),
                );
                leases.push_back(lease);
                peak = peak.max(leases.len());
                state.dirty = false;
                feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
            }
        }
        if media.eos && !media.has_sample()? && !state.pending_feedback {
            // Holding a sample across FLUSH can deadlock decoder pool shutdown.
            current.take();
            if leases.is_empty() {
                completed_loops += 1;
                if completed_loops == args.loops {
                    break;
                }
                media.rewind()?;
                eprintln!("Native GL video rewound after draining consumer samples");
            }
        }
        let mut next = deadline.min(if ready { deadline } else { startup });
        for time in [feedback_deadline, fence_deadline].into_iter().flatten() {
            next = next.min(time);
        }
        if !leases.is_empty() {
            next = next.min(Instant::now() + Duration::from_millis(1));
        }
        wait(&mut queue, &connection, &reader, next)?;
    }
    current.take();
    while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
        retired += 1;
    }
    ensure!(
        ready,
        "probe stopped before actual presentation/input validation"
    );
    let (received, replaced) = media.counters()?;
    let report = serde_json::json!({"schema_version":1,"status":"passed","scope":"shared_video_into_owned_private_wayland_surface",
        "gl":window.report()?,"gstreamer":gst::version_string().to_string(),"egl_context_creations":1,
        "input_region":"empty","witness_button_events":clicks,"frames":records,"presented_frame_ids":state.presented_frame_ids,
        "presented":state.presented,"discarded":state.discarded,"completed_loops":completed_loops,
        "interrupted":stopped.load(Ordering::Relaxed),"source_size":source_size,"input_memory":first_memory,
        "consumer_cpu_buffer_maps":0,"consumer_readbacks":0,"consumer_offscreen_targets":0,
        "decoders":media.decoders()?,
        "delivery":{"received":received,"replaced":replaced,"latest_slot_limit":1},
        "consumer_fences":{"submitted":records.len(),"retired":retired,"peak_in_flight":peak,"limit":3,"cleanup_finish_calls":finishes.get()}});
    eprintln!("Native GL video removing private surfaces");
    drop(leases);
    drop(media);
    drop(painter);
    drop(wrapped);
    drop(display);
    drop(window);
    drop(state);
    connection.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
