// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../nested_egl_publication_probe/control.rs"]
mod control;
#[path = "../probe_support/decoder_trace.rs"]
mod decoder_trace;
#[path = "../owned_egl_wayland_probe/egl_window.rs"]
#[allow(dead_code)] // Windows borrow the one session; static drawing is unused here.
mod egl_window;
#[path = "../probe_support/gl_api.rs"]
mod gl_api;
#[path = "../probe_support/gl_painter.rs"]
mod gl_painter;
#[path = "../native_gl_video_probe/media.rs"]
#[allow(dead_code)] // This bounded replacement diagnostic does not seek EOS.
mod media;
#[path = "../probe_support/native_media_context.rs"]
mod native_media_context;
#[path = "../probe_support/shared_gl.rs"]
mod shared_gl;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
#[allow(dead_code)] // The other diagnostics also use single and multiple output roots.
mod wayland;
#[path = "../probe_support/wayland_poll.rs"]
mod wayland_poll;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use control::Command;
use gstreamer as gst;
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
use wayland_client::Proxy;

#[derive(Parser)]
struct Args {
    previous: PathBuf,
    candidate: PathBuf,
    #[arg(long)]
    output: String,
    #[arg(long)]
    control_socket: PathBuf,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    #[arg(long)]
    intercept_input: bool,
}

struct Source {
    // Struct fields drop in order: retained samples must precede pipeline NULL.
    current: Option<gst::Sample>,
    media: media::Media,
    memory: Option<serde_json::Value>,
    size: Option<(u32, u32)>,
    dirty: bool,
    prepared: bool,
    started: bool,
    loops: u32,
}
impl Source {
    fn new(media: media::Media) -> Self {
        Self {
            current: None,
            media,
            memory: None,
            size: None,
            dirty: false,
            prepared: false,
            started: false,
            loops: 0,
        }
    }
    fn report(&self) -> Result<serde_json::Value> {
        let (received, replaced) = self.media.counters()?;
        Ok(
            serde_json::json!({"input_memory":self.memory,"source_size":self.size,
            "decoders":self.media.decoders()?,"decoder_buffers":self.media.decoder_buffers()?,
            "delivery":{"received":received,"replaced":replaced,"latest_slot_limit":1},
            "prepared":self.prepared,"playback_started":self.started,"completed_loops":self.loops}),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initial,
    Previous,
    Publishing,
    Candidate,
    Restoring,
    Restored,
    Aborted,
    Failed,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "video replacement requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
    gst::init()?;
    ensure!(
        gst::version().0 == 1 && gst::version().1 >= 26,
        "video replacement requires GStreamer 1.26 or newer"
    );
    let control = control::Control::bind(&args.control_socket)?;
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
            !state.closed && !stopped.load(Ordering::Relaxed) && Instant::now() < startup,
            "video replacement configure failed or timed out"
        );
        if state.configured.is_none() {
            wayland_poll::wait(&mut queue, &connection, &reader, startup)?;
        }
    }
    let size = state.configured.context("layer configure missing")?;
    let root_id = state
        .surface
        .as_ref()
        .context("root missing")?
        .id()
        .protocol_id();
    let mut tree = wayland::video_tree::Tree::new(&state, &queue.handle())?;
    let session = egl_window::Session::new(&connection, args.gl_api)?;
    let counts = session.counts();
    let mut windows = [None, None];
    for (index, window) in windows.iter_mut().enumerate() {
        let native =
            egl_window::Window::on_session(session.clone(), tree.surface(index)?, size.0, size.1)?;
        native.disable_swap_throttle()?;
        *window = Some(native);
    }
    let sharing = native_media_context::Sharing::new(session.clone(), args.gl_api)?;
    let gl = session.gl()?;
    let painter = gl_painter::Painter::new(gl, args.gl_api)?;
    let mut sources = [
        Some(Source::new(media::Media::new(
            &args.previous,
            &sharing.display,
            &sharing.wrapped,
            writer.clone(),
            true,
        )?)),
        None,
    ];
    // Leases drop before Source.current, then Media, then painter/sharing/windows/session/tree/root.
    let finishes = Cell::new(0);
    let mut leases = VecDeque::new();
    let mut records = Vec::new();
    let mut retired = 0;
    let mut peak = 0;
    let mut phase = Phase::Initial;
    let mut transition: Option<u64> = None;
    let mut feedback_deadline = None;
    let mut validation_deadline = None;
    let mut prepared_deadline = None;
    let mut attempted = false;
    let mut old_ready = false;
    let mut candidate_ready = false;
    let mut restored_ready = false;
    let mut command_pending = None;
    let mut candidate_report = None;
    let mut candidate_failure = None;
    let mut removal_counts = None;
    let mut rollback_pts = None;
    let mut clock_starts = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
        ensure!(!state.closed, "replacement output or witness closed");
        ensure!(
            !state.intercepted,
            "replacement surface intercepted private input"
        );
        ensure!(state.discarded == 0, "replacement presentation discarded");
        ensure!(
            state.configured == Some(size),
            "this diagnostic keeps published geometry fixed"
        );
        ensure!(
            state
                .surface
                .as_ref()
                .context("root missing")?
                .id()
                .protocol_id()
                == root_id,
            "replacement desktop root changed"
        );
        sources[0]
            .as_mut()
            .context("previous source missing")?
            .media
            .messages()?;
        while shared_gl::retire_one(&mut leases, false, &AtomicBool::new(false))? {
            retired += 1;
        }
        if let Some(lease) = leases.front() {
            ensure!(
                lease.submitted_at.elapsed() < Duration::from_secs(5),
                "replacement consumer fence timed out"
            );
        }
        if let Some(id) = transition
            && state.presented_frame_ids.contains(&id)
        {
            let index = if phase == Phase::Publishing { 1 } else { 0 };
            let source = sources[index]
                .as_mut()
                .context("transition source missing")?;
            source.media.resume()?;
            source.started = true;
            clock_starts.push(serde_json::json!({"source":index,"presented_frame_id":id}));
            phase = match phase {
                Phase::Initial => Phase::Previous,
                Phase::Publishing => Phase::Candidate,
                Phase::Restoring => Phase::Restored,
                _ => anyhow::bail!("unexpected transition presentation"),
            };
            transition = None;
            state.injected = false;
        }
        state.dirty = false; // Repeated same-size configure does not need a priming frame.
        if phase == Phase::Previous && state.witness_buttons >= 2 && !old_ready {
            old_ready = true;
            validation_deadline = None;
            eprintln!("Replacement previous video presented; pointer reached witness");
        }
        if phase == Phase::Candidate && state.witness_buttons >= 4 && !candidate_ready {
            candidate_ready = true;
            validation_deadline = None;
            eprintln!("Replacement candidate video presented; pointer reached witness");
        }
        if phase == Phase::Restored && state.witness_buttons >= 6 && !restored_ready {
            restored_ready = true;
            validation_deadline = None;
            eprintln!("Replacement previous video restored; pointer reached witness");
        }
        if old_ready && !attempted {
            attempted = true;
            prepared_deadline = Some(Instant::now() + Duration::from_secs(8));
            match media::Media::new(
                &args.candidate,
                &sharing.display,
                &sharing.wrapped,
                writer.clone(),
                true,
            ) {
                Ok(media) => sources[1] = Some(Source::new(media)),
                Err(error) => candidate_failure = Some(error.to_string()),
            }
        }
        if let Some(source) = &mut sources[1]
            && phase == Phase::Previous
            && let Err(error) = source.media.messages()
        {
            candidate_failure = Some(error.to_string());
        }
        if let Some(source) = &mut sources[1]
            && phase != Phase::Previous
        {
            source.media.messages()?;
        }
        if let Some(command) = control.next()? {
            ensure!(
                command_pending.is_none(),
                "replacement command already pending"
            );
            match command {
                Command::Publish | Command::Abort => ensure!(
                    phase == Phase::Previous
                        && old_ready
                        && sources[1].as_ref().is_some_and(|source| source.prepared),
                    "invalid replacement command order"
                ),
                Command::Rollback => ensure!(
                    phase == Phase::Candidate && candidate_ready,
                    "invalid replacement command order"
                ),
            }
            command_pending = Some(command);
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "replacement diagnostic duration exceeded"
        );
        ensure!(
            old_ready || Instant::now() < startup,
            "previous video readiness timed out"
        );
        if let Some(until) = validation_deadline {
            ensure!(
                Instant::now() < until,
                "replacement presentation/input timed out"
            );
        }
        if state.pending_feedback {
            ensure!(
                feedback_deadline.is_some_and(|until| Instant::now() < until),
                "replacement feedback timed out"
            );
        } else {
            feedback_deadline = None;
        }
        if let Some(until) = prepared_deadline
            && Instant::now() >= until
            && !sources[1].as_ref().is_some_and(|source| source.prepared)
        {
            candidate_failure = Some("candidate preroll timed out".into());
        }
        if phase == Phase::Previous
            && (candidate_failure.is_some() || matches!(command_pending, Some(Command::Abort)))
        {
            while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
                retired += 1;
            }
            candidate_report = sources[1].as_ref().map(Source::report).transpose()?;
            drop(sources[1].take());
            drop(windows[1].take());
            tree.remove(1)?;
            removal_counts = Some(counts.snapshot());
            prepared_deadline = None;
            phase = if candidate_failure.is_some() {
                Phase::Failed
            } else {
                Phase::Aborted
            };
            command_pending = None;
            eprintln!("Replacement candidate removed; previous video retained");
        }
        if matches!(
            phase,
            Phase::Previous | Phase::Candidate | Phase::Restored | Phase::Aborted | Phase::Failed
        ) {
            state.inject_if_ready();
        }
        // A looping source briefly releases its cached sample before FLUSH.
        // Finish acquiring/rendering the next old sample before parking it for
        // rollback; a queued publish must not suppress that acquisition.
        let switching = command_pending.is_some()
            && sources[0]
                .as_ref()
                .is_some_and(|source| source.current.is_some());
        let target = match command_pending.as_ref().filter(|_| switching) {
            Some(Command::Publish) => 1,
            Some(Command::Rollback) => 0,
            _ => tree.active.unwrap_or(0),
        };
        let mut again = false;
        for index in 0..2 {
            let hidden_candidate = index == 1
                && phase == Phase::Previous
                && !switching
                && sources[1].as_ref().is_some_and(|source| !source.prepared);
            let visible_target =
                index == target && !matches!(phase, Phase::Publishing | Phase::Restoring);
            if !hidden_candidate && !visible_target
                || leases.len() >= 3
                || visible_target && state.pending_feedback
            {
                continue;
            }
            let Some(source) = sources[index].as_mut() else {
                continue;
            };
            if !switching && let Some(sample) = source.media.take()? {
                source.current = Some(sample);
                source.dirty = true;
            }
            if switching && visible_target {
                source.dirty = true;
            }
            if !source.dirty {
                continue;
            }
            let Some(sample) = source.current.as_ref() else {
                continue;
            };
            ensure!(records.len() < 240, "replacement frame limit exceeded");
            let window = windows[index]
                .as_ref()
                .context("native video window missing")?;
            window.resize(size.0, size.1)?;
            let texture = shared_gl::source(sample, &sharing.wrapped, gl)?;
            let dimensions = (texture.width, texture.height);
            ensure!(
                source.size.is_none_or(|previous| previous == dimensions),
                "dynamic video dimensions unsupported"
            );
            source.size = Some(dimensions);
            if source.memory.is_none() {
                source.memory =
                    Some(serde_json::json!({"caps":texture.caps,"memory":texture.memory}));
            }
            let visible = visible_target && !window.refresh_required();
            let kind = if !visible {
                "prime"
            } else if phase == Phase::Initial {
                "initial"
            } else {
                match command_pending.as_ref().filter(|_| switching) {
                    Some(Command::Publish) => "publish",
                    Some(Command::Rollback) => "rollback",
                    _ => "frame",
                }
            };
            let mut lease = shared_gl::Lease {
                gl,
                sample: Some(sample.clone()),
                fence: None,
                pending: true,
                cleanup_finishes: &finishes,
                submitted_at: Instant::now(),
            };
            painter.draw(&texture, None, [0, 0, size.0, size.1])?;
            lease.seal()?;
            let id = records.len() as u64 + 1;
            if visible {
                state.request_surface_feedback(&queue.handle(), tree.surface(index)?, id)?;
            }
            window.present()?;
            let refresh = window.take_refresh();
            ensure!(
                !visible || !refresh,
                "published replacement needs a geometry refresh"
            );
            if visible && kind != "frame" {
                // Only the old source's last rendered sample is retained for rollback.
                // Do not consume its asynchronous pause preroll until after restoration.
                if kind == "publish" {
                    let previous = sources[0].as_ref().context("previous source missing")?;
                    rollback_pts = previous
                        .current
                        .as_ref()
                        .and_then(|sample| sample.buffer())
                        .and_then(|buffer| buffer.pts())
                        .map(|pts| pts.nseconds());
                    previous.media.pause()?;
                    phase = Phase::Publishing;
                } else if kind == "rollback" {
                    sources[1]
                        .as_ref()
                        .context("candidate missing")?
                        .media
                        .pause()?;
                    phase = Phase::Restoring;
                }
                tree.show(index)?;
                transition = Some(id);
                command_pending = None;
                validation_deadline = Some(Instant::now() + Duration::from_secs(5));
            } else {
                tree.flush(index)?;
            }
            records.push(
                serde_json::json!({"id":id,"source":index,"pts_ns":texture.pts_ns,
                "texture_id":texture.texture.0.get(),"visible":visible,"kind":kind,
                "geometry_refresh_required":refresh}),
            );
            leases.push_back(lease);
            peak = peak.max(leases.len());
            let source = sources[index].as_mut().context("drawn source missing")?;
            source.dirty = refresh;
            if !refresh && !source.prepared {
                source.prepared = true;
                if index == 1 {
                    prepared_deadline = None;
                    eprintln!("Replacement candidate cached; previous video continues");
                }
            }
            again |= refresh;
            if visible {
                feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
            }
        }
        if again {
            connection.flush()?;
            continue;
        }
        if leases.is_empty()
            && !state.pending_feedback
            && command_pending.is_none()
            && transition.is_none()
            && let Some(index) = tree.active
            && let Some(source) = sources[index].as_mut()
            && source.media.eos
            && !source.media.has_sample()?
            && !source.dirty
        {
            source.current.take();
            source.loops += 1;
            source.media.rewind()?;
        }
        let mut next = deadline;
        for until in [feedback_deadline, prepared_deadline, validation_deadline]
            .into_iter()
            .flatten()
        {
            next = next.min(until);
        }
        if !old_ready {
            next = next.min(startup);
        }
        if !leases.is_empty() {
            next = next.min(Instant::now() + Duration::from_millis(1));
        }
        wayland_poll::wait_fds(
            &mut queue,
            &connection,
            &[reader.as_fd(), control.as_fd()],
            next,
        )?;
    }
    while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
        retired += 1;
    }
    ensure!(
        old_ready,
        "replacement stopped before previous video presentation/input"
    );
    let complete = restored_ready || matches!(phase, Phase::Aborted | Phase::Failed);
    if candidate_report.is_none() {
        candidate_report = sources[1].as_ref().map(Source::report).transpose()?;
    }
    let mut report = serde_json::json!({"schema_version":1,"scope":"two_video_sources_persistent_root",
        "status":if complete { "passed" } else { "interrupted" },"gl":session.report()?,
        "gstreamer":gst::version_string().to_string(),"root_surface_id":root_id,
        "previous":sources[0].as_ref().context("previous source missing")?.report()?,"candidate":candidate_report,
        "candidate_failure":candidate_failure,"candidate_aborted":phase==Phase::Aborted,
        "candidate_presented":candidate_ready,"previous_restored":restored_ready,"rollback_pts_ns":rollback_pts,
        "removed_candidate_lifecycle":removal_counts,"frames":records,"presented_frame_ids":state.presented_frame_ids,
        "clock_starts":clock_starts,"witness_button_events":state.witness_buttons,
        "consumer_cpu_buffer_maps":0,"consumer_readbacks":0,"consumer_offscreen_targets":0,
        "consumer_fences":{"submitted":records.len(),"retired":retired,"peak_in_flight":peak,
            "limit":3,"cleanup_finish_calls":finishes.get()}});
    eprintln!("Replacement removing private surfaces");
    drop(leases);
    drop(sources);
    drop(painter);
    drop(sharing);
    drop(windows);
    drop(session);
    report["egl_lifecycle"] = counts.snapshot();
    drop(tree);
    drop(state);
    connection.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
