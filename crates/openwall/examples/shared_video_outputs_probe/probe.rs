// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../probe_support/decoder_trace.rs"]
mod decoder_trace;
#[path = "../owned_egl_wayland_probe/egl_window.rs"]
#[allow(dead_code)] // Static probes also paint quadrants with this native owner.
mod egl_window;
#[path = "../probe_support/gl_api.rs"]
mod gl_api;
#[path = "../probe_support/gl_painter.rs"]
mod gl_painter;
#[path = "../native_gl_video_probe/media.rs"]
mod media;
#[path = "../probe_support/native_media_context.rs"]
mod native_media_context;
#[path = "../probe_support/shared_gl.rs"]
mod shared_gl;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
#[allow(dead_code)] // Other private probes use the primary staged-child mode.
mod wayland;
#[path = "../probe_support/wayland_poll.rs"]
mod wayland_poll;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use gstreamer as gst;
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
#[command(about = "One decoder and EGL session for two private headless outputs")]
struct Args {
    video: PathBuf,
    #[arg(long)]
    first: String,
    #[arg(long)]
    second: String,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
    #[arg(long, default_value_t=2, value_parser=clap::value_parser!(u8).range(1..=3))]
    loops: u8,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "shared video output probe requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
    ensure!(
        args.first != args.second,
        "shared video requires distinct private outputs"
    );
    gst::init()?;
    ensure!(
        gst::version().0 == 1 && gst::version().1 >= 26,
        "shared video outputs require GStreamer 1.26 or newer"
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
    let (connection, mut queue, mut state) = wayland::connect(&args.first, false)?;
    state.add_secondary(&args.second, &queue.handle())?;
    let startup = Instant::now() + Duration::from_secs(10);
    while state.configured.is_none()
        || state
            .secondary
            .as_ref()
            .is_none_or(|view| view.configured.is_none())
    {
        queue.dispatch_pending(&mut state)?;
        ensure!(
            !state.closed && !state.secondary.as_ref().is_some_and(|view| view.closed),
            "output closed during configure"
        );
        ensure!(
            !stopped.load(Ordering::Relaxed) && Instant::now() < startup,
            "shared video configure timed out or interrupted"
        );
        if state.configured.is_none()
            || state
                .secondary
                .as_ref()
                .is_none_or(|view| view.configured.is_none())
        {
            wayland_poll::wait(&mut queue, &connection, &reader, startup)?;
        }
    }
    let session = egl_window::Session::new(&connection, args.gl_api)?;
    let counts = session.counts();
    let (width, height) = state.configured.context("first configure missing")?;
    let first =
        egl_window::Window::on_session(session.clone(), state.render_surface()?, width, height)?;
    let secondary = state.secondary.as_ref().context("second surface missing")?;
    let (width, height) = secondary.configured.context("second configure missing")?;
    let second =
        egl_window::Window::on_session(session.clone(), &secondary.surface, width, height)?;
    ensure!(
        first.handles()? == second.handles()?,
        "outputs do not share one native EGL display/context"
    );
    let mut windows = [Some(first), Some(second)];
    let sharing = native_media_context::Sharing::new(session.clone(), args.gl_api)?;
    let gl = session.gl()?;
    let painter = gl_painter::Painter::new(gl, args.gl_api)?;
    let mut media = media::Media::new(
        &args.video,
        &sharing.display,
        &sharing.wrapped,
        writer,
        true,
    )?;
    // Samples/fences precede pipeline teardown; the sharing guard and native owner
    // outlive every producer. Native windows precede protocol role destruction.
    let finishes = Cell::new(0);
    let mut leases = VecDeque::new();
    let mut current = None;
    let mut serial = 0u64;
    let mut rendered = [0u64; 2];
    let mut feedback_deadlines = [None; 2];
    let mut records = Vec::new();
    let mut retired = 0;
    let mut peak = 0;
    let mut loops = 0;
    let mut started = false;
    let mut ready = false;
    let mut first_retired = None;
    let mut source_size = None;
    let mut first_memory = None;
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
        media.messages()?;
        ensure!(!state.intercepted, "shared video intercepted private input");
        ensure!(
            state.discarded <= 3,
            "first output repeatedly discarded video"
        );
        let secondary = state.secondary.as_ref().context("second surface missing")?;
        ensure!(
            !secondary.closed && secondary.discarded <= 3,
            "surviving output closed or repeatedly discarded video"
        );
        if !state.pending_feedback {
            feedback_deadlines[0] = None;
        }
        if !secondary.pending_feedback {
            feedback_deadlines[1] = None;
        }
        if !started && state.presented > 0 && secondary.presented > 0 {
            media.resume()?;
            started = true;
        }
        if !ready && started && state.witness_buttons >= 2 {
            ready = true;
            eprintln!("Shared video presented on both private outputs; pointer reached witness");
        }
        if state.closed && windows[0].is_some() {
            ensure!(ready, "first output closed before shared video readiness");
            windows[0].take();
            state.retire_primary();
            feedback_deadlines[0] = None;
            first_retired = Some(counts.snapshot());
            state
                .secondary
                .as_mut()
                .context("second surface missing")?
                .dirty = true;
            eprintln!("Shared video primary retired; decoder retained");
        }
        while shared_gl::retire_one(&mut leases, false, &AtomicBool::new(false))? {
            retired += 1;
        }
        let fence_deadline = leases
            .front()
            .map(|lease| lease.submitted_at + Duration::from_secs(5));
        for until in feedback_deadlines
            .into_iter()
            .chain([fence_deadline])
            .flatten()
        {
            ensure!(
                Instant::now() < until,
                "shared video feedback or consumer fence timed out"
            );
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(
            Instant::now() < deadline && (ready || Instant::now() < startup),
            "shared video bounded duration/readiness exceeded"
        );
        if windows[0].is_some() {
            state.inject_if_ready();
        }
        if let Some(sample) = media.take()? {
            current = Some(sample);
            serial += 1;
        }
        for index in 0..2 {
            let Some(window) = windows[index].as_ref() else {
                continue;
            };
            let Some(sample) = current.as_ref() else {
                continue;
            };
            let (size, dirty, pending) = if index == 0 {
                (state.configured, state.dirty, state.pending_feedback)
            } else {
                let view = state.secondary.as_ref().context("second surface missing")?;
                (view.configured, view.dirty, view.pending_feedback)
            };
            if pending || leases.len() >= 3 || !dirty && rendered[index] == serial {
                continue;
            }
            ensure!(
                records.len() < 240,
                "shared video draw record limit exceeded"
            );
            let (width, height) = size.context("output configure missing")?;
            window.resize(width, height)?;
            let source = shared_gl::source(sample, &sharing.wrapped, gl)?;
            let size = (source.width, source.height);
            ensure!(
                source_size.is_none_or(|previous| previous == size),
                "dynamic video dimensions unsupported"
            );
            source_size = Some(size);
            if first_memory.is_none() {
                first_memory = Some(serde_json::json!({"caps":source.caps,"memory":source.memory}));
            }
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
            if index == 0 {
                state.request_frame_feedback(&queue.handle(), id)?;
            } else {
                state.secondary_frame_feedback(&queue.handle(), id)?;
            }
            window.present()?;
            let refresh = window.take_refresh();
            records.push(serde_json::json!({"id":id,"output":index,"source_serial":serial,"pts_ns":source.pts_ns,
                "loop":loops,"logical_size":[width,height],"texture_id":source.texture.0.get(),"geometry_refresh_required":refresh,
                "after_primary_retired":first_retired.is_some()}));
            leases.push_back(lease);
            peak = peak.max(leases.len());
            rendered[index] = serial;
            if index == 0 {
                state.dirty = refresh;
            } else {
                state
                    .secondary
                    .as_mut()
                    .context("second surface missing")?
                    .dirty = refresh;
            }
            feedback_deadlines[index] = Some(Instant::now() + Duration::from_secs(5));
        }
        let secondary_pending = state
            .secondary
            .as_ref()
            .is_some_and(|view| view.pending_feedback);
        let caught_up = windows.iter().enumerate().all(|(index, window)| {
            window.is_none()
                || rendered[index] == serial
                    && if index == 0 {
                        !state.dirty
                    } else {
                        state.secondary.as_ref().is_some_and(|view| !view.dirty)
                    }
        });
        if media.eos
            && !media.has_sample()?
            && !state.pending_feedback
            && !secondary_pending
            && caught_up
        {
            current.take();
            if leases.is_empty() {
                loops += 1;
                if loops == args.loops {
                    break;
                }
                media.rewind()?;
                eprintln!("Shared video rewound after draining all output leases");
            }
        }
        let mut next = if ready {
            deadline
        } else {
            deadline.min(startup)
        };
        for until in feedback_deadlines
            .into_iter()
            .chain([fence_deadline])
            .flatten()
        {
            next = next.min(until);
        }
        if !leases.is_empty() {
            next = next.min(Instant::now() + Duration::from_millis(1));
        }
        wayland_poll::wait(&mut queue, &connection, &reader, next)?;
    }
    current.take();
    while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
        retired += 1;
    }
    ensure!(ready, "shared video stopped before readiness");
    let (received, replaced) = media.counters()?;
    let secondary = state.secondary.as_ref().context("second surface missing")?;
    let mut report = serde_json::json!({"schema_version":1,"status":"passed","scope":"one_video_decoder_two_private_outputs",
        "gl":session.report()?,"gstreamer":gst::version_string().to_string(),"completed_loops":loops,"interrupted":stopped.load(Ordering::Relaxed),
        "first_retired_lifecycle":first_retired,"first_presented_ids":state.presented_frame_ids,"second_presented_ids":secondary.presented_frame_ids,
        "witness_button_events":state.witness_buttons,"frames":records,"source_size":source_size,"input_memory":first_memory,
        "decoders":media.decoders()?,"decoder_buffers":media.decoder_buffers()?,"delivery":{"received":received,"replaced":replaced,"latest_slot_limit":1},
        "consumer_fences":{"submitted":records.len(),"retired":retired,"peak_in_flight":peak,"limit":3,"cleanup_finish_calls":finishes.get()},
        "consumer_cpu_buffer_maps":0,"consumer_readbacks":0,"consumer_offscreen_targets":0});
    eprintln!("Shared video removing private surfaces");
    drop(leases);
    drop(media);
    drop(painter);
    drop(sharing);
    drop(windows);
    drop(session);
    report["egl_lifecycle"] = counts.snapshot();
    drop(state);
    connection.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
