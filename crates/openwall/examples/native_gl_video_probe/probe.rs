// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../probe_support/decoder_trace.rs"]
mod decoder_trace;
#[path = "../owned_egl_wayland_probe/egl_window.rs"]
#[allow(dead_code)] // The static quadrant entry point also uses this native owner.
mod egl_window;
#[path = "../probe_support/gl_api.rs"]
mod gl_api;
#[path = "../probe_support/gl_painter.rs"]
mod gl_painter;
mod media;
#[path = "../probe_support/native_measurement.rs"]
mod native_measurement;
#[path = "../probe_support/native_media_context.rs"]
mod native_media_context;
#[path = "../probe_support/publish_control.rs"]
mod publish_control;
#[path = "../probe_support/shared_gl.rs"]
mod shared_gl;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
#[allow(dead_code)] // The static diagnostic uses unlabelled feedback.
mod wayland;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use gstreamer as gst;
#[path = "../probe_support/wayland_poll.rs"]
mod wayland_poll;
use std::{
    cell::Cell,
    collections::VecDeque,
    io::Read,
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_poll::wait;

#[derive(Parser)]
#[command(
    about = "Shared-texture video in a private compositor; never launch on a desktop session"
)]
struct Args {
    video: PathBuf,
    #[arg(long)]
    output: String,
    #[arg(long, conflicts_with="measure_seconds", default_value_t=2, value_parser=clap::value_parser!(u8).range(1..=3))]
    loops: u8,
    #[arg(long, conflicts_with="measure_seconds", default_value_t=30, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
    #[arg(long)]
    intercept_input: bool,
    /// Private harness gate: preroll paused, then publish on one exact command.
    #[arg(long)]
    staged_control: Option<PathBuf>,
    /// Aggregate a sustained private run instead of retaining every frame record.
    #[arg(long, conflicts_with_all=["staged_control", "intercept_input"], value_parser=clap::value_parser!(u64).range(1..=120))]
    measure_seconds: Option<u64>,
    #[arg(long, requires="measure_seconds", default_value_t=5, value_parser=clap::value_parser!(u64).range(0..=30))]
    warmup_seconds: u64,
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
    let staging = args.staged_control.is_some();
    let mut control = args
        .staged_control
        .as_deref()
        .map(publish_control::Control::bind)
        .transpose()?;
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
    let (connection, mut queue, mut state) =
        wayland::connect(&args.output, !staging && args.intercept_input)?;
    if staging {
        state.enable_staging(&queue.handle(), args.intercept_input)?;
    }
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
    // One session owns EGLDisplay/context; individual windows retain that session.
    let native_session = egl_window::Session::new(&connection, args.gl_api)?;
    let window = egl_window::Window::on_session(
        native_session.clone(),
        state.render_surface()?,
        width,
        height,
    )?;
    if staging {
        window.disable_swap_throttle()?;
    }
    let sharing = native_media_context::Sharing::new(native_session.clone(), args.gl_api)?;
    let gl = window.gl()?;
    let painter = gl_painter::Painter::new(gl, args.gl_api)?;
    let mut media = media::Media::new(
        &args.video,
        &sharing.display,
        &sharing.wrapped,
        writer,
        staging,
    )?;
    // Drop order is deliberate: samples/fences before Media's NULL transition,
    // Media before the sharing guard, all before Window/EGL termination.
    let finishes = Cell::new(0);
    let mut leases = VecDeque::new();
    let mut current = None;
    let mut records = Vec::new();
    let mut submitted = 0u64;
    let mut retired = 0;
    let mut peak = 0;
    let mut completed_loops = 0u64;
    let mut first_memory = None;
    let mut source_size = None;
    let mut clicks = 0;
    let mut ready = false;
    let mut feedback_deadline = None;
    let mut published = !staging;
    let mut playback_started = !staging;
    let mut published_size = None;
    let mut staged_ready = false;
    let mut publication_deadline = None;
    let mut measurement = native_measurement::Run::new(args.measure_seconds, args.warmup_seconds);
    let duration = args
        .measure_seconds
        .map(|seconds| seconds + args.warmup_seconds + 15)
        .unwrap_or(args.seconds);
    let deadline = Instant::now() + Duration::from_secs(duration);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
        let requested = control
            .as_mut()
            .map(|control| control.poll())
            .transpose()?
            .unwrap_or(false);
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
        if let Some(until) = publication_deadline {
            ensure!(
                ready || Instant::now() < until,
                "staged video presentation/input timed out"
            );
        }
        if staging && published {
            ensure!(
                state.configured == published_size,
                "this diagnostic only stages video geometry before first publication"
            );
        }
        if staging && published && !playback_started && state.presented > 0 {
            // Start the media clock only after the initial PTS-zero buffer is
            // actually visible, rather than while its parent commit is queued.
            media.resume()?;
            playback_started = true;
            eprintln!("Native GL video first frame presented; playback started");
        }
        if state.witness_buttons >= clicks + 2 {
            clicks = state.witness_buttons;
            eprintln!("Private video click-through confirmed ({clicks} button events)");
        }
        if !ready && state.presented > 0 && clicks >= 2 {
            ready = true;
            eprintln!("Native GL video presented; pointer reached underlying witness");
        }
        if args.measure_seconds.is_some() {
            let (received, replaced) = media.counters()?;
            if measurement.tick(
                ready,
                native_measurement::Counters {
                    submissions: submitted,
                    presentation_feedbacks: u64::from(state.presented),
                    discarded_feedbacks: u64::from(state.discarded),
                    received_samples: received,
                    replaced_samples: replaced,
                    loops: completed_loops,
                },
            )? {
                break;
            }
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "native video probe exceeded its bounded duration"
        );
        ensure!(
            ready || staging && staged_ready || Instant::now() < startup,
            "native video presentation/input readiness timed out"
        );
        // Exercise input between confirmed presentation and the next commit.
        state.inject_if_ready();
        if !state.pending_feedback && leases.len() < 3 {
            if let Some(sample) = media.take()? {
                current = Some(sample);
                state.dirty = true;
            }
            if staging && requested && !published && current.is_some() {
                state.dirty = true;
            }
            if state.dirty
                && let Some(sample) = current.as_ref()
            {
                ensure!(
                    args.measure_seconds.is_some() || records.len() < 240,
                    "native video diagnostic frame limit exceeded"
                );
                let source = shared_gl::source(sample, &sharing.wrapped, gl)?;
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
                let publish_this = published || requested && !window.refresh_required();
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
                let id = submitted + 1;
                if publish_this {
                    state.request_frame_feedback(&queue.handle(), id)?;
                }
                window.present()?;
                let geometry_refresh_required = window.take_refresh();
                if staging {
                    staged_ready |= !geometry_refresh_required;
                    if publish_this {
                        state.publish_staged()?;
                        if !published {
                            published = true;
                            published_size = Some((width, height));
                            publication_deadline = Some(Instant::now() + Duration::from_secs(5));
                            eprintln!(
                                "Native GL video parent published {width}x{height}; playback paused"
                            );
                        }
                    } else {
                        state.flush_staged()?;
                        if !geometry_refresh_required {
                            eprintln!(
                                "Native GL video staged {width}x{height}; paused parent held"
                            );
                        }
                    }
                }
                if args.measure_seconds.is_none() || records.len() < 32 {
                    records.push(
                    serde_json::json!({"id":id,"pts_ns":source.pts_ns,"loop":completed_loops,
                    "logical_size":[width,height],"texture_id":source.texture.0.get(),
                    "geometry_refresh_required":geometry_refresh_required,"published":publish_this}),
                );
                }
                submitted += 1;
                leases.push_back(lease);
                peak = peak.max(leases.len());
                state.dirty = geometry_refresh_required;
                if publish_this {
                    feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
                }
                if staging && !published && geometry_refresh_required {
                    connection.flush()?;
                    continue;
                }
            }
        }
        if published && media.eos && !media.has_sample()? && !state.pending_feedback {
            // Holding a sample across FLUSH can deadlock decoder pool shutdown.
            current.take();
            if leases.is_empty() {
                completed_loops += 1;
                if args.measure_seconds.is_none() && completed_loops == u64::from(args.loops) {
                    break;
                }
                media.rewind()?;
                eprintln!("Native GL video rewound after draining consumer samples");
            }
        }
        let mut next = deadline.min(if ready || staging && staged_ready {
            deadline
        } else {
            startup
        });
        for time in [
            feedback_deadline,
            fence_deadline,
            publication_deadline.filter(|_| !ready),
        ]
        .into_iter()
        .flatten()
        {
            next = next.min(time);
        }
        if !leases.is_empty() {
            next = next.min(Instant::now() + Duration::from_millis(1));
        }
        if let Some(until) = measurement.next_deadline() {
            next = next.min(until);
        }
        if let Some(control) = &control {
            wayland_poll::wait_fds(
                &mut queue,
                &connection,
                &[reader.as_fd(), control.as_fd()],
                next,
            )?;
        } else {
            wait(&mut queue, &connection, &reader, next)?;
        }
    }
    current.take();
    while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
        retired += 1;
    }
    let cancelled = staging && !published && stopped.load(Ordering::Relaxed);
    ensure!(
        ready || cancelled,
        "probe stopped before actual presentation/input validation"
    );
    let (received, replaced) = media.counters()?;
    let counts = window.counts();
    let mut report = serde_json::json!({"schema_version":1,"status":if cancelled {"cancelled_before_publish"} else {"passed"},"scope":"shared_video_into_owned_private_wayland_surface",
        "gl":window.report()?,"gstreamer":gst::version_string().to_string(),"egl_context_creations":1,
        "input_region":"empty","witness_button_events":clicks,"frames":records,"presented_frame_ids":state.presented_frame_ids,
        "presented":state.presented,"discarded":state.discarded,"completed_loops":completed_loops,
        "measurement":measurement.result()?,"submitted_frames":submitted,"retained_frame_records":records.len(),
        "interrupted":stopped.load(Ordering::Relaxed),"source_size":source_size,"input_memory":first_memory,
        "staging":{"enabled":staging,"prepared":staged_ready,"published":published,"playback_started":playback_started},
        "consumer_cpu_buffer_maps":0,"consumer_readbacks":0,"consumer_offscreen_targets":0,
        "decoders":media.decoders()?,"decoder_buffers":media.decoder_buffers()?,
        "delivery":{"received":received,"replaced":replaced,"latest_slot_limit":1},
        "consumer_fences":{"submitted":submitted,"retired":retired,"peak_in_flight":peak,"limit":3,"cleanup_finish_calls":finishes.get()}});
    eprintln!("Native GL video removing private surfaces");
    drop(leases);
    drop(media);
    drop(painter);
    drop(sharing);
    drop(window);
    drop(native_session);
    report["egl_lifecycle"] = counts.snapshot();
    drop(state);
    connection.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
